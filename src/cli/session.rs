//! Execute shell and desktop-launch invocations through application services.

use crate::application::machine_lifecycle::{MachineLifecycleService, MachineReadyError};
use crate::application::runtime::RuntimeCatalog;
use crate::application::sessions::{
    InteractiveShellEnvironment, SessionError, SessionService, ShellAttemptError, ShellLaunchError,
    ShellLaunchRequest, ShellOpenError, WaylandFallbackCause, WaylandShellRequest,
};
use crate::application::x11::{
    X11AccessError, X11AuthorizationDisposition, X11SessionPreparation, X11SessionPreparationPort,
    X11SessionSelection,
};

use super::{ShellCommand, ShellIoMode, ShellX11Selection};

const WAYLAND_FALLBACK_NOTICE: &str = "🪐 Continuing without Wayland...";

#[derive(Debug, thiserror::Error)]
pub(crate) enum ShellCommandError {
    #[error("cannot prepare machine for guest launch: {0}")]
    Machine(#[from] MachineReadyError),
    #[error("{error}")]
    Open {
        #[source]
        error: Box<ShellLaunchError>,
        uses_wayland: bool,
        allow_wayland_fallback: bool,
    },
    #[error("{context}: {error}")]
    Terminal {
        context: &'static str,
        #[source]
        error: SessionError,
    },
}

pub(crate) async fn run_launch(
    command: ShellCommand,
    sessions: &SessionService,
    x11_access: &dyn X11SessionPreparationPort,
    lifecycle: &std::sync::Arc<MachineLifecycleService>,
    runtime: &RuntimeCatalog,
) -> Result<i32, ShellCommandError> {
    lifecycle
        .ensure_running(runtime, command.target().machine())
        .await?;
    run_shell(command, sessions, x11_access).await
}

pub(crate) async fn run_shell(
    command: ShellCommand,
    sessions: &SessionService,
    x11_access: &dyn X11SessionPreparationPort,
) -> Result<i32, ShellCommandError> {
    let io_mode = command.io_mode();
    let requested_command = command.command().cloned();
    let allow_wayland_fallback = command.allows_wayland_fallback();
    let quiet = command.is_quiet();
    let ShellCommand {
        target,
        wayland,
        x11,
        ..
    } = command;
    let uses_wayland = !matches!(&wayland, WaylandShellRequest::Disabled);
    let size = match io_mode {
        ShellIoMode::Interactive => {
            match crate::adapters::session::terminal_io::inherited_terminal_size() {
                Ok(size) => size,
                Err(error) => {
                    return Err(ShellCommandError::Terminal {
                        context: "cannot attach the interactive terminal",
                        error,
                    })
                }
            }
        }
        ShellIoMode::Launcher => crate::adapters::session::terminal_io::launcher_terminal_size(),
    };
    let x11 = match x11 {
        ShellX11Selection::Disabled => None,
        ShellX11Selection::Current => Some(X11SessionSelection::Current),
        ShellX11Selection::Display(display) => Some(X11SessionSelection::Display(display)),
    };
    let mut request = ShellLaunchRequest::new(
        target,
        wayland,
        InteractiveShellEnvironment::capture(),
        size,
    )
    .with_wayland_fallback(allow_wayland_fallback);
    if let Some(command) = requested_command {
        request = request.with_command(command);
    }
    if let Some(selection) = x11 {
        request = request.with_x11(selection);
    }
    let handle = match sessions.launch_shell(request, Some(x11_access)).await {
        Ok(opened) => {
            if let Some(preparation) = opened.x11.as_ref() {
                report_x11_preparation(preparation);
            }
            if let Some(notice) = wayland_fallback_notice(quiet, opened.wayland_fallback.is_some())
            {
                eprintln!("{notice}");
            }
            opened.handle
        }
        Err(error) => {
            return Err(ShellCommandError::Open {
                error: Box::new(error),
                uses_wayland,
                allow_wayland_fallback,
            })
        }
    };
    let result = match io_mode {
        ShellIoMode::Interactive => {
            crate::adapters::session::terminal_io::run_inherited_terminal(handle, !quiet).await
        }
        ShellIoMode::Launcher => {
            crate::adapters::session::terminal_io::run_launcher_terminal(handle).await
        }
    };
    result.map_err(|error| ShellCommandError::Terminal {
        context: match io_mode {
            ShellIoMode::Interactive => "interactive shell failed",
            ShellIoMode::Launcher => "guest launch failed",
        },
        error,
    })
}

pub(crate) fn report_command_error(error: &ShellCommandError) {
    match error {
        ShellCommandError::Machine(error) => {
            eprintln!("lasper: cannot prepare machine for guest launch: {error}");
        }
        ShellCommandError::Terminal { context, error } => report_shell_error(context, error),
        ShellCommandError::Open {
            error,
            uses_wayland,
            allow_wayland_fallback,
        } => match error.as_ref() {
            ShellLaunchError::X11(error) => report_x11_error(error),
            ShellLaunchError::Open { error, x11 } => {
                if let Some(preparation) = x11 {
                    report_x11_preparation(preparation);
                }
                match error {
                    ShellAttemptError::Initial(error) => {
                        let context = if matches!(error, ShellOpenError::WaylandSelection(_)) {
                            "Wayland socket selection failed"
                        } else {
                            "failed to open selected-user shell"
                        };
                        report_shell_error(context, error.session_error());
                        if *uses_wayland && !*allow_wayland_fallback {
                            eprintln!(
                                "lasper: hint: choose another display with --wayland=DISPLAY, or use \
                                 --no-wayland for a terminal-only session"
                            );
                        }
                    }
                    ShellAttemptError::Fallback { cause, error } => {
                        report_wayland_fallback_failure(cause, error);
                    }
                }
            }
        },
    }
}

fn report_x11_error(error: &X11AccessError) {
    eprintln!("lasper: failed to prepare Host X11 session: {error}");
    if let X11AccessError::Projection(error) = error {
        if let Some(hint) = error.hint() {
            eprintln!("lasper: hint: {hint}");
        }
    }
}

fn report_x11_preparation(preparation: &X11SessionPreparation) {
    let context = preparation.context();
    let display = context.display();
    let projection = preparation.check().projection();
    match preparation.disposition() {
        X11AuthorizationDisposition::Added { .. } => eprintln!(
            "🪐 Authorized Host X11 :{display} for mapped uid #{}.",
            projection.identity().host_uid()
        ),
        X11AuthorizationDisposition::AccessControlDisabled => eprintln!(
            "lasper: warning: X11 access control is disabled on :{display}; no per-user ACL entry was added"
        ),
        X11AuthorizationDisposition::PreExisting => {}
        X11AuthorizationDisposition::ReusedManaged { .. } => {}
    }
    for diagnostic in preparation.check().grant_assessment().diagnostics() {
        eprintln!("lasper: warning: {diagnostic}");
    }
    if !projection.filesystem_access().client_writable() {
        eprintln!(
            "lasper: warning: {} is not writable by the selected guest user; compatible clients may still use the Linux abstract X11 transport",
            projection.guest_client_path().display()
        );
    }
}

fn report_shell_error(context: &str, error: &SessionError) {
    eprintln!("lasper: {context}: {error}");
    if let Some(hint) = error.hint() {
        eprintln!("lasper: hint: {hint}");
    }
}

fn report_wayland_fallback_failure(cause: &WaylandFallbackCause, error: &ShellOpenError) {
    report_shell_error(cause.context(), cause.error());
    report_shell_error(
        "failed to open selected-user shell without Wayland",
        error.session_error(),
    );
}

fn wayland_fallback_notice(quiet: bool, fallback_used: bool) -> Option<&'static str> {
    (!quiet && fallback_used).then_some(WAYLAND_FALLBACK_NOTICE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{GuestCommand, ShellTarget};
    use crate::cli::parse::{parse, Invocation};
    use crate::cli::CliDispatch;
    use crate::domain::machine::MachineName;

    fn arguments(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn parse_launcher(args: &[String]) -> Result<ShellCommand, String> {
        let args = std::iter::once("launch".to_owned())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>();
        match parse(&args)? {
            Invocation::Run(CliDispatch::Shell(command)) => Ok(*command),
            other => panic!("expected launch invocation, got {other:?}"),
        }
    }

    #[test]
    fn quiet_suppresses_only_the_successful_wayland_fallback_notice() {
        assert_eq!(
            wayland_fallback_notice(false, true),
            Some(WAYLAND_FALLBACK_NOTICE)
        );
        assert_eq!(wayland_fallback_notice(true, true), None);
        assert_eq!(wayland_fallback_notice(false, false), None);
    }

    #[derive(Default)]
    struct LauncherSession {
        commands: parking_lot::Mutex<Vec<GuestCommand>>,
    }

    #[async_trait::async_trait]
    impl crate::application::sessions::SessionPort for LauncherSession {
        async fn discover_host_wayland_sockets(
            &self,
        ) -> Vec<crate::domain::wayland::HostWaylandSocket> {
            panic!("launch must not discover displays before machine readiness")
        }

        async fn automatic_wayland(
            &self,
            _machine: &MachineName,
        ) -> Result<Option<crate::domain::wayland::HostWaylandSocket>, SessionError> {
            panic!("launch must not select Wayland before machine readiness")
        }

        async fn prepare_wayland(
            &self,
            _request: crate::application::sessions::WaylandPreparationRequest,
        ) -> Result<crate::application::sessions::WaylandSessionContext, SessionError> {
            panic!("launch must not probe Wayland before machine readiness")
        }

        async fn probe_x11_projection(
            &self,
            _request: crate::application::sessions::X11ProjectionProbeRequest,
        ) -> Result<crate::application::sessions::X11ProjectionContext, SessionError> {
            panic!("launch must not probe X11 before machine readiness")
        }

        async fn open_terminal(
            &self,
            request: crate::application::sessions::TerminalSessionRequest,
        ) -> Result<
            crate::application::sessions::TerminalSessionHandle,
            crate::application::sessions::TerminalOpenError,
        > {
            use crate::application::sessions::{terminal_session_channel, TerminalLaunch};
            use crate::domain::session::{SessionLifecycle, TerminalAttachmentKind};

            assert_eq!(request.machine.as_str(), "demo");
            let TerminalLaunch::SelectedUserShell { user, command, .. } = request.launch else {
                panic!("launch requires a selected-user command");
            };
            assert_eq!(user.as_str(), "alice");
            self.commands.lock().push(command.unwrap());
            let (handle, endpoint) =
                terminal_session_channel(request.id, TerminalAttachmentKind::Login);
            endpoint
                .lifecycle
                .send(SessionLifecycle::Exited {
                    success: false,
                    code: Some(37),
                })
                .unwrap();
            Ok(handle)
        }

        async fn open_journal(
            &self,
            _request: crate::application::sessions::JournalSessionRequest,
        ) -> Result<crate::application::sessions::JournalSessionHandle, SessionError> {
            panic!("launch must not open a journal")
        }
    }

    struct UnusedX11Preparation;

    #[async_trait::async_trait]
    impl X11SessionPreparationPort for UnusedX11Preparation {
        async fn prepare_session(
            &self,
            _target: ShellTarget,
            _selection: X11SessionSelection,
        ) -> Result<X11SessionPreparation, X11AccessError> {
            panic!("launch must not authorize X11 before machine readiness")
        }
    }

    fn observed_launch_machine(
        state: crate::domain::runtime::MachineState,
    ) -> (RuntimeCatalog, std::sync::Arc<MachineLifecycleService>) {
        use crate::application::machine_lifecycle::{
            MockMachineControl, MockMachineObservation, MockMachineStartDiagnostics,
            MockMachineStartPreparation,
        };
        use crate::application::operations::{ExecutionRoute, OperationRegistry};
        use crate::application::runtime::MockRuntimePort;
        use crate::domain::runtime::MachineEntry;
        use std::sync::Arc;

        let mut port = MockRuntimePort::new();
        port.expect_list_machines()
            .once()
            .returning(move || Ok(vec![MachineEntry::optimistic_nspawn("demo", state.clone())]));
        port.expect_snapshot_route()
            .return_const(ExecutionRoute::LocalSystemdTools);
        let runtime = RuntimeCatalog::new(None, Arc::new(port), Default::default());
        let lifecycle = Arc::new(MachineLifecycleService::new(
            Arc::new(MockMachineControl::new()),
            Arc::new(MockMachineStartPreparation::new()),
            Arc::new(MockMachineObservation::new()),
            Arc::new(MockMachineStartDiagnostics::new()),
            OperationRegistry::new(),
        ));
        (runtime, lifecycle)
    }

    #[tokio::test]
    async fn launch_readiness_failure_does_not_enter_display_preparation_or_terminal() {
        use crate::domain::runtime::MachineState;
        use std::sync::Arc;

        let command = parse_launcher(&arguments(&[
            "--with-x11",
            "--wayland=wayland-1",
            "alice@demo",
            "/usr/bin/kitty",
        ]))
        .unwrap();
        let port = Arc::new(LauncherSession::default());
        let sessions = SessionService::new(port.clone());
        let (runtime, lifecycle) = observed_launch_machine(MachineState::Exiting);
        let result = run_launch(
            command,
            &sessions,
            &UnusedX11Preparation,
            &lifecycle,
            &runtime,
        )
        .await;
        assert!(matches!(
            result,
            Err(ShellCommandError::Machine(
                MachineReadyError::Unavailable { .. }
            ))
        ));
        assert!(port.commands.lock().is_empty());
    }

    #[tokio::test]
    async fn launch_preserves_guest_command_arguments_and_exit_status() {
        use crate::domain::runtime::MachineState;
        use std::sync::Arc;

        let command = parse_launcher(&arguments(&[
            "--no-wayland",
            "alice@demo",
            "/usr/bin/kitty",
            "--title",
            "a b",
            "",
        ]))
        .unwrap();
        let expected = command.command().unwrap().clone();
        let port = Arc::new(LauncherSession::default());
        let sessions = SessionService::new(port.clone());
        let (runtime, lifecycle) = observed_launch_machine(MachineState::Running);
        let code = run_launch(
            command,
            &sessions,
            &UnusedX11Preparation,
            &lifecycle,
            &runtime,
        )
        .await
        .unwrap();
        assert_eq!(code, 37);
        assert_eq!(port.commands.lock().as_slice(), &[expected]);
    }
}

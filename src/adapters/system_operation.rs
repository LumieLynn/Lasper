//! Typed host system operations shared by direct and elevated modes.

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::{CommandRunner, DefaultCommandRunner};
use crate::domain::machine::{AllowedSignal, MachineName};
use crate::domain::runtime::{ImageEntry, ImageName};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const SYSTEM_MUTATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Source evidence for a typed machinectl/systemctl operation.
///
/// This error stays inside the host adapter.  Daemon handlers map it to an
/// application outcome, while older adapter stores may explicitly convert it
/// to the transitional `NspawnError` until their contracts are migrated.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SystemOperationError {
    #[error("invalid target: {0}")]
    InvalidTarget(String),

    #[error("image '{0}' is protected and cannot be removed")]
    ProtectedImage(String),

    #[error("permission denied")]
    PermissionDenied,

    #[error("command failed ({context}): {command}. Output: {output}")]
    CommandFailed {
        context: String,
        command: String,
        output: String,
    },

    #[error("I/O error in {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("D-Bus error: {0}")]
    Dbus(#[source] zbus::Error),

    #[error("system operation outcome is unknown: {0}")]
    OutcomeUnknown(String),

    #[error("system operation failed: {0}")]
    Backend(String),
}

pub(crate) type SystemOperationResult<T> = std::result::Result<T, SystemOperationError>;

impl SystemOperationError {
    fn cmd_failed(
        context: impl Into<String>,
        command: impl Into<String>,
        output: &std::process::Output,
    ) -> Self {
        Self::CommandFailed {
            context: context.into(),
            command: command.into(),
            output: crate::adapters::process::command_diagnostic(output),
        }
    }
}

impl From<SystemOperationError> for NspawnError {
    fn from(error: SystemOperationError) -> Self {
        match error {
            SystemOperationError::InvalidTarget(message) => Self::Validation(message),
            SystemOperationError::ProtectedImage(image) => Self::ProtectedImage(image),
            SystemOperationError::PermissionDenied => Self::PermissionDenied,
            SystemOperationError::CommandFailed {
                context,
                command,
                output,
            } => Self::CommandFailed(context, command, output),
            SystemOperationError::Io { path, source } => Self::Io(path, source),
            SystemOperationError::Dbus(error) => Self::Dbus(error),
            SystemOperationError::OutcomeUnknown(message) => {
                Self::SystemOperationOutcomeUnknown(message)
            }
            SystemOperationError::Backend(message) => Self::Runtime(message),
        }
    }
}

fn legacy_system_operation_error(error: NspawnError) -> SystemOperationError {
    match error {
        NspawnError::Validation(message) | NspawnError::InvalidConfig(message) => {
            SystemOperationError::InvalidTarget(message)
        }
        NspawnError::ProtectedImage(image) => SystemOperationError::ProtectedImage(image),
        NspawnError::PermissionDenied => SystemOperationError::PermissionDenied,
        NspawnError::CommandFailed(context, command, output) => {
            SystemOperationError::CommandFailed {
                context,
                command,
                output,
            }
        }
        NspawnError::Io(path, source) => SystemOperationError::Io { path, source },
        NspawnError::GenericIo(source) => SystemOperationError::Io {
            path: PathBuf::from("system operation"),
            source,
        },
        NspawnError::Dbus(error) => SystemOperationError::Dbus(error),
        NspawnError::SystemOperationOutcomeUnknown(message) => {
            SystemOperationError::OutcomeUnknown(message)
        }
        other => SystemOperationError::Backend(other.to_string()),
    }
}

#[derive(Clone, Debug)]
pub(crate) enum SystemOperation {
    Start {
        machine: MachineName,
    },
    Terminate {
        machine: MachineName,
    },
    Poweroff {
        machine: MachineName,
    },
    Reboot {
        machine: MachineName,
    },
    Enable {
        machine: MachineName,
    },
    Disable {
        machine: MachineName,
    },
    Kill {
        machine: MachineName,
        signal: AllowedSignal,
    },
    RemoveImage {
        image: ImageName,
    },
    CloneImage {
        source: ImageName,
        destination: ImageName,
    },
    ReloadDaemon,
}

impl From<SystemOperation> for crate::ipc::protocol::system::SystemOperation {
    fn from(operation: SystemOperation) -> Self {
        use crate::ipc::protocol::system::SystemOperation as Wire;

        match operation {
            SystemOperation::Start { machine } => Wire::Start { machine },
            SystemOperation::Terminate { machine } => Wire::Terminate { machine },
            SystemOperation::Poweroff { machine } => Wire::Poweroff { machine },
            SystemOperation::Reboot { machine } => Wire::Reboot { machine },
            SystemOperation::Enable { machine } => Wire::Enable { machine },
            SystemOperation::Disable { machine } => Wire::Disable { machine },
            SystemOperation::Kill { machine, signal } => Wire::Kill { machine, signal },
            SystemOperation::RemoveImage { image } => Wire::RemoveImage { image },
            SystemOperation::CloneImage {
                source,
                destination,
            } => Wire::CloneImage {
                source,
                destination,
            },
            SystemOperation::ReloadDaemon => Wire::ReloadDaemon,
        }
    }
}

impl From<crate::ipc::protocol::system::SystemOperation> for SystemOperation {
    fn from(operation: crate::ipc::protocol::system::SystemOperation) -> Self {
        use crate::ipc::protocol::system::SystemOperation as Wire;

        match operation {
            Wire::Start { machine } => Self::Start { machine },
            Wire::Terminate { machine } => Self::Terminate { machine },
            Wire::Poweroff { machine } => Self::Poweroff { machine },
            Wire::Reboot { machine } => Self::Reboot { machine },
            Wire::Enable { machine } => Self::Enable { machine },
            Wire::Disable { machine } => Self::Disable { machine },
            Wire::Kill { machine, signal } => Self::Kill { machine, signal },
            Wire::RemoveImage { image } => Self::RemoveImage { image },
            Wire::CloneImage {
                source,
                destination,
            } => Self::CloneImage {
                source,
                destination,
            },
            Wire::ReloadDaemon => Self::ReloadDaemon,
        }
    }
}

#[derive(Clone)]
pub struct SystemOperationStore {
    executor: Arc<dyn SystemOperationExecutor>,
}

impl SystemOperationStore {
    pub(crate) fn direct(local_runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            executor: Arc::new(DirectSystemOperationExecutor { local_runner }),
        }
    }

    pub(crate) fn elevated(daemon: Arc<ElevatedDaemon>) -> Self {
        Self {
            executor: Arc::new(ElevatedSystemOperationExecutor { daemon }),
        }
    }

    async fn execute(&self, operation: SystemOperation) -> Result<()> {
        self.executor
            .execute(operation)
            .await
            .map_err(NspawnError::from)
    }

    pub async fn disable(&self, name: &str) -> Result<()> {
        self.execute(SystemOperation::Disable {
            machine: machine_name(name)?,
        })
        .await
    }

    pub async fn remove_image(&self, name: &str) -> Result<()> {
        if ImageEntry::is_protected_name(name) {
            return Err(NspawnError::ProtectedImage(name.into()));
        }
        self.execute(SystemOperation::RemoveImage {
            image: image_name(name)?,
        })
        .await
    }

    pub async fn clone_image(&self, source: &str, destination: &str) -> Result<()> {
        self.execute(SystemOperation::CloneImage {
            source: image_name(source)?,
            destination: image_name(destination)?,
        })
        .await
    }

    pub async fn reload_daemon(&self) -> Result<()> {
        self.execute(SystemOperation::ReloadDaemon).await
    }
}

pub(crate) async fn execute_systemd_tools_image_remove(
    image: ImageName,
) -> SystemOperationResult<()> {
    execute_systemd_tools_image_remove_with_runner(image, &DefaultCommandRunner).await
}

pub(crate) async fn execute_systemd_tools_image_remove_with_runner(
    image: ImageName,
    runner: &dyn CommandRunner,
) -> SystemOperationResult<()> {
    execute_system_operation_with_context(
        SystemOperation::RemoveImage { image },
        runner,
        "typed image removal",
    )
    .await
}

impl std::fmt::Debug for SystemOperationStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemOperationStore")
            .field("route", &self.executor.route())
            .finish()
    }
}

#[async_trait::async_trait]
trait SystemOperationExecutor: Send + Sync + 'static {
    fn route(&self) -> &'static str;

    async fn execute(&self, operation: SystemOperation) -> SystemOperationResult<()>;
}

struct DirectSystemOperationExecutor {
    local_runner: Arc<dyn CommandRunner>,
}

#[async_trait::async_trait]
impl SystemOperationExecutor for DirectSystemOperationExecutor {
    fn route(&self) -> &'static str {
        "direct"
    }

    async fn execute(&self, operation: SystemOperation) -> SystemOperationResult<()> {
        execute_system_operation_with_runner(operation, self.local_runner.as_ref()).await
    }
}

struct ElevatedSystemOperationExecutor {
    daemon: Arc<ElevatedDaemon>,
}

#[async_trait::async_trait]
impl SystemOperationExecutor for ElevatedSystemOperationExecutor {
    fn route(&self) -> &'static str {
        "elevated_rpc"
    }

    async fn execute(&self, operation: SystemOperation) -> SystemOperationResult<()> {
        self.daemon
            .system_operation(operation)
            .await
            .map_err(|source| SystemOperationError::Io {
                path: PathBuf::from("system operation"),
                source,
            })
    }
}

fn machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn image_name(name: &str) -> Result<ImageName> {
    ImageName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

pub(crate) async fn execute_system_operation(
    operation: SystemOperation,
) -> SystemOperationResult<()> {
    execute_system_operation_with_runner(operation, &DefaultCommandRunner).await
}

pub(crate) async fn execute_dbus_system_operation(
    dbus: &crate::adapters::runtime::dbus::DbusBackend,
    operation: SystemOperation,
) -> SystemOperationResult<()> {
    let result = match operation {
        SystemOperation::Start { machine } => dbus.start(machine.as_str()).await,
        SystemOperation::Terminate { machine } => dbus.terminate(machine.as_str()).await,
        SystemOperation::Poweroff { machine } => dbus.poweroff(machine.as_str()).await,
        SystemOperation::Reboot { machine } => dbus.reboot(machine.as_str()).await,
        SystemOperation::Enable { machine } => dbus.enable(machine.as_str()).await,
        SystemOperation::Disable { machine } => dbus.disable(machine.as_str()).await,
        SystemOperation::Kill { machine, signal } => dbus.kill(machine.as_str(), signal).await,
        SystemOperation::RemoveImage { image } => dbus.remove(image.as_str()).await,
        SystemOperation::ReloadDaemon => dbus.reload_daemon().await,
        SystemOperation::CloneImage { .. } => Err(NspawnError::Validation(
            "image cloning is not a machined D-Bus operation".into(),
        )),
    };
    result.map_err(legacy_system_operation_error)
}

pub(crate) async fn execute_system_operation_with_runner(
    operation: SystemOperation,
    runner: &dyn CommandRunner,
) -> SystemOperationResult<()> {
    execute_system_operation_with_context(operation, runner, "typed system operation").await
}

async fn execute_system_operation_with_context(
    operation: SystemOperation,
    runner: &dyn CommandRunner,
    context: &str,
) -> SystemOperationResult<()> {
    let completion = completion_policy(&operation);
    let (program, args) = command(&operation)?;
    let (output, deadline) = match completion {
        CommandCompletionPolicy::Bounded(timeout) => (
            runner.run_bounded(program, args.clone(), timeout).await,
            Some(timeout),
        ),
        CommandCompletionPolicy::WaitForAuthoritativeCompletion => {
            (runner.run(program, args.clone()).await, None)
        }
    };
    let output = output.map_err(|source| {
        if source.kind() == std::io::ErrorKind::TimedOut {
            let timing = deadline
                .map(|timeout| format!(" exceeded its {}s completion deadline", timeout.as_secs()))
                .unwrap_or_else(|| " lost its authoritative completion result".into());
            SystemOperationError::OutcomeUnknown(format!(
                "{} {}{}; reconcile host state before retrying",
                program,
                args.join(" "),
                timing
            ))
        } else {
            SystemOperationError::Io {
                path: PathBuf::from(program),
                source,
            }
        }
    })?;
    crate::adapters::process::log_output(program, &output);
    if output.status.success() {
        Ok(())
    } else {
        Err(SystemOperationError::cmd_failed(
            context,
            format!("{} {}", program, args.join(" ")),
            &output,
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandCompletionPolicy {
    Bounded(Duration),
    WaitForAuthoritativeCompletion,
}

fn completion_policy(operation: &SystemOperation) -> CommandCompletionPolicy {
    match operation {
        // machined intentionally permits image removal to take arbitrarily
        // long, while clone duration scales with image size. Both are run by
        // background operations whose resource claims prevent local races.
        SystemOperation::RemoveImage { .. } | SystemOperation::CloneImage { .. } => {
            CommandCompletionPolicy::WaitForAuthoritativeCompletion
        }
        SystemOperation::Start { .. }
        | SystemOperation::Terminate { .. }
        | SystemOperation::Poweroff { .. }
        | SystemOperation::Reboot { .. }
        | SystemOperation::Enable { .. }
        | SystemOperation::Disable { .. }
        | SystemOperation::Kill { .. }
        | SystemOperation::ReloadDaemon => {
            CommandCompletionPolicy::Bounded(SYSTEM_MUTATION_TIMEOUT)
        }
    }
}

fn command(operation: &SystemOperation) -> SystemOperationResult<(&'static str, Vec<String>)> {
    let command = match operation {
        SystemOperation::Start { machine } => ("machinectl", vec!["--", "start", machine.as_str()]),
        SystemOperation::Terminate { machine } => {
            ("machinectl", vec!["--", "terminate", machine.as_str()])
        }
        SystemOperation::Poweroff { machine } => {
            ("machinectl", vec!["--", "poweroff", machine.as_str()])
        }
        SystemOperation::Reboot { machine } => {
            ("machinectl", vec!["--", "reboot", machine.as_str()])
        }
        SystemOperation::Enable { machine } => {
            ("machinectl", vec!["--", "enable", machine.as_str()])
        }
        SystemOperation::Disable { machine } => {
            ("machinectl", vec!["--", "disable", machine.as_str()])
        }
        SystemOperation::Kill { machine, signal } => (
            "machinectl",
            vec!["-s", signal.as_name(), "--", "kill", machine.as_str()],
        ),
        SystemOperation::RemoveImage { image } => {
            if ImageEntry::is_protected_name(image.as_str()) {
                return Err(SystemOperationError::ProtectedImage(image.as_str().into()));
            }
            ("machinectl", vec!["--", "remove", image.as_str()])
        }
        SystemOperation::CloneImage {
            source,
            destination,
        } => (
            "machinectl",
            vec!["--", "clone", source.as_str(), destination.as_str()],
        ),
        SystemOperation::ReloadDaemon => ("systemctl", vec!["--", "daemon-reload"]),
    };
    Ok((
        command.0,
        command.1.into_iter().map(str::to_string).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn success() -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: vec![],
            stderr: vec![],
        }
    }

    fn failure(stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: vec![],
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    struct RecordingExecutor {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SystemOperationExecutor for RecordingExecutor {
        fn route(&self) -> &'static str {
            "recording"
        }

        async fn execute(&self, operation: SystemOperation) -> SystemOperationResult<()> {
            assert!(matches!(operation, SystemOperation::Disable { .. }));
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn store_delegates_to_its_fixed_executor() {
        let executor = Arc::new(RecordingExecutor {
            calls: AtomicUsize::new(0),
        });
        let store = SystemOperationStore {
            executor: executor.clone(),
        };

        store.disable("test-machine").await.unwrap();

        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(format!("{store:?}").contains("recording"));
    }

    #[test]
    fn completion_policy_separates_short_mutations_from_long_operations() {
        let machine = MachineName::new("test-machine").unwrap();
        assert_eq!(
            completion_policy(&SystemOperation::Start {
                machine: machine.clone(),
            }),
            CommandCompletionPolicy::Bounded(SYSTEM_MUTATION_TIMEOUT)
        );
        assert_eq!(
            completion_policy(&SystemOperation::RemoveImage {
                image: ImageName::new("test-image").unwrap(),
            }),
            CommandCompletionPolicy::WaitForAuthoritativeCompletion
        );
        assert_eq!(
            completion_policy(&SystemOperation::CloneImage {
                source: ImageName::new("base").unwrap(),
                destination: ImageName::new("clone").unwrap(),
            }),
            CommandCompletionPolicy::WaitForAuthoritativeCompletion
        );
    }

    #[test]
    fn operation_deserialization_rejects_untyped_arguments() {
        let traversal = r#"{"operation":"remove_image","image":"../host"}"#;
        assert!(
            serde_json::from_str::<crate::ipc::protocol::system::SystemOperation>(traversal)
                .is_err()
        );

        let arbitrary = r#"{"operation":"start","machine":"valid","program":"sh"}"#;
        assert!(
            serde_json::from_str::<crate::ipc::protocol::system::SystemOperation>(arbitrary)
                .is_err()
        );
    }

    #[test]
    fn operation_wire_format_contains_no_program_or_argv_fields() {
        let operation = SystemOperation::Kill {
            machine: MachineName::new("test-machine").unwrap(),
            signal: AllowedSignal::Kill,
        };
        let value = serde_json::to_value(crate::ipc::protocol::system::SystemOperation::from(
            operation,
        ))
        .unwrap();

        assert_eq!(value["operation"], "kill");
        assert_eq!(value["machine"], "test-machine");
        assert_eq!(value["signal"], "SIGKILL");
        assert!(value.get("program").is_none());
        assert!(value.get("args").is_none());
    }

    #[test]
    fn adapter_operation_maps_to_the_private_wire_contract() {
        let operation = SystemOperation::Kill {
            machine: MachineName::new("test-machine").unwrap(),
            signal: AllowedSignal::Kill,
        };
        let wire = crate::ipc::protocol::system::SystemOperation::from(operation.clone());
        let round_trip = SystemOperation::from(wire.clone());
        let round_trip_wire = crate::ipc::protocol::system::SystemOperation::from(round_trip);

        assert_eq!(wire, round_trip_wire);
    }

    #[tokio::test]
    async fn hidden_image_removal_is_a_fixed_machinectl_command() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .withf(|program, args| {
                program == "machinectl"
                    && args.len() == 3
                    && args[0] == "--"
                    && args[1] == "remove"
                    && args[2] == ".oci-sha256:abc"
            })
            .returning(|_, _| Ok(success()));

        execute_system_operation_with_runner(
            SystemOperation::RemoveImage {
                image: ImageName::new(".oci-sha256:abc").unwrap(),
            },
            &runner,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn typed_systemd_tools_removal_reports_success() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run().once().returning(|_, _| Ok(success()));

        assert!(execute_systemd_tools_image_remove_with_runner(
            ImageName::new("ubuntu").unwrap(),
            &runner,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn typed_systemd_tools_removal_reports_launch_failure_as_not_attempted() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run().once().returning(|_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "machinectl missing",
            ))
        });

        assert!(matches!(
            execute_systemd_tools_image_remove_with_runner(
                ImageName::new("ubuntu").unwrap(),
                &runner,
            )
            .await,
            Err(SystemOperationError::Io { .. })
        ));
    }

    #[tokio::test]
    async fn typed_systemd_tools_removal_reports_nonzero_exit_as_failed() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .once()
            .returning(|_, _| Ok(failure("image is busy")));

        assert!(matches!(
            execute_systemd_tools_image_remove_with_runner(
                ImageName::new("ubuntu").unwrap(),
                &runner,
            )
            .await,
            Err(SystemOperationError::CommandFailed { .. })
        ));
    }

    #[tokio::test]
    async fn short_systemd_tools_mutation_timeout_preserves_unknown_outcome() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .withf(|program, args, timeout| {
                program == "systemctl"
                    && args.iter().map(String::as_str).eq(["--", "daemon-reload"])
                    && *timeout == SYSTEM_MUTATION_TIMEOUT
            })
            .returning(|_, _, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "deadline exceeded",
                ))
            });

        assert!(matches!(
            execute_system_operation_with_runner(SystemOperation::ReloadDaemon, &runner).await,
            Err(SystemOperationError::OutcomeUnknown(_))
        ));
    }

    #[test]
    fn legacy_unknown_outcome_keeps_its_semantics() {
        assert!(matches!(
            legacy_system_operation_error(NspawnError::SystemOperationOutcomeUnknown(
                "deadline exceeded".into()
            )),
            SystemOperationError::OutcomeUnknown(_)
        ));
    }

    #[tokio::test]
    async fn host_image_is_rejected_before_command_execution() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run().never();

        let error = execute_system_operation_with_runner(
            SystemOperation::RemoveImage {
                image: ImageName::new(".host").unwrap(),
            },
            &runner,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SystemOperationError::ProtectedImage(_)));
    }

    #[tokio::test]
    async fn clone_source_uses_image_name_semantics() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .withf(|program, args| {
                program == "machinectl"
                    && args.iter().map(String::as_str).eq([
                        "--",
                        "clone",
                        "Base Image",
                        "clone-target",
                    ])
            })
            .returning(|_, _| Ok(success()));

        execute_system_operation_with_runner(
            SystemOperation::CloneImage {
                source: ImageName::new("Base Image").unwrap(),
                destination: ImageName::new("clone-target").unwrap(),
            },
            &runner,
        )
        .await
        .unwrap();
    }
}

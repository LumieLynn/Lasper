//! Start and reap the privilege launcher, including run0's system service.

use crate::adapters::process::{command_diagnostic, CommandRunner, DefaultCommandRunner};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};

// Provisioning has an eight-second cancellation grace, followed by session
// cleanup. Let the daemon finish that work before forcing launcher shutdown.
const DAEMON_EXIT_GRACE: Duration = Duration::from_secs(12);
const SERVICE_QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(16);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ElevationMethod {
    #[default]
    Sudo,
    Run0,
}

impl ElevationMethod {
    fn program(self) -> &'static str {
        match self {
            Self::Sudo => "sudo",
            Self::Run0 => "run0",
        }
    }
}

impl std::fmt::Display for ElevationMethod {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.program())
    }
}

impl std::str::FromStr for ElevationMethod {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "sudo" => Ok(Self::Sudo),
            "run0" => Ok(Self::Run0),
            _ => Err(format!(
                "unknown elevation method '{value}'; expected sudo or run0"
            )),
        }
    }
}

pub(super) struct DaemonInvocation<'a> {
    pub(super) executable: &'a Path,
    pub(super) fd_socket: &'a Path,
    pub(super) rpc_socket: &'a Path,
    pub(super) invoking_uid: u32,
    pub(super) frontend_pid: u32,
}

pub(super) struct DaemonLauncher {
    method: ElevationMethod,
    child: Option<Child>,
    stdout_task: Option<tokio::task::JoinHandle<()>>,
    service_unit: Option<String>,
    socket_directory: Arc<tempfile::TempDir>,
}

impl DaemonLauncher {
    pub(super) fn spawn(
        method: ElevationMethod,
        invocation: DaemonInvocation<'_>,
        socket_directory: Arc<tempfile::TempDir>,
    ) -> std::io::Result<Self> {
        let service_unit = (method == ElevationMethod::Run0)
            .then(|| format!("lasper-daemon-{}.service", uuid::Uuid::new_v4().simple()));
        let mut command = daemon_command(method, &invocation, service_unit.as_deref());
        let mut child = command.spawn().map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("failed to launch {method} daemon: {error}"),
            )
        })?;
        let mut stdout = child.stdout.take().expect("launcher stdout is piped");
        let stdout_task = tokio::spawn(async move {
            if let Err(error) = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await {
                log::debug!("Failed to drain elevated daemon stdout: {error}");
            }
        });
        Ok(Self {
            method,
            child: Some(child),
            stdout_task: Some(stdout_task),
            service_unit,
            socket_directory,
        })
    }

    pub(super) async fn connect_rpc_socket(
        &mut self,
        path: &Path,
    ) -> std::io::Result<tokio::net::UnixStream> {
        wait_for_rpc_socket(
            path,
            self.child.as_mut().expect("launcher is running"),
            self.method,
        )
        .await
    }

    /// Close RPC first, then wait for the daemon and its launcher to finish.
    pub(super) async fn finish(self) -> std::io::Result<()> {
        self.finish_with_grace(DAEMON_EXIT_GRACE).await
    }

    pub(super) async fn abort(self) -> std::io::Result<()> {
        self.finish_with_grace(Duration::ZERO).await
    }

    async fn finish_with_grace(mut self, grace: Duration) -> std::io::Result<()> {
        let child = self.child.as_mut().expect("launcher is running");
        let result = match tokio::time::timeout(grace, child.wait()).await {
            Ok(Ok(status)) if status.success() => {
                log::debug!("{} daemon launcher exited with {status}", self.method);
                Ok(())
            }
            Ok(Ok(status)) => {
                log::warn!("{} daemon launcher exited with {status}", self.method);
                self.terminate().await
            }
            Ok(Err(error)) => {
                log::warn!(
                    "Failed to wait for {} daemon launcher: {error}",
                    self.method
                );
                self.terminate().await
            }
            Err(_) => {
                if !grace.is_zero() {
                    log::warn!(
                        "{} daemon did not exit within {}s after RPC shutdown",
                        self.method,
                        grace.as_secs()
                    );
                }
                self.terminate().await
            }
        };
        self.child.take();
        self.service_unit.take();
        if let Some(task) = self.stdout_task.take() {
            task.abort();
            let _ = task.await;
        }
        result
    }

    /// Cancel the launcher before stopping its unit, so it cannot keep asking
    /// for authorization while cleanup runs. Killing run0 alone does not stop
    /// a service which systemd has already accepted.
    async fn terminate(&mut self) -> std::io::Result<()> {
        let child_result = match self.child.as_mut() {
            Some(child) => child.kill().await,
            None => Ok(()),
        };
        let service_result = match &self.service_unit {
            Some(unit) => stop_service(unit, &DefaultCommandRunner).await,
            None => Ok(()),
        };
        service_result.and(child_result)
    }
}

impl Drop for DaemonLauncher {
    fn drop(&mut self) {
        if let Some(task) = self.stdout_task.take() {
            task.abort();
        }
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = child.start_kill();
        let unit = self.service_unit.take();
        // Startup can fail or be cancelled before ownership reaches the RPC
        // task. Keep the socket directory alive until this cleanup finishes.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let directory = Arc::clone(&self.socket_directory);
            runtime.spawn(async move {
                let _directory = directory;
                let _ = child.wait().await;
                if let Some(unit) = unit {
                    if let Err(error) = stop_service(&unit, &DefaultCommandRunner).await {
                        log::error!("Failed to clean up run0 daemon: {error}");
                    }
                }
            });
        } else if let Some(unit) = unit {
            log::error!("Cannot confirm run0 daemon cleanup; inspect {unit}");
        }
    }
}

fn daemon_command(
    method: ElevationMethod,
    invocation: &DaemonInvocation<'_>,
    service_unit: Option<&str>,
) -> Command {
    let mut command = Command::new(method.program());
    command.kill_on_drop(true);
    if let Some(unit) = service_unit {
        command
            .arg(format!("--unit={unit}"))
            .arg("--property=TimeoutStopSec=12s")
            .arg("--chdir=/");
    }
    command
        .arg("--")
        .arg(invocation.executable)
        .arg("--daemon")
        .arg("--fd-sock")
        .arg(invocation.fd_socket)
        .arg("--rpc-sock")
        .arg(invocation.rpc_socket)
        .arg("--daemon-uid")
        .arg(invocation.invoking_uid.to_string())
        .arg("--daemon-pid")
        .arg(invocation.frontend_pid.to_string());
    configure_daemon_stdio(&mut command);
    command
}

fn configure_daemon_stdio(command: &mut Command) {
    // A stdout pipe avoids sudo-rs's foreground PTY relay and selects direct
    // stdio in run0, including v256 which has no explicit --pipe option. The
    // authentication agent can still open the controlling terminal itself.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
}

async fn wait_for_rpc_socket(
    path: &Path,
    child: &mut Child,
    method: ElevationMethod,
) -> std::io::Result<tokio::net::UnixStream> {
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                if let Some(status) = child.try_wait()? {
                    return Err(std::io::Error::other(format!(
                        "{method} daemon exited before its RPC socket became ready ({status}); last connection error: {error}"
                    )));
                }
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                ) {
                    return Err(error);
                }
            }
        }
        // Authentication has no fixed deadline: a password prompt may stay
        // open while the user is away. Launcher exit still ends this wait.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn stop_service(unit: &str, runner: &dyn CommandRunner) -> std::io::Result<()> {
    // Authentication can fail before StartTransientUnit creates anything.
    // Read LoadState first: StopUnit itself can require authorization even
    // for a missing name, which would obscure the original launch error.
    let observation = runner.run_bounded(
        "systemctl",
        [
            "--system",
            "--no-ask-password",
            "show",
            "--property=LoadState",
            "--value",
            "--",
            unit,
        ].map(str::to_string).to_vec(),
        SERVICE_QUERY_TIMEOUT,
    )
        .await
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("failed to inspect run0 daemon unit {unit}: {error}; service exit is unconfirmed"),
            )
        })?;
    if !service_is_present(unit, &observation)? {
        return Ok(());
    }

    let output = runner
        .run_bounded(
            "systemctl",
            ["--system", "--no-ask-password", "stop", "--", unit]
                .map(str::to_string)
                .to_vec(),
            SERVICE_STOP_TIMEOUT,
        )
        .await
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "failed to stop run0 daemon unit {unit}: {error}; service exit is unconfirmed"
                ),
            )
        })?;
    validate_service_stop(unit, &output)
}

fn service_is_present(unit: &str, output: &std::process::Output) -> std::io::Result<bool> {
    let state = String::from_utf8_lossy(&output.stdout);
    if state.trim() == "not-found" {
        Ok(false)
    } else if output.status.success() && !state.trim().is_empty() {
        Ok(true)
    } else {
        Err(std::io::Error::other(format!(
            "failed to inspect run0 daemon unit {unit}: {}; service exit is unconfirmed",
            command_diagnostic(output)
        )))
    }
}

fn validate_service_stop(unit: &str, output: &std::process::Output) -> std::io::Result<()> {
    // systemctl maps NoSuchUnit to LSB EXIT_NOTINSTALLED (5). A transient
    // service may already have been collected after the daemon exited.
    if output.status.success() || output.status.code() == Some(5) {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "failed to stop run0 daemon unit {unit}: {}; service exit is unconfirmed",
            command_diagnostic(output)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;

    fn launch_test_process(command: &mut Command) -> DaemonLauncher {
        configure_daemon_stdio(command);
        command.kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let stdout_task = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await;
        });
        DaemonLauncher {
            method: ElevationMethod::Sudo,
            child: Some(child),
            stdout_task: Some(stdout_task),
            service_unit: None,
            socket_directory: Arc::new(tempfile::tempdir().unwrap()),
        }
    }

    #[test]
    fn launchers_keep_daemon_paths_and_identity_as_separate_arguments() {
        let executable = Path::new("/opt/Lasper test/lasper");
        let invocation = DaemonInvocation {
            executable,
            fd_socket: Path::new("/run/user/1000/lasper-test/fd.sock"),
            rpc_socket: Path::new("/run/user/1000/lasper-test/rpc.sock"),
            invoking_uid: 1000,
            frontend_pid: 4321,
        };
        for method in [ElevationMethod::Sudo, ElevationMethod::Run0] {
            let unit = (method == ElevationMethod::Run0).then_some("lasper-daemon-test.service");
            let command = daemon_command(method, &invocation, unit);
            let arguments = command.as_std().get_args().collect::<Vec<_>>();
            let separator = arguments
                .iter()
                .position(|argument| *argument == "--")
                .unwrap();
            assert_eq!(arguments[separator + 1], executable.as_os_str());
            assert_eq!(arguments[separator + 2], "--daemon");
            assert_eq!(
                &arguments[arguments.len() - 4..],
                ["--daemon-uid", "1000", "--daemon-pid", "4321"]
            );
            if method == ElevationMethod::Run0 {
                assert!(arguments[..separator]
                    .contains(&std::ffi::OsStr::new("--unit=lasper-daemon-test.service")));
                // All options used here are available in run0 v256.
                assert!(!arguments.contains(&std::ffi::OsStr::new("--pipe")));
            }
        }
    }

    #[tokio::test]
    async fn daemon_stdio_does_not_attach_the_launcher_to_terminal_input() {
        let mut command = Command::new("sh");
        command.args(["-c", "test ! -t 0 && test -p /proc/self/fd/1"]);
        let mut launcher = launch_test_process(&mut command);
        assert!(launcher
            .child
            .as_mut()
            .unwrap()
            .wait()
            .await
            .unwrap()
            .success());
        launcher.finish().await.unwrap();
    }

    #[tokio::test]
    async fn lingering_launcher_is_killed_and_reaped_after_shutdown_grace() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let launcher = launch_test_process(&mut command);
        let pid = launcher.child.as_ref().unwrap().id().unwrap();
        let pidfd = crate::adapters::process::open_pidfd(pid).unwrap();
        launcher
            .finish_with_grace(Duration::from_millis(1))
            .await
            .unwrap();
        let pidfd = tokio::io::unix::AsyncFd::new(pidfd).unwrap();
        let _ready = tokio::time::timeout(Duration::from_secs(1), pidfd.readable())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[tokio::test]
    async fn cancelled_startup_reaps_the_launcher() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let launcher = launch_test_process(&mut command);
        let pid = launcher.child.as_ref().unwrap().id().unwrap();
        let pidfd =
            tokio::io::unix::AsyncFd::new(crate::adapters::process::open_pidfd(pid).unwrap())
                .unwrap();
        let directory = Arc::downgrade(&launcher.socket_directory);
        drop(launcher);
        let _ready = tokio::time::timeout(Duration::from_secs(1), pidfd.readable())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while directory.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn socket_wait_allows_time_for_interactive_authentication() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("rpc.sock");
        let listener_path = socket_path.clone();
        let listener = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(6)).await;
            tokio::net::UnixListener::bind(listener_path).unwrap()
        });
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut launcher = launch_test_process(&mut command);
        let stream = launcher.connect_rpc_socket(&socket_path).await.unwrap();
        drop(stream);
        drop(listener.await.unwrap());
        launcher.abort().await.unwrap();
    }

    #[tokio::test]
    async fn early_launcher_exit_ends_socket_wait_with_its_method_and_status() {
        for method in [ElevationMethod::Sudo, ElevationMethod::Run0] {
            let directory = tempfile::tempdir().unwrap();
            let mut child = Command::new("sh").args(["-c", "exit 23"]).spawn().unwrap();
            let error = wait_for_rpc_socket(&directory.path().join("rpc.sock"), &mut child, method)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(method.program()));
            assert!(error.to_string().contains("exit status: 23"));
        }
    }

    #[test]
    fn collected_units_are_distinguished_from_denied_service_stop_requests() {
        for status in [0, 5] {
            let output = std::process::Output {
                status: std::process::ExitStatus::from_raw(status << 8),
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
            validate_service_stop("lasper-daemon-test.service", &output).unwrap();
        }
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(4 << 8),
            stdout: Vec::new(),
            stderr: b"Access denied".to_vec(),
        };
        let error = validate_service_stop("lasper-daemon-test.service", &output)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Access denied"));
        assert!(error.contains("lasper-daemon-test.service"));
        assert!(error.contains("unconfirmed"));
    }

    #[test]
    fn failed_authorization_does_not_require_stopping_an_absent_service() {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"not-found\n".to_vec(),
            stderr: Vec::new(),
        };
        assert!(!service_is_present("lasper-daemon-test.service", &output).unwrap());
        let present = std::process::Output {
            stdout: b"loaded\n".to_vec(),
            ..output
        };
        assert!(service_is_present("lasper-daemon-test.service", &present).unwrap());
        let denied = std::process::Output {
            status: std::process::ExitStatus::from_raw(4 << 8),
            stdout: Vec::new(),
            stderr: b"Access denied".to_vec(),
        };
        assert!(service_is_present("lasper-daemon-test.service", &denied)
            .unwrap_err()
            .to_string()
            .contains("Access denied"));
    }

    #[tokio::test]
    async fn absent_service_cleanup_only_uses_a_read_query() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .withf(|program, args, _| {
                program == "systemctl"
                    && args[2] == "show"
                    && args.last().unwrap() == "lasper-daemon-test.service"
            })
            .times(1)
            .return_once(|_, _, _| {
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: b"not-found\n".to_vec(),
                    stderr: Vec::new(),
                })
            });
        stop_service("lasper-daemon-test.service", &runner)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn present_service_stop_cannot_prompt_and_keeps_authorization_errors() {
        let mut runner = MockCommandRunner::new();
        let mut sequence = mockall::Sequence::new();
        runner
            .expect_run_bounded()
            .withf(|_, args, _| args[2] == "show")
            .times(1)
            .in_sequence(&mut sequence)
            .return_once(|_, _, _| {
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(0),
                    stdout: b"loaded\n".to_vec(),
                    stderr: Vec::new(),
                })
            });
        runner
            .expect_run_bounded()
            .withf(|program, args, timeout| {
                program == "systemctl"
                    && args[1] == "--no-ask-password"
                    && args[2] == "stop"
                    && args.last().unwrap() == "lasper-daemon-test.service"
                    && !timeout.is_zero()
            })
            .times(1)
            .in_sequence(&mut sequence)
            .return_once(|_, _, _| {
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(4 << 8),
                    stdout: Vec::new(),
                    stderr: b"Access denied".to_vec(),
                })
            });
        let error = stop_service("lasper-daemon-test.service", &runner)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Access denied"));
        assert!(error.contains("service exit is unconfirmed"));
    }
}

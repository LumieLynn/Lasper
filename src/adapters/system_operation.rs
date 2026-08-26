//! Typed host system operations shared by direct and elevated modes.

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::lifecycle::error::map_image_control_error;
use crate::adapters::process::{CommandRunner, DefaultCommandRunner};
use crate::application::image_lifecycle::ImageControlOutcome;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{AllowedSignal, ImageEntry, ImageName, MachineName};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
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
        self.executor.execute(operation).await
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

pub(crate) async fn execute_cli_image_remove(image: ImageName) -> ImageControlOutcome {
    execute_cli_image_remove_with_runner(image, &DefaultCommandRunner).await
}

pub(crate) async fn execute_cli_image_remove_with_runner(
    image: ImageName,
    runner: &dyn CommandRunner,
) -> ImageControlOutcome {
    let operation = SystemOperation::RemoveImage { image };
    let (program, args) = match command(&operation) {
        Ok(command) => command,
        Err(error) => return map_image_control_error(error),
    };
    let output = match runner.run(program, args.clone()).await {
        Ok(output) => output,
        Err(error) => {
            return ImageControlOutcome::NotAttempted {
                reason: format!("failed to launch {program}: {error}"),
            }
        }
    };
    crate::adapters::process::log_output(program, &output);
    if output.status.success() {
        ImageControlOutcome::Removed
    } else {
        ImageControlOutcome::Failed {
            reason: NspawnError::cmd_failed(
                "typed image removal",
                format!("{} {}", program, args.join(" ")),
                &output,
            )
            .to_string(),
        }
    }
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

    async fn execute(&self, operation: SystemOperation) -> Result<()>;
}

struct DirectSystemOperationExecutor {
    local_runner: Arc<dyn CommandRunner>,
}

#[async_trait::async_trait]
impl SystemOperationExecutor for DirectSystemOperationExecutor {
    fn route(&self) -> &'static str {
        "direct"
    }

    async fn execute(&self, operation: SystemOperation) -> Result<()> {
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

    async fn execute(&self, operation: SystemOperation) -> Result<()> {
        self.daemon
            .system_operation(operation)
            .await
            .map_err(|error| NspawnError::Io(PathBuf::from("system operation"), error))
    }
}

fn machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn image_name(name: &str) -> Result<ImageName> {
    ImageName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

pub(crate) async fn execute_system_operation(operation: SystemOperation) -> Result<()> {
    execute_system_operation_with_runner(operation, &DefaultCommandRunner).await
}

pub(crate) async fn execute_dbus_system_operation(
    dbus: &crate::adapters::runtime::dbus::DbusBackend,
    operation: SystemOperation,
) -> Result<()> {
    match operation {
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
    }
}

pub(crate) async fn execute_system_operation_with_runner(
    operation: SystemOperation,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let (program, args) = command(&operation)?;
    let output = runner
        .run(program, args.clone())
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from(program), error))?;
    crate::adapters::process::log_output(program, &output);
    if output.status.success() {
        Ok(())
    } else {
        Err(NspawnError::cmd_failed(
            "typed system operation",
            format!("{} {}", program, args.join(" ")),
            &output,
        ))
    }
}

fn command(operation: &SystemOperation) -> Result<(&'static str, Vec<String>)> {
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
                return Err(NspawnError::ProtectedImage(image.as_str().into()));
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

        async fn execute(&self, operation: SystemOperation) -> Result<()> {
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
    fn operation_deserialization_rejects_untyped_arguments() {
        let traversal = r#"{"operation":"remove_image","image":"../host"}"#;
        assert!(serde_json::from_str::<SystemOperation>(traversal).is_err());

        let arbitrary = r#"{"operation":"start","machine":"valid","program":"sh"}"#;
        assert!(serde_json::from_str::<SystemOperation>(arbitrary).is_err());
    }

    #[test]
    fn operation_wire_format_contains_no_program_or_argv_fields() {
        let operation = SystemOperation::Kill {
            machine: MachineName::new("test-machine").unwrap(),
            signal: AllowedSignal::Kill,
        };
        let value = serde_json::to_value(operation).unwrap();

        assert_eq!(value["operation"], "kill");
        assert_eq!(value["machine"], "test-machine");
        assert_eq!(value["signal"], "SIGKILL");
        assert!(value.get("program").is_none());
        assert!(value.get("args").is_none());
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
    async fn typed_cli_removal_reports_success() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run().once().returning(|_, _| Ok(success()));

        assert_eq!(
            execute_cli_image_remove_with_runner(ImageName::new("ubuntu").unwrap(), &runner,).await,
            ImageControlOutcome::Removed
        );
    }

    #[tokio::test]
    async fn typed_cli_removal_reports_launch_failure_as_not_attempted() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run().once().returning(|_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "machinectl missing",
            ))
        });

        assert!(matches!(
            execute_cli_image_remove_with_runner(ImageName::new("ubuntu").unwrap(), &runner,).await,
            ImageControlOutcome::NotAttempted { .. }
        ));
    }

    #[tokio::test]
    async fn typed_cli_removal_reports_nonzero_exit_as_failed() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .once()
            .returning(|_, _| Ok(failure("image is busy")));

        assert!(matches!(
            execute_cli_image_remove_with_runner(ImageName::new("ubuntu").unwrap(), &runner,).await,
            ImageControlOutcome::Failed { .. }
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
        assert!(matches!(error, NspawnError::ProtectedImage(_)));
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

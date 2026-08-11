//! Typed host system operations shared by direct and elevated modes.

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{AllowedSignal, ImageEntry, ImageName, MachineName};
use crate::nspawn::sys::command::{CommandRunner, DefaultCommandRunner};
use crate::nspawn::sys::daemon::ElevatedDaemon;
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
        source: MachineName,
        destination: MachineName,
    },
    ReloadDaemon,
}

#[derive(Clone)]
pub struct SystemOperationStore {
    local_runner: Arc<dyn CommandRunner>,
    daemon: Option<Arc<ElevatedDaemon>>,
}

impl SystemOperationStore {
    pub fn new(local_runner: Arc<dyn CommandRunner>, daemon: Option<Arc<ElevatedDaemon>>) -> Self {
        Self {
            local_runner,
            daemon,
        }
    }

    async fn execute(&self, operation: SystemOperation) -> Result<()> {
        if let Some(daemon) = &self.daemon {
            daemon
                .system_operation(operation)
                .await
                .map_err(|error| NspawnError::Io(PathBuf::from("system operation"), error))
        } else {
            execute_system_operation_with_runner(operation, self.local_runner.as_ref()).await
        }
    }

    pub async fn start(&self, name: &str) -> Result<()> {
        self.execute(SystemOperation::Start {
            machine: machine_name(name)?,
        })
        .await
    }

    pub async fn terminate(&self, name: &str) -> Result<()> {
        self.execute(SystemOperation::Terminate {
            machine: machine_name(name)?,
        })
        .await
    }

    pub async fn poweroff(&self, name: &str) -> Result<()> {
        self.execute(SystemOperation::Poweroff {
            machine: machine_name(name)?,
        })
        .await
    }

    pub async fn reboot(&self, name: &str) -> Result<()> {
        self.execute(SystemOperation::Reboot {
            machine: machine_name(name)?,
        })
        .await
    }

    pub async fn enable(&self, name: &str) -> Result<()> {
        self.execute(SystemOperation::Enable {
            machine: machine_name(name)?,
        })
        .await
    }

    pub async fn disable(&self, name: &str) -> Result<()> {
        self.execute(SystemOperation::Disable {
            machine: machine_name(name)?,
        })
        .await
    }

    pub async fn kill(&self, name: &str, signal: AllowedSignal) -> Result<()> {
        self.execute(SystemOperation::Kill {
            machine: machine_name(name)?,
            signal,
        })
        .await
    }

    pub async fn remove_image(&self, name: &str) -> Result<()> {
        if ImageEntry::is_protected_name(name) {
            return Err(NspawnError::Validation(
                "the .host image cannot be removed".into(),
            ));
        }
        self.execute(SystemOperation::RemoveImage {
            image: image_name(name)?,
        })
        .await
    }

    pub async fn clone_image(&self, source: &str, destination: &str) -> Result<()> {
        self.execute(SystemOperation::CloneImage {
            source: machine_name(source)?,
            destination: machine_name(destination)?,
        })
        .await
    }

    pub async fn reload_daemon(&self) -> Result<()> {
        self.execute(SystemOperation::ReloadDaemon).await
    }
}

#[async_trait::async_trait]
impl crate::nspawn::adapters::comm::backend::MachineControl for SystemOperationStore {
    async fn start(&self, name: &str) -> Result<()> {
        Self::start(self, name).await
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        Self::terminate(self, name).await
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        Self::poweroff(self, name).await
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        Self::reboot(self, name).await
    }

    async fn enable(&self, name: &str) -> Result<()> {
        Self::enable(self, name).await
    }

    async fn disable(&self, name: &str) -> Result<()> {
        Self::disable(self, name).await
    }

    async fn kill(&self, name: &str, signal: AllowedSignal) -> Result<()> {
        Self::kill(self, name, signal).await
    }

    async fn remove(&self, name: &str) -> Result<()> {
        Self::remove_image(self, name).await
    }

    async fn reload_daemon(&self) -> Result<()> {
        Self::reload_daemon(self).await
    }
}

impl std::fmt::Debug for SystemOperationStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemOperationStore")
            .field("daemon", &self.daemon)
            .finish_non_exhaustive()
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
    dbus: &crate::nspawn::adapters::comm::dbus::DbusBackend,
    operation: SystemOperation,
) -> Result<()> {
    use crate::nspawn::adapters::comm::backend::ContainerBackend;

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

async fn execute_system_operation_with_runner(
    operation: SystemOperation,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let (program, args) = command(&operation)?;
    let output = runner
        .run(program, args.clone())
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from(program), error))?;
    crate::nspawn::sys::log_output(program, &output);
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
        SystemOperation::Start { machine } => ("machinectl", vec!["start", machine.as_str()]),
        SystemOperation::Terminate { machine } => {
            ("machinectl", vec!["terminate", machine.as_str()])
        }
        SystemOperation::Poweroff { machine } => ("machinectl", vec!["poweroff", machine.as_str()]),
        SystemOperation::Reboot { machine } => ("machinectl", vec!["reboot", machine.as_str()]),
        SystemOperation::Enable { machine } => ("machinectl", vec!["enable", machine.as_str()]),
        SystemOperation::Disable { machine } => ("machinectl", vec!["disable", machine.as_str()]),
        SystemOperation::Kill { machine, signal } => (
            "machinectl",
            vec!["kill", "-s", signal.as_name(), machine.as_str()],
        ),
        SystemOperation::RemoveImage { image } => {
            if ImageEntry::is_protected_name(image.as_str()) {
                return Err(NspawnError::Validation(
                    "the .host image cannot be removed".into(),
                ));
            }
            ("machinectl", vec!["remove", image.as_str()])
        }
        SystemOperation::CloneImage {
            source,
            destination,
        } => (
            "machinectl",
            vec!["clone", source.as_str(), destination.as_str()],
        ),
        SystemOperation::ReloadDaemon => ("systemctl", vec!["daemon-reload"]),
    };
    Ok((
        command.0,
        command.1.into_iter().map(str::to_string).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::sys::command::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;

    fn success() -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: vec![],
            stderr: vec![],
        }
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
                    && args.len() == 2
                    && args[0] == "remove"
                    && args[1] == ".oci-sha256:abc"
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
        assert!(matches!(error, NspawnError::Validation(_)));
    }
}

//! A single native request between readiness observation and submission.
//! Preparation starts no guest process, so dropping it needs no remote cancel.
//! Once opened, the PTY/session owner takes over resource cleanup.

use crate::adapters::runtime::systemd_tools;
use crate::adapters::session::io::MachinePty;
use crate::application::sessions::execution::{
    GuestExecutionError, GuestExecutionFailure, GuestExecutionRequest, PreparedGuestExecution,
};
use crate::application::sessions::{ObservedMachineInstance, SessionError, TerminalSessionHandle};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionSize};
use portable_pty::CommandBuilder;
use std::path::Path;
use std::time::Duration;

use super::native::map_execution_error;
use super::{NativeExecution, NativeSession};

/// Deliberately not Clone: opening consumes this request and its observation.
pub(crate) struct PreparedNativeExecution {
    execution: NativeExecution,
    request: GuestExecutionRequest,
    instance: Option<ObservedMachineInstance>,
    deadline: tokio::time::Instant,
}

impl PreparedNativeExecution {
    pub(super) fn new(
        execution: NativeExecution,
        request: GuestExecutionRequest,
        instance: Option<ObservedMachineInstance>,
        deadline: tokio::time::Instant,
    ) -> Self {
        Self {
            execution,
            request,
            instance,
            deadline,
        }
    }
}

#[async_trait::async_trait]
impl PreparedGuestExecution for PreparedNativeExecution {
    type Session = NativeSession;

    async fn open(self) -> Result<NativeSession, GuestExecutionError> {
        let machine = self.request.target().machine();
        verify_deadline(Some(self.deadline))?;
        match &self.execution {
            NativeExecution::Dbus(dbus) => {
                if let Some(expected) = self.instance {
                    let current = self
                        .execution
                        .instance_before_deadline(
                            machine,
                            (tokio::time::Instant::now() + Duration::from_secs(1))
                                .min(self.deadline),
                        )
                        .await
                        .map_err(|observation| {
                            GuestExecutionError::new(
                                GuestExecutionFailure::NotAttempted,
                                SessionError::new(format!(
                                    "cannot recheck prepared execution in {machine}: {observation}"
                                )),
                            )
                        })?;
                    verify_instance(machine, expected, current).map_err(|error| {
                        GuestExecutionError::new(GuestExecutionFailure::NotAttempted, error)
                    })?;
                }
                // machine1 resolves a name, so this check cannot make the
                // lookup atomic with submission. Do not retry a submitted
                // request through a different route.
                verify_deadline(Some(self.deadline))?;
                let master = dbus
                    .open_guest_session(&self.request, self.deadline)
                    .await
                    .map_err(|error| {
                        map_execution_error("execute selected guest process", error)
                    })?;
                Ok(NativeSession::Pty(MachinePty {
                    master,
                    machine_removed: None,
                }))
            }
            NativeExecution::SystemdTools => {
                Ok(NativeSession::Command(PreparedNativeCommand::new(
                    machine.clone(),
                    self.instance,
                    Some(self.deadline),
                    systemd_tools::session::guest_session_command(&self.request),
                )))
            }
        }
    }

    async fn open_local(
        self,
        id: SessionId,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, GuestExecutionError> {
        self.open().await?.into_local(id, size)
    }
}

/// Retains tools-route evidence until local or daemon delivery spawns the
/// command. Constructing machinectl argv does not submit a guest process.
pub(crate) struct PreparedNativeCommand {
    machine: MachineName,
    instance: Option<ObservedMachineInstance>,
    deadline: Option<tokio::time::Instant>,
    command: CommandBuilder,
}

impl PreparedNativeCommand {
    pub(super) fn new(
        machine: MachineName,
        instance: Option<ObservedMachineInstance>,
        deadline: Option<tokio::time::Instant>,
        command: CommandBuilder,
    ) -> Self {
        Self {
            machine,
            instance,
            deadline,
            command,
        }
    }

    pub(crate) fn into_command(self) -> Result<CommandBuilder, GuestExecutionError> {
        verify_deadline(self.deadline)?;
        let registration = crate::paths::runtime_machine_state(self.machine.as_str());
        self.into_command_at(&registration)
            .map_err(|error| GuestExecutionError::new(GuestExecutionFailure::NotAttempted, error))
    }

    fn into_command_at(self, registration: &Path) -> Result<CommandBuilder, SessionError> {
        if let Some(expected) = self.instance {
            let current = crate::adapters::runtime::registration::machine_instance_at(
                registration,
                self.machine.as_str(),
            )
            .map_err(|error| {
                SessionError::new(format!(
                    "cannot recheck prepared execution in {}: {error}",
                    self.machine
                ))
            })?;
            verify_instance(&self.machine, expected, current)?;
        }
        // The native client still resolves the name after this check.
        Ok(self.command)
    }
}

fn verify_deadline(deadline: Option<tokio::time::Instant>) -> Result<(), GuestExecutionError> {
    if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
        Err(GuestExecutionError::new(
            GuestExecutionFailure::NotAttempted,
            SessionError::new(
                "guest execution deadline expired before submission; retry the request",
            ),
        ))
    } else {
        Ok(())
    }
}

fn verify_instance(
    machine: &MachineName,
    expected: ObservedMachineInstance,
    current: ObservedMachineInstance,
) -> Result<(), SessionError> {
    match expected.compare(current) {
        Some(true) => Ok(()),
        Some(false) => Err(SessionError::new(format!(
            "machine {machine} changed instance after guest execution preparation; retry the request"
        ))),
        None => Err(SessionError::new(format!(
            "cannot confirm the prepared machine instance in {machine}; retry the request"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::ObservedNamespaceIdentity;
    use std::num::NonZeroU64;

    #[tokio::test]
    async fn expired_tools_preparation_is_not_submitted() {
        let command = PreparedNativeCommand::new(
            MachineName::new("test-machine").unwrap(),
            None,
            Some(tokio::time::Instant::now() - Duration::from_secs(1)),
            CommandBuilder::new("machinectl"),
        );
        let error = command.into_command().unwrap_err();
        assert_eq!(error.failure, GuestExecutionFailure::NotAttempted);
        assert!(error
            .to_string()
            .contains("deadline expired before submission"));
    }

    #[test]
    fn tools_submission_rechecks_registration_and_preserves_the_command() {
        let directory = tempfile::tempdir().unwrap();
        let registration = directory.path().join("test-machine");
        std::fs::write(
            &registration,
            format!("NAME=test-machine\nLEADER={}\n", std::process::id()),
        )
        .unwrap();
        let instance = crate::adapters::runtime::registration::machine_instance_at(
            &registration,
            "test-machine",
        )
        .unwrap();
        let prepared = |instance| {
            PreparedNativeCommand::new(
                MachineName::new("test-machine").unwrap(),
                Some(instance),
                None,
                CommandBuilder::new("machinectl"),
            )
        };

        let command = prepared(instance).into_command_at(&registration).unwrap();
        assert_eq!(
            command.get_argv().as_slice(),
            [std::ffi::OsString::from("machinectl")]
        );

        let stale = ObservedMachineInstance::from_process(
            instance.leader_pid(),
            NonZeroU64::new(instance.leader_start_time().unwrap().get() + 1).unwrap(),
            instance.namespaces(),
        );
        let error = prepared(stale).into_command_at(&registration).unwrap_err();
        assert!(error.to_string().contains("changed instance"));

        std::fs::remove_file(&registration).unwrap();
        let error = prepared(instance)
            .into_command_at(&registration)
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot recheck prepared execution"));
    }

    #[test]
    fn login_prompt_does_not_require_selected_user_instance_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let command = PreparedNativeCommand::new(
            MachineName::new("test-machine").unwrap(),
            None,
            None,
            CommandBuilder::new("machinectl"),
        );
        assert!(command
            .into_command_at(&directory.path().join("absent-registration"))
            .is_ok());
    }

    #[test]
    fn submission_rejects_restart_and_changed_namespace_evidence() {
        let machine = MachineName::new("test-machine").unwrap();
        let pid_namespace = ObservedNamespaceIdentity::new(4, 100);
        let user_namespace = ObservedNamespaceIdentity::new(4, 101);
        let expected = ObservedMachineInstance::from_process(
            42,
            NonZeroU64::new(1000).unwrap(),
            Some((pid_namespace, user_namespace)),
        );
        assert!(verify_instance(&machine, expected, expected).is_ok());
        for current in [
            ObservedMachineInstance::from_process(43, NonZeroU64::new(1000).unwrap(), None),
            ObservedMachineInstance::from_process(42, NonZeroU64::new(1001).unwrap(), None),
            ObservedMachineInstance::from_process(
                42,
                NonZeroU64::new(1000).unwrap(),
                Some((ObservedNamespaceIdentity::new(4, 102), user_namespace)),
            ),
        ] {
            assert!(verify_instance(&machine, expected, current)
                .unwrap_err()
                .to_string()
                .contains("changed instance"));
        }
    }
}

//! Runtime composition for the machine lifecycle vertical slice.

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::lifecycle::error::map_machine_control_error;
use crate::adapters::process::CommandRunner;
use crate::adapters::runtime::dbus::DbusBackend;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::system_operation::{
    execute_dbus_system_operation, execute_system_operation_with_runner, SystemOperation,
    SystemOperationStore,
};
use crate::application::machine_lifecycle::{
    MachineControl, MachineControlOutcome, MachineControlTransport, MachineLifecycleService,
    MachineObservation, MachinePreparationError, MachineRuntimeAction,
    MachineRuntimeControlRequest, MachineStartDiagnostics, MachineStartPreparation,
    NspawnLaunchRequest, NspawnUnitAction, NspawnUnitControlRequest, RoutedMachineControlOutcome,
    StartFailureEvidence,
};
use crate::application::operations::{ExecutionRoute, RouteFallback};
use crate::application::runtime::RuntimeResult;
use crate::application::{OperationRegistry, RuntimeCatalog};
use crate::domain::inspection::MachineProperties;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageName, MachineEntry};
use crate::nspawn::errors::NspawnError;
use std::sync::Arc;

pub(crate) struct MachineLifecycleAdapters {
    pub(crate) local_cmd: Arc<dyn CommandRunner>,
    pub(crate) system_operations: SystemOperationStore,
    pub(crate) nspawn: crate::adapters::config::NspawnConfigStore,
    pub(crate) systemd_unit: crate::adapters::config::SystemdUnitStore,
    pub(crate) nvidia_state: crate::adapters::platform::nvidia::NvidiaStateStore,
    pub(crate) rootfs: crate::adapters::rootfs::RootfsStore,
}

pub(crate) enum MachineLifecycleRoute {
    DirectDbus,
    LocalCli,
    Elevated {
        daemon: Arc<ElevatedDaemon>,
        transport: MachineControlTransport,
    },
}

pub(crate) fn compose_machine_lifecycle(
    runtime: Arc<RuntimeCatalog>,
    registry: Arc<OperationRegistry>,
    route: MachineLifecycleRoute,
    adapters: MachineLifecycleAdapters,
) -> Arc<MachineLifecycleService> {
    let MachineLifecycleAdapters {
        local_cmd,
        system_operations,
        nspawn,
        systemd_unit,
        nvidia_state,
        rootfs,
    } = adapters;
    let control: Arc<dyn MachineControl> = Arc::new(RoutedMachineControl {
        route: match route {
            MachineLifecycleRoute::DirectDbus => MachineControlRoute::DirectDbus {
                dbus: DbusBackend::new(),
                fallback_runner: local_cmd.clone(),
            },
            MachineLifecycleRoute::LocalCli => MachineControlRoute::LocalCli {
                runner: local_cmd.clone(),
            },
            MachineLifecycleRoute::Elevated { daemon, transport } => {
                MachineControlRoute::Daemon { daemon, transport }
            }
        },
    });
    let preparation: Arc<dyn MachineStartPreparation> = Arc::new(StoreStartPreparation {
        nspawn,
        systemd_unit,
        nvidia_state,
        rootfs,
        system_operations,
        runtime: runtime.clone(),
    });
    let observation: Arc<dyn MachineObservation> = Arc::new(CatalogMachineObservation {
        runtime: runtime.clone(),
    });
    let diagnostics: Arc<dyn MachineStartDiagnostics> =
        Arc::new(LocalStartDiagnostics { runner: local_cmd });
    Arc::new(MachineLifecycleService::new(
        control,
        preparation,
        observation,
        diagnostics,
        registry,
    ))
}

enum MachineControlRoute {
    DirectDbus {
        dbus: DbusBackend,
        fallback_runner: Arc<dyn CommandRunner>,
    },
    LocalCli {
        runner: Arc<dyn CommandRunner>,
    },
    Daemon {
        daemon: Arc<ElevatedDaemon>,
        transport: MachineControlTransport,
    },
}

struct RoutedMachineControl {
    route: MachineControlRoute,
}

#[derive(Clone, Debug)]
enum MachineControlIntent {
    Launch { image: ImageName },
    Runtime(MachineRuntimeAction),
    Unit(NspawnUnitAction),
}

#[async_trait::async_trait]
impl MachineControl for RoutedMachineControl {
    async fn launch(
        &self,
        image: &ImageName,
        machine: &MachineName,
    ) -> RoutedMachineControlOutcome {
        self.execute(
            machine,
            MachineControlIntent::Launch {
                image: image.clone(),
            },
        )
        .await
    }

    async fn execute_runtime(
        &self,
        machine: &MachineName,
        action: MachineRuntimeAction,
    ) -> RoutedMachineControlOutcome {
        self.execute(machine, MachineControlIntent::Runtime(action))
            .await
    }

    async fn execute_unit(
        &self,
        machine: &MachineName,
        action: NspawnUnitAction,
    ) -> RoutedMachineControlOutcome {
        self.execute(machine, MachineControlIntent::Unit(action))
            .await
    }
}

impl RoutedMachineControl {
    async fn execute(
        &self,
        machine: &MachineName,
        intent: MachineControlIntent,
    ) -> RoutedMachineControlOutcome {
        match &self.route {
            MachineControlRoute::DirectDbus {
                dbus,
                fallback_runner,
            } => {
                let dbus_outcome = if RuntimeSource::is_available(dbus).await {
                    execute_dbus_machine_control(dbus, machine.clone(), intent.clone()).await
                } else {
                    MachineControlOutcome::NotAttempted {
                        reason: "D-Bus backend is unavailable".into(),
                    }
                };
                match dbus_outcome {
                    MachineControlOutcome::NotAttempted { reason } => {
                        let outcome = execute_cli_machine_control_with_runner(
                            machine.clone(),
                            intent,
                            fallback_runner.as_ref(),
                        )
                        .await;
                        RoutedMachineControlOutcome {
                            outcome,
                            route: ExecutionRoute::LocalCli,
                            fallback: Some(RouteFallback {
                                from: ExecutionRoute::DirectDbus,
                                to: ExecutionRoute::LocalCli,
                                reason,
                            }),
                        }
                    }
                    outcome => RoutedMachineControlOutcome {
                        outcome,
                        route: ExecutionRoute::DirectDbus,
                        fallback: None,
                    },
                }
            }
            MachineControlRoute::LocalCli { runner } => RoutedMachineControlOutcome {
                outcome: execute_cli_machine_control_with_runner(
                    machine.clone(),
                    intent,
                    runner.as_ref(),
                )
                .await,
                route: ExecutionRoute::LocalCli,
                fallback: None,
            },
            MachineControlRoute::Daemon { daemon, transport } => {
                let outcome = match execute_daemon_machine_control(
                    daemon,
                    machine.clone(),
                    intent.clone(),
                    *transport,
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => MachineControlOutcome::OutcomeUnknown {
                        reason: format!("daemon response was lost: {error}"),
                    },
                };
                if *transport == MachineControlTransport::Dbus {
                    if let MachineControlOutcome::NotAttempted { reason } = outcome {
                        let fallback_outcome = match execute_daemon_machine_control(
                            daemon,
                            machine.clone(),
                            intent,
                            MachineControlTransport::Cli,
                        )
                        .await
                        {
                            Ok(outcome) => outcome,
                            Err(error) => MachineControlOutcome::OutcomeUnknown {
                                reason: format!("daemon response was lost: {error}"),
                            },
                        };
                        return RoutedMachineControlOutcome {
                            outcome: fallback_outcome,
                            route: ExecutionRoute::ElevatedCli,
                            fallback: Some(RouteFallback {
                                from: ExecutionRoute::ElevatedDbus,
                                to: ExecutionRoute::ElevatedCli,
                                reason,
                            }),
                        };
                    }
                }
                RoutedMachineControlOutcome {
                    outcome,
                    route: match transport {
                        MachineControlTransport::Dbus => ExecutionRoute::ElevatedDbus,
                        MachineControlTransport::Cli => ExecutionRoute::ElevatedCli,
                    },
                    fallback: None,
                }
            }
        }
    }
}

async fn execute_daemon_machine_control(
    daemon: &ElevatedDaemon,
    machine: MachineName,
    intent: MachineControlIntent,
    transport: MachineControlTransport,
) -> std::io::Result<MachineControlOutcome> {
    match intent {
        MachineControlIntent::Launch { image } => {
            daemon
                .nspawn_launch(NspawnLaunchRequest {
                    image,
                    machine,
                    transport,
                })
                .await
        }
        MachineControlIntent::Runtime(action) => {
            daemon
                .machine_runtime_control(MachineRuntimeControlRequest {
                    machine,
                    action,
                    transport,
                })
                .await
        }
        MachineControlIntent::Unit(action) => {
            daemon
                .nspawn_unit_control(NspawnUnitControlRequest {
                    machine,
                    action,
                    transport,
                })
                .await
        }
    }
}

async fn execute_dbus_machine_control(
    dbus: &DbusBackend,
    machine: MachineName,
    intent: MachineControlIntent,
) -> MachineControlOutcome {
    match execute_dbus_system_operation(dbus, system_operation(machine, intent)).await {
        Ok(()) => MachineControlOutcome::Succeeded,
        Err(error) => map_machine_control_error(error),
    }
}

async fn execute_cli_machine_control(
    machine: MachineName,
    intent: MachineControlIntent,
) -> MachineControlOutcome {
    execute_cli_machine_control_with_runner(
        machine,
        intent,
        &crate::adapters::process::DefaultCommandRunner,
    )
    .await
}

pub(crate) async fn execute_cli_nspawn_launch(
    image: ImageName,
    machine: MachineName,
) -> MachineControlOutcome {
    if image.as_str() != machine.as_str() {
        return MachineControlOutcome::Rejected {
            rejection: crate::application::machine_lifecycle::MachineRejection::InvalidTarget,
            reason: "nspawn launch currently requires matching image and machine names".into(),
        };
    }
    execute_cli_machine_control(machine, MachineControlIntent::Launch { image }).await
}

pub(crate) async fn execute_cli_machine_runtime(
    machine: MachineName,
    action: MachineRuntimeAction,
) -> MachineControlOutcome {
    execute_cli_machine_control(machine, MachineControlIntent::Runtime(action)).await
}

pub(crate) async fn execute_cli_nspawn_unit(
    machine: MachineName,
    action: NspawnUnitAction,
) -> MachineControlOutcome {
    execute_cli_machine_control(machine, MachineControlIntent::Unit(action)).await
}

async fn execute_cli_machine_control_with_runner(
    machine: MachineName,
    intent: MachineControlIntent,
    runner: &dyn CommandRunner,
) -> MachineControlOutcome {
    match execute_system_operation_with_runner(system_operation(machine, intent), runner).await {
        Ok(()) => MachineControlOutcome::Succeeded,
        Err(NspawnError::Io(_, error)) => MachineControlOutcome::NotAttempted {
            reason: format!("failed to launch machine control command: {error}"),
        },
        Err(error) => map_machine_control_error(error),
    }
}

fn system_operation(machine: MachineName, intent: MachineControlIntent) -> SystemOperation {
    match intent {
        MachineControlIntent::Launch { .. } => SystemOperation::Start { machine },
        MachineControlIntent::Runtime(MachineRuntimeAction::Terminate) => {
            SystemOperation::Terminate { machine }
        }
        MachineControlIntent::Runtime(MachineRuntimeAction::Poweroff) => {
            SystemOperation::Poweroff { machine }
        }
        MachineControlIntent::Runtime(MachineRuntimeAction::Reboot) => {
            SystemOperation::Reboot { machine }
        }
        MachineControlIntent::Runtime(MachineRuntimeAction::Kill { signal }) => {
            SystemOperation::Kill { machine, signal }
        }
        MachineControlIntent::Unit(NspawnUnitAction::Enable) => SystemOperation::Enable { machine },
        MachineControlIntent::Unit(NspawnUnitAction::Disable) => {
            SystemOperation::Disable { machine }
        }
    }
}

struct StoreStartPreparation {
    nspawn: crate::adapters::config::NspawnConfigStore,
    systemd_unit: crate::adapters::config::SystemdUnitStore,
    nvidia_state: crate::adapters::platform::nvidia::NvidiaStateStore,
    rootfs: crate::adapters::rootfs::RootfsStore,
    system_operations: SystemOperationStore,
    runtime: Arc<RuntimeCatalog>,
}

#[async_trait::async_trait]
impl MachineStartPreparation for StoreStartPreparation {
    async fn prepare(&self, machine: &MachineName) -> Result<(), MachinePreparationError> {
        let result = crate::adapters::platform::nvidia::ensure_gpu_passthrough(
            machine.as_str(),
            &self.nspawn,
            &self.systemd_unit,
            &self.nvidia_state,
            &self.rootfs,
        )
        .await;
        self.runtime.invalidate();
        result.map_err(map_machine_preparation_error)?;
        if let Err(error) = self.system_operations.reload_daemon().await {
            log::warn!(
                "systemd daemon reload after pre-start reconciliation failed for {}: {}",
                machine,
                error
            );
        }
        Ok(())
    }
}

fn map_machine_preparation_error(error: NspawnError) -> MachinePreparationError {
    let permission_denied =
        error.is_polkit_rejection() || matches!(&error, NspawnError::PermissionDenied);
    let invalid_configuration = matches!(
        &error,
        NspawnError::Validation(_) | NspawnError::InvalidConfig(_)
    );
    let message = error.to_string();
    if permission_denied {
        MachinePreparationError::permission_denied(message)
    } else if invalid_configuration {
        MachinePreparationError::invalid_configuration(message)
    } else {
        MachinePreparationError::failed(message)
    }
}

struct CatalogMachineObservation {
    runtime: Arc<RuntimeCatalog>,
}

#[async_trait::async_trait]
impl MachineObservation for CatalogMachineObservation {
    async fn inspect(
        &self,
        machine: &MachineName,
        entry: &MachineEntry,
    ) -> RuntimeResult<MachineProperties> {
        self.runtime
            .inspect(machine.as_str(), entry)
            .await
            .map(|query| query.value)
    }

    fn invalidate(&self) {
        self.runtime.invalidate();
    }
}

struct LocalStartDiagnostics {
    runner: Arc<dyn CommandRunner>,
}

#[async_trait::async_trait]
impl MachineStartDiagnostics for LocalStartDiagnostics {
    async fn collect(
        &self,
        machine: &MachineName,
        invocation_id: Option<String>,
        started_epoch: u64,
    ) -> StartFailureEvidence {
        let unit = machine.systemd_nspawn_unit();
        let (mut args, selector_display) = if let Some(invocation_id) = invocation_id.as_deref() {
            (
                vec![format!("_SYSTEMD_INVOCATION_ID={invocation_id}")],
                format!("_SYSTEMD_INVOCATION_ID={invocation_id}"),
            )
        } else {
            (
                vec![
                    "-u".into(),
                    unit,
                    "--since".into(),
                    format!("@{started_epoch}"),
                ],
                format!(
                    "-u {} --since @{started_epoch}",
                    machine.systemd_nspawn_unit()
                ),
            )
        };
        args.extend([
            "-n".into(),
            "40".into(),
            "--no-pager".into(),
            "--quiet".into(),
            "--output=short".into(),
        ]);
        let journal = self
            .runner
            .run("journalctl", args)
            .await
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|output| !output.is_empty());
        StartFailureEvidence {
            journal_command: format!("journalctl {selector_display} --no-pager"),
            journal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;

    fn success() -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: vec![],
            stderr: vec![],
        }
    }

    #[tokio::test]
    async fn cli_control_uses_fixed_typed_command() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .withf(|program, args| {
                program == "machinectl"
                    && args.iter().map(String::as_str).eq(["--", "start", "test"])
            })
            .returning(|_, _| Ok(success()));

        let outcome = execute_cli_machine_control_with_runner(
            MachineName::new("test").unwrap(),
            MachineControlIntent::Launch {
                image: ImageName::new("test").unwrap(),
            },
            &runner,
        )
        .await;

        assert_eq!(outcome, MachineControlOutcome::Succeeded);
    }

    #[tokio::test]
    async fn cli_launch_failure_is_explicitly_not_attempted() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run().returning(|_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "machinectl missing",
            ))
        });

        let outcome = execute_cli_machine_control_with_runner(
            MachineName::new("test").unwrap(),
            MachineControlIntent::Launch {
                image: ImageName::new("test").unwrap(),
            },
            &runner,
        )
        .await;

        assert!(matches!(
            outcome,
            MachineControlOutcome::NotAttempted { .. }
        ));
    }

    #[test]
    fn preparation_errors_keep_host_semantics_at_the_adapter_boundary() {
        let permission = map_machine_preparation_error(NspawnError::PermissionDenied);
        assert!(permission.is_permission_denied());
        assert!(!permission.is_invalid_configuration());

        let invalid = map_machine_preparation_error(NspawnError::InvalidConfig("bad unit".into()));
        assert!(invalid.is_invalid_configuration());
        assert!(!invalid.is_permission_denied());

        let failed = map_machine_preparation_error(NspawnError::Runtime("probe failed".into()));
        assert!(!failed.is_invalid_configuration());
        assert!(!failed.is_permission_denied());
        assert_eq!(failed.to_string(), "Runtime error: probe failed");
    }
}

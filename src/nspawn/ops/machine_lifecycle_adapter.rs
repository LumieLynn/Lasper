//! Runtime composition for the machine lifecycle vertical slice.

use super::machine_lifecycle::{
    map_native_error, MachineAction, MachineControl, MachineControlOutcome, MachineControlRequest,
    MachineControlTransport, MachineLifecycleService, MachineObservation, MachineStartDiagnostics,
    MachineStartPreparation, RoutedMachineControlOutcome, StartFailureEvidence,
};
use super::route::{ExecutionRoute, RouteFallback};
use super::{OperationRegistry, PermissionLevel, RuntimeCatalog};
use crate::nspawn::adapters::comm::backend::RuntimeSource;
use crate::nspawn::adapters::comm::dbus::DbusBackend;
use crate::nspawn::errors::NspawnError;
use crate::nspawn::models::{ContainerEntry, MachineName, MachineProperties};
use crate::nspawn::ops::system_operation::{
    execute_dbus_system_operation, execute_system_operation_with_runner, SystemOperation,
};
use crate::nspawn::sys::command::CommandRunner;
use crate::nspawn::sys::daemon::ElevatedDaemon;
use crate::nspawn::sys::ExecutionContext;
use std::sync::Arc;

pub(crate) fn compose_machine_lifecycle(
    runtime: Arc<RuntimeCatalog>,
    registry: Arc<OperationRegistry>,
    level: PermissionLevel,
    cli_mode: bool,
    exec_ctx: &ExecutionContext,
) -> Arc<MachineLifecycleService> {
    let control: Arc<dyn MachineControl> = Arc::new(RoutedMachineControl {
        route: match select_machine_control_route(level, cli_mode) {
            MachineControlRouteKind::DirectDbus => MachineControlRoute::DirectDbus {
                dbus: DbusBackend::new(),
                fallback_runner: exec_ctx.local_cmd.clone(),
            },
            MachineControlRouteKind::LocalCli => MachineControlRoute::LocalCli {
                runner: exec_ctx.local_cmd.clone(),
            },
            MachineControlRouteKind::Daemon(transport) => MachineControlRoute::Daemon {
                daemon: exec_ctx
                    .daemon_ref()
                    .cloned()
                    .expect("elevated machine lifecycle requires daemon"),
                transport,
            },
        },
    });
    let preparation: Arc<dyn MachineStartPreparation> = Arc::new(StoreStartPreparation {
        nspawn: exec_ctx.nspawn.clone(),
        systemd_unit: exec_ctx.systemd_unit.clone(),
        nvidia_state: exec_ctx.nvidia_state.clone(),
        rootfs: exec_ctx.rootfs.clone(),
        system_operations: exec_ctx.system_operations.clone(),
        runtime: runtime.clone(),
    });
    let observation: Arc<dyn MachineObservation> = Arc::new(CatalogMachineObservation {
        runtime: runtime.clone(),
    });
    let diagnostics: Arc<dyn MachineStartDiagnostics> = Arc::new(LocalStartDiagnostics {
        runner: exec_ctx.local_cmd.clone(),
    });
    Arc::new(MachineLifecycleService::new(
        control,
        preparation,
        observation,
        diagnostics,
        registry,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MachineControlRouteKind {
    DirectDbus,
    LocalCli,
    Daemon(MachineControlTransport),
}

fn select_machine_control_route(level: PermissionLevel, cli_mode: bool) -> MachineControlRouteKind {
    match (level, cli_mode) {
        (PermissionLevel::Elevated, false) => {
            MachineControlRouteKind::Daemon(MachineControlTransport::Dbus)
        }
        (PermissionLevel::Elevated, true) => {
            MachineControlRouteKind::Daemon(MachineControlTransport::Cli)
        }
        (PermissionLevel::User | PermissionLevel::Root, false) => {
            MachineControlRouteKind::DirectDbus
        }
        (PermissionLevel::User | PermissionLevel::Root, true) => MachineControlRouteKind::LocalCli,
    }
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

#[async_trait::async_trait]
impl MachineControl for RoutedMachineControl {
    async fn execute(
        &self,
        machine: &MachineName,
        action: MachineAction,
    ) -> RoutedMachineControlOutcome {
        match &self.route {
            MachineControlRoute::DirectDbus {
                dbus,
                fallback_runner,
            } => {
                let dbus_outcome = if RuntimeSource::is_available(dbus).await {
                    execute_dbus_machine_control(dbus, machine.clone(), action).await
                } else {
                    MachineControlOutcome::NotAttempted {
                        reason: "D-Bus backend is unavailable".into(),
                    }
                };
                match dbus_outcome {
                    MachineControlOutcome::NotAttempted { reason } => {
                        let outcome = execute_cli_machine_control_with_runner(
                            machine.clone(),
                            action,
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
                    action,
                    runner.as_ref(),
                )
                .await,
                route: ExecutionRoute::LocalCli,
                fallback: None,
            },
            MachineControlRoute::Daemon { daemon, transport } => {
                let request = MachineControlRequest {
                    machine: machine.clone(),
                    action,
                    transport: *transport,
                };
                let outcome = match daemon.machine_control(request).await {
                    Ok(outcome) => outcome,
                    Err(error) => MachineControlOutcome::OutcomeUnknown {
                        reason: format!("daemon response was lost: {error}"),
                    },
                };
                if *transport == MachineControlTransport::Dbus {
                    if let MachineControlOutcome::NotAttempted { reason } = outcome {
                        let fallback_request = MachineControlRequest {
                            machine: machine.clone(),
                            action,
                            transport: MachineControlTransport::Cli,
                        };
                        let fallback_outcome = match daemon.machine_control(fallback_request).await
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

pub(crate) async fn execute_dbus_machine_control(
    dbus: &DbusBackend,
    machine: MachineName,
    action: MachineAction,
) -> MachineControlOutcome {
    match execute_dbus_system_operation(dbus, system_operation(machine, action)).await {
        Ok(()) => MachineControlOutcome::Succeeded,
        Err(error) => map_native_error(error),
    }
}

pub(crate) async fn execute_cli_machine_control(
    machine: MachineName,
    action: MachineAction,
) -> MachineControlOutcome {
    execute_cli_machine_control_with_runner(
        machine,
        action,
        &crate::nspawn::sys::command::DefaultCommandRunner,
    )
    .await
}

async fn execute_cli_machine_control_with_runner(
    machine: MachineName,
    action: MachineAction,
    runner: &dyn CommandRunner,
) -> MachineControlOutcome {
    match execute_system_operation_with_runner(system_operation(machine, action), runner).await {
        Ok(()) => MachineControlOutcome::Succeeded,
        Err(NspawnError::Io(_, error)) => MachineControlOutcome::NotAttempted {
            reason: format!("failed to launch machine control command: {error}"),
        },
        Err(error) => map_native_error(error),
    }
}

fn system_operation(machine: MachineName, action: MachineAction) -> SystemOperation {
    match action {
        MachineAction::Start => SystemOperation::Start { machine },
        MachineAction::Terminate => SystemOperation::Terminate { machine },
        MachineAction::Poweroff => SystemOperation::Poweroff { machine },
        MachineAction::Reboot => SystemOperation::Reboot { machine },
        MachineAction::Enable => SystemOperation::Enable { machine },
        MachineAction::Disable => SystemOperation::Disable { machine },
        MachineAction::Kill { signal } => SystemOperation::Kill { machine, signal },
    }
}

struct StoreStartPreparation {
    nspawn: crate::nspawn::adapters::config::NspawnConfigStore,
    systemd_unit: crate::nspawn::adapters::config::SystemdUnitStore,
    nvidia_state: crate::nspawn::platform::nvidia::NvidiaStateStore,
    rootfs: crate::nspawn::adapters::rootfs::RootfsStore,
    system_operations: super::SystemOperationStore,
    runtime: Arc<RuntimeCatalog>,
}

#[async_trait::async_trait]
impl MachineStartPreparation for StoreStartPreparation {
    async fn prepare(&self, machine: &MachineName) -> Result<(), String> {
        let result = crate::nspawn::platform::nvidia::ensure_gpu_passthrough(
            machine.as_str(),
            &self.nspawn,
            &self.systemd_unit,
            &self.nvidia_state,
            &self.rootfs,
        )
        .await;
        self.runtime.invalidate();
        result.map_err(|error| error.to_string())?;
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

struct CatalogMachineObservation {
    runtime: Arc<RuntimeCatalog>,
}

#[async_trait::async_trait]
impl MachineObservation for CatalogMachineObservation {
    async fn inspect(
        &self,
        machine: &MachineName,
        entry: &ContainerEntry,
    ) -> Result<MachineProperties, String> {
        self.runtime
            .inspect(machine.as_str(), entry)
            .await
            .map(|query| query.value)
            .map_err(|error| error.to_string())
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
    fn composition_selects_each_execution_route_once() {
        assert_eq!(
            select_machine_control_route(PermissionLevel::User, false),
            MachineControlRouteKind::DirectDbus
        );
        assert_eq!(
            select_machine_control_route(PermissionLevel::Root, true),
            MachineControlRouteKind::LocalCli
        );
        assert_eq!(
            select_machine_control_route(PermissionLevel::Elevated, false),
            MachineControlRouteKind::Daemon(MachineControlTransport::Dbus)
        );
        assert_eq!(
            select_machine_control_route(PermissionLevel::Elevated, true),
            MachineControlRouteKind::Daemon(MachineControlTransport::Cli)
        );
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
            MachineAction::Start,
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
            MachineAction::Start,
            &runner,
        )
        .await;

        assert!(matches!(
            outcome,
            MachineControlOutcome::NotAttempted { .. }
        ));
    }
}

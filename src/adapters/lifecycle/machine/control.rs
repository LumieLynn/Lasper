//! Machine control routing and host control executors.

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::lifecycle::error::map_system_operation_machine_error;
use crate::adapters::process::CommandRunner;
use crate::adapters::runtime::dbus::DbusBackend;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::system_operation::{
    execute_dbus_system_operation_with_runner, execute_system_operation_with_runner,
    SystemOperation, SystemOperationError,
};
use crate::application::machine_lifecycle::{
    MachineControl, MachineControlOutcome, MachineControlTransport, MachineRuntimeAction,
    MachineRuntimeControlRequest, NspawnLaunchRequest, NspawnUnitAction, NspawnUnitControlRequest,
    RoutedMachineControlOutcome,
};
use crate::application::operations::{ExecutionRoute, RouteFallback};
use crate::domain::machine::MachineName;
use crate::domain::runtime::ImageName;
use std::sync::Arc;

pub(super) enum MachineControlRoute {
    DirectDbus {
        dbus: DbusBackend,
        local_runner: Arc<dyn CommandRunner>,
    },
    LocalSystemdTools {
        runner: Arc<dyn CommandRunner>,
    },
    Daemon {
        daemon: Arc<ElevatedDaemon>,
        transport: MachineControlTransport,
    },
}

pub(super) struct RoutedMachineControl {
    pub(super) route: MachineControlRoute,
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
            MachineControlRoute::DirectDbus { dbus, local_runner } => {
                let dbus_outcome = if RuntimeSource::is_available(dbus).await {
                    execute_dbus_machine_control(
                        dbus,
                        machine.clone(),
                        intent.clone(),
                        local_runner.as_ref(),
                    )
                    .await
                } else {
                    MachineControlOutcome::NotAttempted {
                        reason: "D-Bus backend is unavailable".into(),
                    }
                };
                match dbus_outcome {
                    MachineControlOutcome::NotAttempted { reason } => {
                        let outcome = execute_systemd_tools_machine_control_with_runner(
                            machine.clone(),
                            intent,
                            local_runner.as_ref(),
                        )
                        .await;
                        RoutedMachineControlOutcome {
                            outcome,
                            route: ExecutionRoute::LocalSystemdTools,
                            fallback: Some(RouteFallback {
                                from: ExecutionRoute::DirectDbus,
                                to: ExecutionRoute::LocalSystemdTools,
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
            MachineControlRoute::LocalSystemdTools { runner } => RoutedMachineControlOutcome {
                outcome: execute_systemd_tools_machine_control_with_runner(
                    machine.clone(),
                    intent,
                    runner.as_ref(),
                )
                .await,
                route: ExecutionRoute::LocalSystemdTools,
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
                            MachineControlTransport::SystemdTools,
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
                            route: ExecutionRoute::ElevatedSystemdTools,
                            fallback: Some(RouteFallback {
                                from: ExecutionRoute::ElevatedDbus,
                                to: ExecutionRoute::ElevatedSystemdTools,
                                reason,
                            }),
                        };
                    }
                }
                RoutedMachineControlOutcome {
                    outcome,
                    route: match transport {
                        MachineControlTransport::Dbus => ExecutionRoute::ElevatedDbus,
                        MachineControlTransport::SystemdTools => {
                            ExecutionRoute::ElevatedSystemdTools
                        }
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
    runner: &dyn CommandRunner,
) -> MachineControlOutcome {
    match execute_dbus_system_operation_with_runner(dbus, system_operation(machine, intent), runner)
        .await
    {
        Ok(()) => MachineControlOutcome::Succeeded,
        Err(error) => map_system_operation_machine_error(error),
    }
}

async fn execute_systemd_tools_machine_control(
    machine: MachineName,
    intent: MachineControlIntent,
) -> MachineControlOutcome {
    execute_systemd_tools_machine_control_with_runner(
        machine,
        intent,
        &crate::adapters::process::DefaultCommandRunner,
    )
    .await
}

pub(crate) async fn execute_systemd_tools_nspawn_launch(
    image: ImageName,
    machine: MachineName,
) -> MachineControlOutcome {
    if image.as_str() != machine.as_str() {
        return MachineControlOutcome::Rejected {
            rejection: crate::application::machine_lifecycle::MachineRejection::InvalidTarget,
            reason: "nspawn launch currently requires matching image and machine names".into(),
        };
    }
    execute_systemd_tools_machine_control(machine, MachineControlIntent::Launch { image }).await
}

pub(crate) async fn execute_systemd_tools_machine_runtime(
    machine: MachineName,
    action: MachineRuntimeAction,
) -> MachineControlOutcome {
    execute_systemd_tools_machine_control(machine, MachineControlIntent::Runtime(action)).await
}

pub(crate) async fn execute_systemd_tools_nspawn_unit(
    machine: MachineName,
    action: NspawnUnitAction,
) -> MachineControlOutcome {
    execute_systemd_tools_machine_control(machine, MachineControlIntent::Unit(action)).await
}

async fn execute_systemd_tools_machine_control_with_runner(
    machine: MachineName,
    intent: MachineControlIntent,
    runner: &dyn CommandRunner,
) -> MachineControlOutcome {
    match execute_system_operation_with_runner(system_operation(machine, intent), runner).await {
        Ok(()) => MachineControlOutcome::Succeeded,
        Err(SystemOperationError::Io { source: error, .. }) => {
            MachineControlOutcome::NotAttempted {
                reason: format!("failed to launch machine control command: {error}"),
            }
        }
        Err(error) => map_system_operation_machine_error(error),
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
    async fn systemd_tools_control_uses_fixed_typed_command() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .withf(|program, args, timeout| {
                program == "machinectl"
                    && args.iter().map(String::as_str).eq(["--", "start", "test"])
                    && *timeout == std::time::Duration::from_secs(60)
            })
            .returning(|_, _, _| Ok(success()));

        let outcome = execute_systemd_tools_machine_control_with_runner(
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
    async fn systemd_tools_launch_failure_is_explicitly_not_attempted() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run_bounded().returning(|_, _, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "machinectl missing",
            ))
        });

        let outcome = execute_systemd_tools_machine_control_with_runner(
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

    #[tokio::test]
    async fn systemd_tools_mutation_timeout_has_unknown_outcome() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run_bounded().returning(|_, _, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "deadline exceeded",
            ))
        });

        let outcome = execute_systemd_tools_machine_control_with_runner(
            MachineName::new("test").unwrap(),
            MachineControlIntent::Runtime(MachineRuntimeAction::Poweroff),
            &runner,
        )
        .await;

        assert!(matches!(
            outcome,
            MachineControlOutcome::OutcomeUnknown { .. }
        ));
    }
}

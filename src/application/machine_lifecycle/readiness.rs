//! Observe or start a machine before opening a desktop command session.

use std::sync::Arc;

use crate::application::runtime::{RuntimeCatalog, RuntimeError};
use crate::domain::machine::MachineName;
use crate::domain::runtime::{MachineEntry, MachineState};

use super::{MachineLifecycleResult, MachineLifecycleService, MachineRejection};

#[derive(Debug, thiserror::Error)]
pub enum MachineReadyError {
    #[error("could not observe machine state: {0}")]
    Observation(#[from] RuntimeError),
    #[error("machine {machine} is {state}; no start was attempted", state = .state.label())]
    Unavailable {
        machine: MachineName,
        state: MachineState,
    },
    #[error("machine image {0} was not found")]
    NotFound(MachineName),
    #[error("machine start was rejected: {rejection}: {reason}")]
    Rejected {
        rejection: MachineRejection,
        reason: String,
    },
    #[error("machine start was not attempted: {0}")]
    NotAttempted(String),
    #[error("machine start failed: {0}")]
    Failed(String),
    #[error("machine readiness could not be confirmed: {0}")]
    OutcomeUnknown(String),
}

impl MachineLifecycleService {
    pub async fn ensure_running(
        self: &Arc<Self>,
        runtime: &RuntimeCatalog,
        machine: &MachineName,
    ) -> Result<(), MachineReadyError> {
        let machines = runtime.machines().await?.value;
        if let Some(entry) = machines
            .into_iter()
            .find(|entry| entry.name == machine.as_str())
        {
            return self.wait_for_running(runtime, machine, entry).await;
        }

        // Recheck registrations in the image snapshot before submitting a start.
        // A concurrent start must never be mistaken for a stopped image.
        let snapshot = runtime.snapshot().await?.value;
        if let Some(entry) = snapshot
            .machines
            .into_iter()
            .find(|entry| entry.name == machine.as_str())
        {
            return self.wait_for_running(runtime, machine, entry).await;
        }
        let image = snapshot
            .images
            .into_iter()
            .find(|image| image.name == machine.as_str())
            .ok_or_else(|| MachineReadyError::NotFound(machine.clone()))?;
        let operation =
            self.begin_launch(&image, None)
                .map_err(|rejection| MachineReadyError::Rejected {
                    reason: rejection.to_string(),
                    rejection,
                })?;
        match operation.run().await.result {
            MachineLifecycleResult::Succeeded => Ok(()),
            MachineLifecycleResult::NotAttempted(reason) => {
                Err(MachineReadyError::NotAttempted(reason))
            }
            MachineLifecycleResult::Rejected { rejection, reason } => {
                Err(MachineReadyError::Rejected { rejection, reason })
            }
            MachineLifecycleResult::Failed(reason) => Err(MachineReadyError::Failed(reason)),
            MachineLifecycleResult::OutcomeUnknown(reason) => {
                Err(MachineReadyError::OutcomeUnknown(reason))
            }
        }
    }

    async fn wait_for_running(
        &self,
        runtime: &RuntimeCatalog,
        machine: &MachineName,
        mut entry: MachineEntry,
    ) -> Result<(), MachineReadyError> {
        let started_at = tokio::time::Instant::now();
        loop {
            match entry.state {
                MachineState::Running => return Ok(()),
                MachineState::Starting => {}
                state => {
                    return Err(MachineReadyError::Unavailable {
                        machine: machine.clone(),
                        state,
                    });
                }
            }
            if started_at.elapsed() >= self.start_timeout {
                return Err(MachineReadyError::OutcomeUnknown(format!(
                    "machine {machine} is still starting after {}s",
                    self.start_timeout.as_secs_f64()
                )));
            }
            tokio::time::sleep(self.start_interval).await;
            entry = runtime
                .machines()
                .await?
                .value
                .into_iter()
                .find(|entry| entry.name == machine.as_str())
                .ok_or_else(|| {
                    MachineReadyError::Failed(format!(
                        "machine {machine} disappeared while waiting for startup"
                    ))
                })?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::machine_lifecycle::{
        MachineControlOutcome, MachinePreparationError, MockMachineControl, MockMachineObservation,
        MockMachineStartDiagnostics, MockMachineStartPreparation, RoutedMachineControlOutcome,
    };
    use crate::application::operations::{ExecutionRoute, OperationRegistry};
    use crate::application::runtime::MockRuntimePort;
    use crate::domain::inspection::{MachineProperties, GROUP_SYSTEMD_UNIT};
    use crate::domain::runtime::{ImageEntry, RuntimeSnapshot};
    use std::time::Duration;

    fn machine() -> MachineName {
        MachineName::new("desktop").unwrap()
    }

    fn entry(state: MachineState) -> MachineEntry {
        MachineEntry::optimistic_nspawn("desktop", state)
    }

    fn stopped_snapshot() -> RuntimeSnapshot {
        RuntimeSnapshot::new(
            vec![],
            vec![ImageEntry {
                name: "desktop".into(),
                image_type: "directory".into(),
                readonly: false,
                usage: None,
                dbus_object_path: None,
            }],
        )
    }

    fn port() -> MockRuntimePort {
        let mut port = MockRuntimePort::new();
        port.expect_snapshot_route()
            .return_const(ExecutionRoute::LocalSystemdTools);
        port
    }

    fn runtime(port: MockRuntimePort) -> RuntimeCatalog {
        RuntimeCatalog::new(None, Arc::new(port), vec![], None)
    }

    fn lifecycle(
        preparation: MockMachineStartPreparation,
        control: MockMachineControl,
        observation: MockMachineObservation,
    ) -> Arc<MachineLifecycleService> {
        Arc::new(
            MachineLifecycleService::new(
                Arc::new(control),
                Arc::new(preparation),
                Arc::new(observation),
                Arc::new(MockMachineStartDiagnostics::new()),
                OperationRegistry::new(),
            )
            .with_start_timing(Duration::from_millis(10), Duration::from_millis(1)),
        )
    }

    fn idle_lifecycle() -> Arc<MachineLifecycleService> {
        lifecycle(
            MockMachineStartPreparation::new(),
            MockMachineControl::new(),
            MockMachineObservation::new(),
        )
    }

    fn stopped_runtime() -> RuntimeCatalog {
        let mut port = port();
        port.expect_list_machines().once().returning(|| Ok(vec![]));
        port.expect_snapshot()
            .once()
            .returning(|| Ok(stopped_snapshot()));
        runtime(port)
    }

    #[tokio::test]
    async fn running_machine_skips_image_discovery_and_start_preparation() {
        let mut port = port();
        port.expect_list_machines()
            .once()
            .returning(|| Ok(vec![entry(MachineState::Running)]));

        idle_lifecycle()
            .ensure_running(&runtime(port), &machine())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stopped_machine_uses_existing_preparation_control_and_confirmation() {
        let mut sequence = mockall::Sequence::new();
        let mut preparation = MockMachineStartPreparation::new();
        preparation
            .expect_prepare()
            .withf(|target| target.as_str() == "desktop")
            .once()
            .in_sequence(&mut sequence)
            .returning(|_| Ok(()));
        let mut control = MockMachineControl::new();
        control
            .expect_launch()
            .withf(|image, target| image.as_str() == "desktop" && target.as_str() == "desktop")
            .once()
            .in_sequence(&mut sequence)
            .returning(|_, _| RoutedMachineControlOutcome {
                outcome: MachineControlOutcome::Succeeded,
                route: ExecutionRoute::DirectDbus,
                fallback: None,
            });
        let mut observation = MockMachineObservation::new();
        observation
            .expect_inspect()
            .once()
            .in_sequence(&mut sequence)
            .returning(|_, _| {
                let mut properties = MachineProperties::default();
                properties.insert(GROUP_SYSTEMD_UNIT, "ActiveState".into(), "active".into());
                Ok(properties)
            });
        observation.expect_invalidate().once().return_const(());

        lifecycle(preparation, control, observation)
            .ensure_running(&stopped_runtime(), &machine())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn machine_registered_during_image_discovery_is_not_started_again() {
        let mut port = port();
        port.expect_list_machines().once().returning(|| Ok(vec![]));
        port.expect_snapshot().once().returning(|| {
            Ok(RuntimeSnapshot::new(
                vec![entry(MachineState::Running)],
                vec![],
            ))
        });

        idle_lifecycle()
            .ensure_running(&runtime(port), &machine())
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn starting_machine_waits_without_starting_again() {
        let mut sequence = mockall::Sequence::new();
        let mut port = port();
        port.expect_list_machines()
            .once()
            .in_sequence(&mut sequence)
            .returning(|| Ok(vec![entry(MachineState::Starting)]));
        port.expect_list_machines()
            .once()
            .in_sequence(&mut sequence)
            .returning(|| Ok(vec![entry(MachineState::Running)]));

        idle_lifecycle()
            .ensure_running(&runtime(port), &machine())
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn startup_wait_timeout_does_not_submit_a_start() {
        let mut port = port();
        port.expect_list_machines()
            .returning(|| Ok(vec![entry(MachineState::Starting)]));

        assert!(matches!(
            idle_lifecycle()
                .ensure_running(&runtime(port), &machine())
                .await,
            Err(MachineReadyError::OutcomeUnknown(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn disappearing_startup_is_not_restarted() {
        let mut sequence = mockall::Sequence::new();
        let mut port = port();
        port.expect_list_machines()
            .once()
            .in_sequence(&mut sequence)
            .returning(|| Ok(vec![entry(MachineState::Starting)]));
        port.expect_list_machines()
            .once()
            .in_sequence(&mut sequence)
            .returning(|| Ok(vec![]));

        assert!(matches!(
            idle_lifecycle()
                .ensure_running(&runtime(port), &machine())
                .await,
            Err(MachineReadyError::Failed(_))
        ));
    }

    #[tokio::test]
    async fn exiting_and_unknown_machines_are_not_treated_as_stopped() {
        for state in [
            MachineState::Exiting,
            MachineState::Unknown("frozen".into()),
        ] {
            let mut port = port();
            let observed_state = state.clone();
            port.expect_list_machines()
                .once()
                .returning(move || Ok(vec![entry(observed_state.clone())]));

            let error = idle_lifecycle()
                .ensure_running(&runtime(port), &machine())
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                MachineReadyError::Unavailable { state: actual, .. } if actual == state
            ));
        }
    }

    #[tokio::test]
    async fn failed_observation_cannot_authorize_startup() {
        let mut port = port();
        port.expect_list_machines()
            .once()
            .returning(|| Err(RuntimeError::permission_denied("denied")));

        assert!(matches!(
            idle_lifecycle()
                .ensure_running(&runtime(port), &machine())
                .await,
            Err(MachineReadyError::Observation(
                RuntimeError::PermissionDenied(_)
            ))
        ));
    }

    #[tokio::test]
    async fn missing_image_does_not_submit_a_start() {
        let mut port = port();
        port.expect_list_machines().once().returning(|| Ok(vec![]));
        port.expect_snapshot()
            .once()
            .returning(|| Ok(RuntimeSnapshot::new(vec![], vec![])));

        assert!(matches!(
            idle_lifecycle()
                .ensure_running(&runtime(port), &machine())
                .await,
            Err(MachineReadyError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn preparation_permission_failure_prevents_start_submission() {
        let mut preparation = MockMachineStartPreparation::new();
        preparation
            .expect_prepare()
            .once()
            .returning(|_| Err(MachinePreparationError::permission_denied("NVIDIA state")));

        assert!(matches!(
            lifecycle(
                preparation,
                MockMachineControl::new(),
                MockMachineObservation::new(),
            )
            .ensure_running(&stopped_runtime(), &machine())
            .await,
            Err(MachineReadyError::Rejected {
                rejection: MachineRejection::PermissionDenied,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn rejected_or_unknown_start_is_not_replayed_or_confirmed() {
        for outcome in [
            MachineControlOutcome::Rejected {
                rejection: MachineRejection::PermissionDenied,
                reason: "authorization denied".into(),
            },
            MachineControlOutcome::OutcomeUnknown {
                reason: "connection lost".into(),
            },
        ] {
            let mut preparation = MockMachineStartPreparation::new();
            preparation.expect_prepare().once().returning(|_| Ok(()));
            let mut control = MockMachineControl::new();
            control
                .expect_launch()
                .once()
                .returning(move |_, _| RoutedMachineControlOutcome {
                    outcome: outcome.clone(),
                    route: ExecutionRoute::DirectDbus,
                    fallback: None,
                });
            let mut observation = MockMachineObservation::new();
            observation.expect_invalidate().once().return_const(());

            let error = lifecycle(preparation, control, observation)
                .ensure_running(&stopped_runtime(), &machine())
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                MachineReadyError::Rejected { .. } | MachineReadyError::OutcomeUnknown(_)
            ));
        }
    }
}

//! One bounded native observation for an explicit guest execution request.
//! Missing host procfs access remains unknown, preserving native/polkit entry.

use crate::adapters::process::guest_bus::GuestBusTarget;
use crate::adapters::process::DefaultCommandRunner;
use crate::adapters::runtime::{dbus, registration, systemd_tools};
use crate::application::sessions::execution::{
    GuestExecutionError, GuestExecutionFailure, GuestExecutionObservation, GuestExecutionPort,
    NativeReadiness,
};
use crate::application::sessions::{ObservedMachineInstance, SessionError};
use crate::domain::machine::MachineName;
use std::time::Duration;
use tokio::time::Instant;

use super::NativeExecution;

const OBSERVATION_BUDGET: Duration = Duration::from_secs(1);
const NATIVE_STARTUP_BUDGET: Duration = Duration::from_secs(10);

pub(super) async fn require_native(
    execution: &impl GuestExecutionPort,
    machine: &MachineName,
    overall_deadline: Instant,
    privileged: bool,
) -> Result<Option<ObservedMachineInstance>, GuestExecutionError> {
    let deadline = (Instant::now() + NATIVE_STARTUP_BUDGET).min(overall_deadline);
    let mut delay = Duration::from_millis(200);
    let mut instance: Option<ObservedMachineInstance> = None;
    let mut observed_systemd = false;
    loop {
        if Instant::now() >= deadline {
            return Err(not_attempted(SessionError::new(format!(
                "guest execution preparation deadline expired in {machine}"
            ))));
        }
        let observation = execution
            .observe(machine, deadline)
            .await
            .map_err(not_attempted)?;
        if let (Some(previous), Some(current)) = (instance, observation.instance) {
            if previous.compare(current) == Some(false) {
                return Err(not_attempted(instance_changed(machine)));
            }
        }
        instance = observation.instance.or(instance);
        log::debug!(
            "Native execution readiness for {machine}: {:?}",
            observation.native
        );
        // Keep the wait decision once this request has observed systemd;
        // a later incomplete observation does not change the route.
        observed_systemd |= observation.systemd_init;
        if observed_systemd && needs_startup_wait(&observation.native) && Instant::now() < deadline
        {
            log::debug!("Waiting for the systemd guest manager in {machine}");
            tokio::time::sleep_until((Instant::now() + delay).min(deadline)).await;
            delay = (delay * 2).min(Duration::from_secs(1));
            if Instant::now() < deadline {
                continue;
            }
        }
        return native_decision(observation.native, privileged)
            .map(|()| instance)
            .map_err(|error| error.context(&format!("prepare guest execution in {machine}")));
    }
}

impl NativeExecution {
    pub(super) async fn observe_native(
        &self,
        machine: &MachineName,
        overall_deadline: Instant,
    ) -> Result<GuestExecutionObservation, SessionError> {
        let deadline = (Instant::now() + OBSERVATION_BUDGET).min(overall_deadline);
        let before = match self.instance_before_deadline(machine, deadline).await {
            Ok(instance) => instance,
            Err(native) => {
                return Ok(GuestExecutionObservation {
                    native,
                    systemd_init: false,
                    instance: None,
                })
            }
        };
        let target = GuestBusTarget::open(before.leader_pid());
        let systemd_init = target
            .as_ref()
            .ok()
            .and_then(|target| target.systemd_init().ok())
            .unwrap_or(false);
        // Each adapter owns its deadline and child cleanup. An outer timeout
        // must not cancel busctl's termination/wait after its query expires.
        let readiness = match self {
            Self::Dbus(_) => match &target {
                Ok(target) => dbus::readiness::observe(target, deadline).await,
                Err(error) => NativeReadiness::unknown(error),
            },
            Self::SystemdTools => {
                systemd_tools::readiness::observe(machine, &DefaultCommandRunner, deadline).await
            }
        };
        if let Ok(target) = target {
            target.verify().map_err(|error| {
                SessionError::new(format!(
                    "guest execution target changed while observing {machine}: {error}"
                ))
            })?;
        }
        let readiness = match self.instance_before_deadline(machine, deadline).await {
            Ok(after) if before.compare(after) == Some(false) => {
                return Err(instance_changed(machine))
            }
            Ok(after) if before.compare(after) == Some(true) => readiness,
            Ok(_) => NativeReadiness::unknown(
                "machine instance could not be confirmed after readiness query",
            ),
            // A later lookup failure cannot erase an explicit authorization
            // rejection and turn it into a retry or native execution attempt.
            Err(_) if matches!(readiness, NativeReadiness::Denied { .. }) => readiness,
            Err(observation) => observation,
        };
        Ok(GuestExecutionObservation {
            native: readiness,
            systemd_init,
            instance: Some(before),
        })
    }

    pub(super) async fn instance_before_deadline(
        &self,
        machine: &MachineName,
        deadline: Instant,
    ) -> Result<ObservedMachineInstance, NativeReadiness> {
        // Resolving the name shares the query budget; the host bus's normal
        // timeout must not extend one readiness round.
        let leader = tokio::time::timeout_at(deadline, self.machine_leader(machine))
            .await
            .map_err(|_| NativeReadiness::Unresponsive {
                reason: "machine lookup exceeded the readiness deadline".into(),
            })?
            .map_err(NativeReadiness::unknown)?;
        registration::observe_machine_instance(leader).map_err(NativeReadiness::unknown)
    }
}

fn instance_changed(machine: &MachineName) -> SessionError {
    SessionError::new(format!(
        "machine {machine} changed instance during guest execution preparation; retry the request"
    ))
}

fn needs_startup_wait(readiness: &NativeReadiness) -> bool {
    matches!(
        readiness,
        NativeReadiness::NoBus { .. }
            | NativeReadiness::NoManager
            | NativeReadiness::Unresponsive { .. }
    )
}

fn not_attempted(error: SessionError) -> GuestExecutionError {
    GuestExecutionError::new(GuestExecutionFailure::NotAttempted, error)
}

fn native_decision(
    readiness: NativeReadiness,
    privileged: bool,
) -> Result<(), GuestExecutionError> {
    match readiness {
        NativeReadiness::Ready { .. } => Ok(()),
        // An unavailable host observation must not block the native entry.
        // In user mode, even a denied read can still reach machine1/polkit.
        NativeReadiness::Unknown { reason } => {
            log::debug!("Native readiness unknown; preserving native authorization path: {reason}");
            Ok(())
        }
        NativeReadiness::Denied { .. } if !privileged => Ok(()),
        NativeReadiness::NoBus { reason } => Err(not_attempted(SessionError::with_hint(
            format!("native guest system bus is unavailable: {reason}"),
            "Check the guest system bus. A non-systemd guest needs the guest helper; helper execution is not connected yet.",
        ))),
        NativeReadiness::NoManager => Err(not_attempted(SessionError::with_hint(
            "native guest systemd manager is unavailable",
            "Wait for the guest manager to start. A non-systemd guest needs the guest helper; helper execution is not connected yet.",
        ))),
        NativeReadiness::Denied { reason } => Err(GuestExecutionError::new(GuestExecutionFailure::Rejected, SessionError::new(format!("guest bus readiness query was denied: {reason}")))),
        NativeReadiness::Unresponsive { reason } => Err(not_attempted(SessionError::with_hint(
            format!("native guest manager is not ready: {reason}"),
            "Check that the guest bus and manager respond before retrying. No guest process was submitted.",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::session::execution::PreparedNativeExecution;
    use crate::application::sessions::execution::GuestExecutionRequest;
    use std::collections::VecDeque;
    use std::num::NonZeroU64;
    use std::sync::Mutex;

    struct Observations(Mutex<VecDeque<GuestExecutionObservation>>);

    #[async_trait::async_trait]
    impl GuestExecutionPort for Observations {
        type Prepared = PreparedNativeExecution;

        async fn observe(
            &self,
            _: &MachineName,
            _: Instant,
        ) -> Result<GuestExecutionObservation, SessionError> {
            let mut observations = self.0.lock().unwrap();
            if observations.len() > 1 {
                Ok(observations.pop_front().unwrap())
            } else {
                let last = observations.front().unwrap();
                Ok(observation(
                    last.native.clone(),
                    last.systemd_init,
                    last.instance,
                ))
            }
        }

        async fn prepare(
            &self,
            _: GuestExecutionRequest,
            _: Instant,
        ) -> Result<Self::Prepared, GuestExecutionError> {
            panic!("readiness must not submit a guest process")
        }
    }

    fn instance(start: u64) -> ObservedMachineInstance {
        ObservedMachineInstance::from_process(42, NonZeroU64::new(start).unwrap(), None)
    }

    fn observation(
        native: NativeReadiness,
        systemd_init: bool,
        instance: Option<ObservedMachineInstance>,
    ) -> GuestExecutionObservation {
        GuestExecutionObservation {
            native,
            systemd_init,
            instance,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_the_same_systemd_instance_even_if_later_init_observation_is_missing() {
        let observations = Observations(Mutex::new(VecDeque::from([
            observation(NativeReadiness::NoManager, true, Some(instance(1))),
            observation(NativeReadiness::NoManager, false, Some(instance(1))),
            observation(
                NativeReadiness::Ready {
                    version: "261".into(),
                },
                false,
                Some(instance(1)),
            ),
        ])));
        let started = Instant::now();
        let result = require_native(
            &observations,
            &MachineName::new("demo").unwrap(),
            started + Duration::from_secs(20),
            true,
        )
        .await
        .unwrap();
        assert_eq!(result, Some(instance(1)));
        assert_eq!(Instant::now() - started, Duration::from_millis(600));
    }

    #[tokio::test(start_paused = true)]
    async fn startup_wait_uses_the_callers_deadline_and_preserves_the_last_diagnostic() {
        let observations = Observations(Mutex::new(VecDeque::from([observation(
            NativeReadiness::NoBus {
                reason: "socket absent".into(),
            },
            true,
            Some(instance(1)),
        )])));
        let started = Instant::now();
        let error = require_native(
            &observations,
            &MachineName::new("demo").unwrap(),
            started + Duration::from_millis(350),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(error.failure, GuestExecutionFailure::NotAttempted);
        assert!(error.to_string().contains("socket absent"));
        assert_eq!(Instant::now() - started, Duration::from_millis(350));
    }

    #[tokio::test(start_paused = true)]
    async fn restart_during_readiness_wait_rejects_the_original_request() {
        let observations = Observations(Mutex::new(VecDeque::from([
            observation(NativeReadiness::NoManager, true, Some(instance(1))),
            observation(
                NativeReadiness::Ready {
                    version: "261".into(),
                },
                true,
                Some(instance(2)),
            ),
        ])));
        let error = require_native(
            &observations,
            &MachineName::new("demo").unwrap(),
            Instant::now() + Duration::from_secs(20),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(error.failure, GuestExecutionFailure::NotAttempted);
        assert!(error.to_string().contains("changed instance"));
    }

    #[test]
    fn unavailable_host_evidence_and_user_policy_checks_preserve_native_entry() {
        for privileged in [false, true] {
            assert!(
                native_decision(NativeReadiness::unknown("host procfs denied"), privileged).is_ok()
            );
        }
        assert!(native_decision(
            NativeReadiness::Denied {
                reason: "policy".into()
            },
            false
        )
        .is_ok());
        assert!(native_decision(
            NativeReadiness::Denied {
                reason: "policy".into()
            },
            true
        )
        .is_err());
        assert!(native_decision(NativeReadiness::NoManager, true).is_err());
        assert!(!needs_startup_wait(&NativeReadiness::unknown(
            "missing busctl"
        )));
        assert!(!needs_startup_wait(&NativeReadiness::Denied {
            reason: "policy".into()
        }));
        assert!(needs_startup_wait(&NativeReadiness::NoManager));
    }
}

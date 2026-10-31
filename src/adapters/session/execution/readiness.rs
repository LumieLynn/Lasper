//! One bounded native observation for an explicit guest execution request.
//! Missing host procfs access remains unknown, preserving native/polkit entry.

use crate::adapters::process::guest_bus::GuestBusTarget;
use crate::adapters::process::DefaultCommandRunner;
use crate::adapters::runtime::{dbus, registration, systemd_tools};
use crate::application::sessions::execution::NativeReadiness;
use crate::application::sessions::{ObservedMachineInstance, SessionError};
use crate::domain::machine::MachineName;
use std::time::Duration;
use tokio::time::Instant;

use super::NativeExecution;

const OBSERVATION_BUDGET: Duration = Duration::from_secs(1);
const NATIVE_STARTUP_BUDGET: Duration = Duration::from_secs(10);

struct NativeObservation {
    readiness: NativeReadiness,
    systemd_init: bool,
    instance: Option<ObservedMachineInstance>,
}

impl NativeObservation {
    fn unavailable(readiness: NativeReadiness) -> Self {
        Self {
            readiness,
            systemd_init: false,
            instance: None,
        }
    }
}

impl NativeExecution {
    pub(super) async fn require_native(&self, machine: &MachineName) -> Result<(), SessionError> {
        let deadline = Instant::now() + NATIVE_STARTUP_BUDGET;
        let mut delay = Duration::from_millis(200);
        let mut instance = None;
        loop {
            let observation = self.observe_native(machine, deadline, instance).await?;
            instance = observation.instance.or(instance);
            log::debug!(
                "Native execution readiness for {machine}: {:?}",
                observation.readiness
            );
            if observation.systemd_init
                && needs_startup_wait(&observation.readiness)
                && Instant::now() < deadline
            {
                log::debug!("Waiting for the systemd guest manager in {machine}");
                tokio::time::sleep_until((Instant::now() + delay).min(deadline)).await;
                delay = (delay * 2).min(Duration::from_secs(1));
                if Instant::now() < deadline {
                    continue;
                }
            }
            return native_decision(observation.readiness, uzers::get_effective_uid() == 0)
                .map_err(|error| error.context(&format!("prepare guest execution in {machine}")));
        }
    }

    async fn observe_native(
        &self,
        machine: &MachineName,
        overall_deadline: Instant,
        expected: Option<ObservedMachineInstance>,
    ) -> Result<NativeObservation, SessionError> {
        let deadline = (Instant::now() + OBSERVATION_BUDGET).min(overall_deadline);
        let before = match self.instance_before_deadline(machine, deadline).await {
            Ok(instance) => instance,
            Err(observation) => return Ok(NativeObservation::unavailable(observation)),
        };
        if expected.is_some_and(|previous| previous.compare(before) == Some(false)) {
            return Err(instance_changed(machine));
        }
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
        Ok(NativeObservation {
            readiness,
            systemd_init,
            instance: Some(before),
        })
    }

    async fn instance_before_deadline(
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

fn native_decision(readiness: NativeReadiness, privileged: bool) -> Result<(), SessionError> {
    match readiness {
        NativeReadiness::Ready { .. } => Ok(()),
        // An unavailable host observation must not block the native entry.
        // In user mode, even a denied read can still reach machine1/polkit.
        NativeReadiness::Unknown { reason } => {
            log::debug!("Native readiness unknown; preserving native authorization path: {reason}");
            Ok(())
        }
        NativeReadiness::Denied { .. } if !privileged => Ok(()),
        NativeReadiness::NoBus { reason } => Err(SessionError::with_hint(
            format!("native guest system bus is unavailable: {reason}"),
            "Check the guest system bus. A non-systemd guest needs the guest helper; helper execution is not connected yet.",
        )),
        NativeReadiness::NoManager => Err(SessionError::with_hint(
            "native guest systemd manager is unavailable",
            "Wait for the guest manager to start. A non-systemd guest needs the guest helper; helper execution is not connected yet.",
        )),
        NativeReadiness::Denied { reason } => Err(SessionError::new(format!("guest bus readiness query was denied: {reason}"))),
        NativeReadiness::Unresponsive { reason } => Err(SessionError::with_hint(
            format!("native guest manager is not ready: {reason}"),
            "Check that the guest bus and manager respond before retrying. No guest process was submitted.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

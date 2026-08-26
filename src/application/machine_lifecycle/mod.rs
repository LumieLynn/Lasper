//! Application-owned machine lifecycle workflow and transition projection.

use super::operations::{ExecutionRoute, RouteFallback};
use super::operations::{
    OperationRegistry, ResourceClaim, ResourceConflict, ResourceKey, ResourceReservation,
};
use crate::nspawn::models::{
    AllowedSignal, MachineEntry, MachineName, MachineProperties, MachineState,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const START_CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);
const START_CONFIRM_INTERVAL: Duration = Duration::from_millis(100);
const START_TRANSITION_TIMEOUT: Duration = Duration::from_secs(60);
const OTHER_TRANSITION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum MachineAction {
    Start,
    Terminate,
    Poweroff,
    Reboot,
    Enable,
    Disable,
    Kill { signal: AllowedSignal },
}

impl MachineAction {
    pub fn success_label(self) -> &'static str {
        match self {
            Self::Start => "Started",
            Self::Terminate => "Terminated",
            Self::Poweroff => "Powered off",
            Self::Reboot => "Rebooting",
            Self::Enable => "Enabled",
            Self::Disable => "Disabled",
            Self::Kill {
                signal: AllowedSignal::Kill,
            } => "Sent SIGKILL to",
            Self::Kill {
                signal: AllowedSignal::Terminate,
            } => "Sent SIGTERM to",
        }
    }

    pub fn audit_label(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Terminate => "Terminate",
            Self::Poweroff => "Power off",
            Self::Reboot => "Reboot",
            Self::Enable => "Enable",
            Self::Disable => "Disable",
            Self::Kill {
                signal: AllowedSignal::Kill,
            } => "Send SIGKILL to",
            Self::Kill {
                signal: AllowedSignal::Terminate,
            } => "Send SIGTERM to",
        }
    }

    fn requires_running(self) -> bool {
        matches!(
            self,
            Self::Terminate | Self::Poweroff | Self::Reboot | Self::Kill { .. }
        )
    }

    fn transition(self) -> Option<MachineTransitionKind> {
        match self {
            Self::Start => Some(MachineTransitionKind::Starting),
            Self::Terminate | Self::Poweroff => Some(MachineTransitionKind::Stopping),
            Self::Reboot => Some(MachineTransitionKind::Rebooting { saw_absent: false }),
            Self::Enable | Self::Disable | Self::Kill { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MachineControlTransport {
    Dbus,
    Cli,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MachineControlRequest {
    pub machine: MachineName,
    pub action: MachineAction,
    pub transport: MachineControlTransport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineRejection {
    InvalidTarget,
    Busy,
    NotFound,
    AlreadyRunning,
    NotRunning,
    PermissionDenied,
    Unsupported,
}

impl std::fmt::Display for MachineRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidTarget => "invalid machine target",
            Self::Busy => "machine has a conflicting operation in progress",
            Self::NotFound => "machine was not found",
            Self::AlreadyRunning => "machine is already running",
            Self::NotRunning => "machine is not running",
            Self::PermissionDenied => "permission was denied",
            Self::Unsupported => "operation is not supported",
        };
        f.write_str(message)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MachineControlOutcome {
    Succeeded,
    NotAttempted {
        reason: String,
    },
    Rejected {
        rejection: MachineRejection,
        reason: String,
    },
    Failed {
        reason: String,
    },
    OutcomeUnknown {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedMachineControlOutcome {
    pub outcome: MachineControlOutcome,
    pub route: ExecutionRoute,
    pub fallback: Option<RouteFallback>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MachineLifecycleResult {
    Succeeded,
    NotAttempted(String),
    Rejected {
        rejection: MachineRejection,
        reason: String,
    },
    Failed(String),
    OutcomeUnknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineLifecycleOutcome {
    pub machine: MachineName,
    pub action: MachineAction,
    pub result: MachineLifecycleResult,
    pub route: Option<ExecutionRoute>,
    pub fallback: Option<RouteFallback>,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait MachineControl: Send + Sync + 'static {
    async fn execute(
        &self,
        machine: &MachineName,
        action: MachineAction,
    ) -> RoutedMachineControlOutcome;
}

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait MachineStartPreparation: Send + Sync + 'static {
    async fn prepare(&self, machine: &MachineName) -> Result<(), String>;
}

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait MachineObservation: Send + Sync + 'static {
    async fn inspect(
        &self,
        machine: &MachineName,
        entry: &MachineEntry,
    ) -> Result<MachineProperties, String>;
    fn invalidate(&self);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartFailureEvidence {
    pub journal_command: String,
    pub journal: Option<String>,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait MachineStartDiagnostics: Send + Sync + 'static {
    async fn collect(
        &self,
        machine: &MachineName,
        invocation_id: Option<String>,
        started_epoch: u64,
    ) -> StartFailureEvidence;
}

#[derive(Clone, Debug)]
enum MachineTransitionKind {
    Starting,
    Stopping,
    Rebooting { saw_absent: bool },
}

#[derive(Clone, Debug)]
struct MachineTransition {
    kind: MachineTransitionKind,
    started_at: Instant,
}

pub struct MachineLifecycleService {
    control: Arc<dyn MachineControl>,
    preparation: Arc<dyn MachineStartPreparation>,
    observation: Arc<dyn MachineObservation>,
    diagnostics: Arc<dyn MachineStartDiagnostics>,
    registry: Arc<OperationRegistry>,
    transitions: parking_lot::Mutex<HashMap<String, MachineTransition>>,
    start_timeout: Duration,
    start_interval: Duration,
}

impl MachineLifecycleService {
    pub fn new(
        control: Arc<dyn MachineControl>,
        preparation: Arc<dyn MachineStartPreparation>,
        observation: Arc<dyn MachineObservation>,
        diagnostics: Arc<dyn MachineStartDiagnostics>,
        registry: Arc<OperationRegistry>,
    ) -> Self {
        Self {
            control,
            preparation,
            observation,
            diagnostics,
            registry,
            transitions: parking_lot::Mutex::new(HashMap::new()),
            start_timeout: START_CONFIRM_TIMEOUT,
            start_interval: START_CONFIRM_INTERVAL,
        }
    }

    #[cfg(test)]
    fn with_start_timing(mut self, timeout: Duration, interval: Duration) -> Self {
        self.start_timeout = timeout;
        self.start_interval = interval;
        self
    }

    pub fn begin(
        self: &Arc<Self>,
        name: &str,
        action: MachineAction,
        observed_state: Option<MachineState>,
    ) -> Result<MachineOperation, MachineRejection> {
        let machine = MachineName::new(name).map_err(|_| MachineRejection::InvalidTarget)?;
        if action == MachineAction::Start
            && observed_state
                .as_ref()
                .is_some_and(MachineState::is_running)
        {
            return Err(MachineRejection::AlreadyRunning);
        }
        if action.requires_running()
            && !observed_state
                .as_ref()
                .is_some_and(MachineState::is_running)
        {
            return Err(MachineRejection::NotRunning);
        }
        let reservation = self
            .registry
            .reserve([ResourceClaim::exclusive(ResourceKey::for_machine(&machine))])
            .map_err(|ResourceConflict { .. }| MachineRejection::Busy)?;
        let transition = action.transition();
        if let Some(kind) = transition.clone() {
            self.transitions.lock().insert(
                machine.as_str().to_string(),
                MachineTransition {
                    kind,
                    started_at: Instant::now(),
                },
            );
        }
        Ok(MachineOperation {
            service: Arc::clone(self),
            machine,
            action,
            transition,
            retain_transition: false,
            _reservation: reservation,
        })
    }

    pub fn project_machines(&self, mut entries: Vec<MachineEntry>) -> Vec<MachineEntry> {
        let now = Instant::now();
        let mut transitions = self.transitions.lock();
        transitions.retain(|name, transition| {
            let timeout = if matches!(transition.kind, MachineTransitionKind::Starting) {
                START_TRANSITION_TIMEOUT
            } else {
                OTHER_TRANSITION_TIMEOUT
            };
            if now.duration_since(transition.started_at) > timeout {
                return false;
            }
            let entry = entries.iter_mut().find(|entry| entry.name == *name);
            match (&mut transition.kind, entry) {
                (MachineTransitionKind::Starting, Some(entry))
                    if entry.state == MachineState::Running =>
                {
                    false
                }
                (MachineTransitionKind::Starting, Some(entry)) => {
                    entry.state = MachineState::Starting;
                    true
                }
                (MachineTransitionKind::Starting, None) => true,
                (MachineTransitionKind::Stopping, Some(entry)) => {
                    entry.state = MachineState::Exiting;
                    true
                }
                (MachineTransitionKind::Stopping, None) => false,
                (MachineTransitionKind::Rebooting { saw_absent }, None) => {
                    *saw_absent = true;
                    true
                }
                (MachineTransitionKind::Rebooting { saw_absent: true }, Some(_)) => false,
                (MachineTransitionKind::Rebooting { saw_absent: false }, Some(entry)) => {
                    entry.state = MachineState::Exiting;
                    true
                }
            }
        });
        let missing_starts = transitions
            .iter()
            .filter(|(_, transition)| matches!(transition.kind, MachineTransitionKind::Starting))
            .filter(|(name, _)| entries.iter().all(|entry| entry.name != name.as_str()))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        drop(transitions);
        entries.extend(missing_starts.into_iter().map(|name| MachineEntry {
            name,
            state: MachineState::Starting,
            address: None,
            all_addresses: Vec::new(),
        }));
        entries.sort();
        entries
    }

    fn clear_transition(&self, machine: &MachineName) {
        self.transitions.lock().remove(machine.as_str());
    }

    async fn confirm_start(&self, machine: &MachineName) -> StartConfirmation {
        let started_at = tokio::time::Instant::now();
        let started_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = MachineEntry {
            name: machine.as_str().to_string(),
            state: MachineState::Starting,
            address: None,
            all_addresses: Vec::new(),
        };
        let mut last_observation = "systemd unit properties unavailable".to_string();
        let mut last_invocation_id = None;
        loop {
            match self.observation.inspect(machine, &entry).await {
                Ok(properties) => {
                    let invocation_id = systemd_property(&properties, "InvocationID")
                        .filter(|value| valid_invocation_id(value))
                        .map(str::to_string);
                    if invocation_id.is_some() {
                        last_invocation_id = invocation_id.clone();
                    }
                    match observe_start(&properties) {
                        StartObservation::Started => return StartConfirmation::Started,
                        StartObservation::Pending(details) => {
                            if !details.is_empty() {
                                last_observation = details;
                            }
                        }
                        StartObservation::Failed(details) => {
                            let reason = self
                                .start_failure_reason(
                                    machine,
                                    &details,
                                    invocation_id.as_deref(),
                                    started_epoch,
                                )
                                .await;
                            return StartConfirmation::Failed(reason);
                        }
                    }
                }
                Err(error) => last_observation = error,
            }
            if started_at.elapsed() >= self.start_timeout {
                let details = format!(
                    "start confirmation timed out after {}s; {}",
                    self.start_timeout.as_secs_f64(),
                    last_observation
                );
                let reason = self
                    .start_failure_reason(
                        machine,
                        &details,
                        last_invocation_id.as_deref(),
                        started_epoch,
                    )
                    .await;
                return StartConfirmation::OutcomeUnknown(reason);
            }
            tokio::time::sleep(self.start_interval).await;
        }
    }

    async fn start_failure_reason(
        &self,
        machine: &MachineName,
        details: &str,
        invocation_id: Option<&str>,
        started_epoch: u64,
    ) -> String {
        let evidence = self
            .diagnostics
            .collect(machine, invocation_id.map(str::to_string), started_epoch)
            .await;
        if let Some(journal) = evidence.journal {
            log::error!(
                "Container start failed for {}: {}\nRecent {} journal:\n{}",
                machine,
                details,
                machine.systemd_nspawn_unit(),
                journal
            );
        } else {
            log::error!("Container start failed for {}: {}", machine, details);
        }
        format!(
            "Container '{}' failed to start ({}). Inspect host logs with `{}`.",
            machine, details, evidence.journal_command
        )
    }
}

pub struct MachineOperation {
    service: Arc<MachineLifecycleService>,
    machine: MachineName,
    action: MachineAction,
    transition: Option<MachineTransitionKind>,
    retain_transition: bool,
    _reservation: ResourceReservation,
}

impl MachineOperation {
    pub async fn run(mut self) -> MachineLifecycleOutcome {
        if self.action == MachineAction::Start {
            if let Err(reason) = self.service.preparation.prepare(&self.machine).await {
                return self.outcome(MachineLifecycleResult::Failed(reason), None, None);
            }
        }

        let routed = self
            .service
            .control
            .execute(&self.machine, self.action)
            .await;
        let result = match routed.outcome {
            MachineControlOutcome::Succeeded if self.action == MachineAction::Start => {
                match self.service.confirm_start(&self.machine).await {
                    StartConfirmation::Started => {
                        self.retain_transition = self.transition.is_some();
                        MachineLifecycleResult::Succeeded
                    }
                    StartConfirmation::Failed(reason) => MachineLifecycleResult::Failed(reason),
                    StartConfirmation::OutcomeUnknown(reason) => {
                        self.retain_transition = self.transition.is_some();
                        MachineLifecycleResult::OutcomeUnknown(reason)
                    }
                }
            }
            MachineControlOutcome::Succeeded => {
                self.retain_transition = self.transition.is_some();
                MachineLifecycleResult::Succeeded
            }
            MachineControlOutcome::NotAttempted { reason } => {
                MachineLifecycleResult::NotAttempted(reason)
            }
            MachineControlOutcome::Rejected { rejection, reason } => {
                MachineLifecycleResult::Rejected { rejection, reason }
            }
            MachineControlOutcome::Failed { reason } => MachineLifecycleResult::Failed(reason),
            MachineControlOutcome::OutcomeUnknown { reason } => {
                self.retain_transition = self.transition.is_some();
                MachineLifecycleResult::OutcomeUnknown(reason)
            }
        };
        self.service.observation.invalidate();
        self.outcome(result, Some(routed.route), routed.fallback)
    }

    fn outcome(
        &self,
        result: MachineLifecycleResult,
        route: Option<ExecutionRoute>,
        fallback: Option<RouteFallback>,
    ) -> MachineLifecycleOutcome {
        MachineLifecycleOutcome {
            machine: self.machine.clone(),
            action: self.action,
            result,
            route,
            fallback,
        }
    }
}

impl Drop for MachineOperation {
    fn drop(&mut self) {
        if self.transition.is_some() && !self.retain_transition {
            self.service.clear_transition(&self.machine);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StartObservation {
    Started,
    Pending(String),
    Failed(String),
}

enum StartConfirmation {
    Started,
    Failed(String),
    OutcomeUnknown(String),
}

fn systemd_property<'a>(properties: &'a MachineProperties, key: &str) -> Option<&'a str> {
    properties
        .groups
        .iter()
        .find(|group| group.name == crate::nspawn::models::GROUP_SYSTEMD_UNIT)
        .and_then(|group| group.properties.get(key))
        .map(String::as_str)
}

fn observe_start(properties: &MachineProperties) -> StartObservation {
    let active_state = systemd_property(properties, "ActiveState").unwrap_or_default();
    let service_result = systemd_property(properties, "Result").unwrap_or_default();
    let details = [
        "ActiveState",
        "SubState",
        "Result",
        "ExecMainCode",
        "ExecMainStatus",
        "StatusText",
    ]
    .into_iter()
    .filter_map(|key| systemd_property(properties, key).map(|value| format!("{key}={value}")))
    .collect::<Vec<_>>()
    .join(", ");
    if active_state == "active" {
        StartObservation::Started
    } else if active_state == "failed"
        || (!service_result.is_empty()
            && service_result != "success"
            && service_result != "[not set]")
    {
        StartObservation::Failed(details)
    } else {
        StartObservation::Pending(details)
    }
}

fn valid_invocation_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, state: MachineState) -> MachineEntry {
        MachineEntry {
            name: name.into(),
            state,
            address: None,
            all_addresses: vec![],
        }
    }

    fn active_properties() -> MachineProperties {
        let mut properties = MachineProperties::default();
        properties.insert(
            crate::nspawn::models::GROUP_SYSTEMD_UNIT,
            "ActiveState".into(),
            "active".into(),
        );
        properties
    }

    fn service(
        control: MockMachineControl,
        observation: MockMachineObservation,
    ) -> Arc<MachineLifecycleService> {
        let mut preparation = MockMachineStartPreparation::new();
        preparation.expect_prepare().returning(|_| Ok(()));
        let mut diagnostics = MockMachineStartDiagnostics::new();
        diagnostics
            .expect_collect()
            .returning(|machine, _, _| StartFailureEvidence {
                journal_command: format!("journalctl -u {}", machine.systemd_nspawn_unit()),
                journal: None,
            });
        Arc::new(MachineLifecycleService::new(
            Arc::new(control),
            Arc::new(preparation),
            Arc::new(observation),
            Arc::new(diagnostics),
            OperationRegistry::new(),
        ))
    }

    #[test]
    fn transition_projection_synthesizes_start_and_resolves_running_state() {
        let control = MockMachineControl::new();
        let observation = MockMachineObservation::new();
        let service = service(control, observation);
        let operation = service.begin("test", MachineAction::Start, None).unwrap();

        assert_eq!(
            service.project_machines(vec![]),
            vec![entry("test", MachineState::Starting)]
        );
        assert_eq!(
            service.project_machines(vec![entry("test", MachineState::Running)]),
            vec![entry("test", MachineState::Running)]
        );
        drop(operation);
    }

    #[test]
    fn dropped_unsubmitted_operation_rolls_back_transition() {
        let control = MockMachineControl::new();
        let observation = MockMachineObservation::new();
        let service = service(control, observation);
        let operation = service.begin("test", MachineAction::Start, None).unwrap();
        drop(operation);

        assert!(service.project_machines(vec![]).is_empty());
    }

    #[tokio::test]
    async fn successful_start_is_confirmed_through_observation() {
        let mut control = MockMachineControl::new();
        control
            .expect_execute()
            .returning(|_, _| RoutedMachineControlOutcome {
                outcome: MachineControlOutcome::Succeeded,
                route: ExecutionRoute::DirectDbus,
                fallback: None,
            });
        let mut observation = MockMachineObservation::new();
        observation
            .expect_inspect()
            .returning(|_, _| Ok(active_properties()));
        observation.expect_invalidate().once().return_const(());
        let service = service(control, observation);
        let operation = service.begin("test", MachineAction::Start, None).unwrap();

        let outcome = operation.run().await;

        assert_eq!(outcome.result, MachineLifecycleResult::Succeeded);
        assert_eq!(outcome.route, Some(ExecutionRoute::DirectDbus));
    }

    #[tokio::test]
    async fn start_confirmation_timeout_is_outcome_unknown_and_keeps_projection() {
        let mut control = MockMachineControl::new();
        control
            .expect_execute()
            .returning(|_, _| RoutedMachineControlOutcome {
                outcome: MachineControlOutcome::Succeeded,
                route: ExecutionRoute::DirectDbus,
                fallback: None,
            });
        let mut preparation = MockMachineStartPreparation::new();
        preparation.expect_prepare().returning(|_| Ok(()));
        let mut observation = MockMachineObservation::new();
        observation.expect_inspect().once().returning(|_, _| {
            let mut properties = MachineProperties::default();
            properties.insert(
                crate::nspawn::models::GROUP_SYSTEMD_UNIT,
                "ActiveState".into(),
                "activating".into(),
            );
            Ok(properties)
        });
        observation.expect_invalidate().once().return_const(());
        let mut diagnostics = MockMachineStartDiagnostics::new();
        diagnostics
            .expect_collect()
            .once()
            .returning(|machine, _, _| StartFailureEvidence {
                journal_command: format!("journalctl -u {}", machine.systemd_nspawn_unit()),
                journal: None,
            });
        let service = Arc::new(
            MachineLifecycleService::new(
                Arc::new(control),
                Arc::new(preparation),
                Arc::new(observation),
                Arc::new(diagnostics),
                OperationRegistry::new(),
            )
            .with_start_timing(Duration::ZERO, Duration::ZERO),
        );
        let operation = service.begin("test", MachineAction::Start, None).unwrap();

        let outcome = operation.run().await;

        assert!(matches!(
            outcome.result,
            MachineLifecycleResult::OutcomeUnknown(_)
        ));
        assert_eq!(
            service.project_machines(vec![]),
            vec![entry("test", MachineState::Starting)]
        );
    }

    #[tokio::test]
    async fn rejected_control_clears_transition_without_fallback_replay() {
        let mut control = MockMachineControl::new();
        control
            .expect_execute()
            .returning(|_, _| RoutedMachineControlOutcome {
                outcome: MachineControlOutcome::Rejected {
                    rejection: MachineRejection::PermissionDenied,
                    reason: "denied".into(),
                },
                route: ExecutionRoute::DirectDbus,
                fallback: None,
            });
        let mut observation = MockMachineObservation::new();
        observation.expect_invalidate().once().return_const(());
        let service = service(control, observation);
        let operation = service.begin("test", MachineAction::Start, None).unwrap();

        let outcome = operation.run().await;

        assert!(matches!(
            outcome.result,
            MachineLifecycleResult::Rejected {
                rejection: MachineRejection::PermissionDenied,
                ..
            }
        ));
        assert!(service.project_machines(vec![]).is_empty());
    }

    #[test]
    fn begin_rejects_invalid_state_and_conflicting_resource() {
        let control = MockMachineControl::new();
        let observation = MockMachineObservation::new();
        let service = service(control, observation);
        assert!(matches!(
            service.begin("test", MachineAction::Start, Some(MachineState::Running)),
            Err(MachineRejection::AlreadyRunning)
        ));
        let first = service.begin("test", MachineAction::Start, None).unwrap();
        assert!(matches!(
            service.begin("test", MachineAction::Start, None),
            Err(MachineRejection::Busy)
        ));
        drop(first);
    }

    #[test]
    fn start_observation_distinguishes_active_pending_and_failed() {
        assert_eq!(
            observe_start(&active_properties()),
            StartObservation::Started
        );
        let mut failed = MachineProperties::default();
        for (key, value) in [
            ("ActiveState", "failed"),
            ("SubState", "failed"),
            ("Result", "exit-code"),
            ("ExecMainStatus", "1"),
        ] {
            failed.insert(
                crate::nspawn::models::GROUP_SYSTEMD_UNIT,
                key.into(),
                value.into(),
            );
        }
        assert!(matches!(
            observe_start(&failed),
            StartObservation::Failed(_)
        ));
        assert!(valid_invocation_id("0123456789abcdef0123456789ABCDEF"));
        assert!(!valid_invocation_id("[not set]"));
    }
}

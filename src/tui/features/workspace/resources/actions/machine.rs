//! Dispatch and presentation of machine lifecycle operations.
use crate::application::machine_lifecycle::MachineOperation;
use crate::application::MachineLifecycleAction;
use crate::tui::features::workspace::resources::actions::ResourceActionServices;
use crate::tui::protocol::events::AppEvent;
use tokio::sync::mpsc::Sender;

pub(crate) fn submit(
    operation: MachineOperation,
    services: &ResourceActionServices,
    tx: Sender<AppEvent>,
) {
    let permissions = services.permissions.clone();
    let host_operation = services.host_operations.begin();
    tokio::spawn(async move {
        let _host_operation = host_operation;
        let outcome = match permissions
            .request_elevation(format!(
                "{} {}",
                operation.action().audit_label(),
                operation.machine()
            ))
            .await
        {
            Ok(audit) => audit.run(operation.run()).await,
            Err(error) => operation.not_attempted(error.to_string()),
        };
        let _ = tx.send(AppEvent::MachineOperationFinished(outcome)).await;
    });
}

pub(crate) fn machine_action_can_change_x11_lifecycle(
    action: crate::application::MachineLifecycleAction,
) -> bool {
    matches!(
        action,
        crate::application::MachineLifecycleAction::Runtime(
            crate::application::MachineRuntimeAction::Terminate
                | crate::application::MachineRuntimeAction::Poweroff
                | crate::application::MachineRuntimeAction::Reboot
                | crate::application::MachineRuntimeAction::Kill { .. }
        )
    )
}

pub(crate) fn machine_outcome_status(
    outcome: crate::application::MachineLifecycleOutcome,
) -> (String, crate::tui::foundation::status::StatusLevel) {
    use crate::application::MachineLifecycleResult;

    let fallback = outcome
        .fallback
        .map(|fallback| format!(" (systemd tools fallback: {})", fallback.reason))
        .unwrap_or_default();
    let machine = outcome.machine.as_str();
    match outcome.result {
        MachineLifecycleResult::Succeeded => (
            format!("{} {}{}", outcome.action.success_label(), machine, fallback),
            crate::tui::foundation::status::StatusLevel::Success,
        ),
        MachineLifecycleResult::NotAttempted(reason) => (
            format!(
                "{} {} was not attempted: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::tui::foundation::status::StatusLevel::Error,
        ),
        MachineLifecycleResult::Rejected { reason, .. } => (
            format!(
                "{} {} was rejected: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::tui::foundation::status::StatusLevel::Warn,
        ),
        MachineLifecycleResult::Failed(reason) => (
            format!(
                "{} {} failed: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::tui::foundation::status::StatusLevel::Error,
        ),
        MachineLifecycleResult::OutcomeUnknown(reason) => (
            format!(
                "{} {} outcome is unknown: {}{}",
                outcome.action.audit_label(),
                machine,
                reason,
                fallback
            ),
            crate::tui::foundation::status::StatusLevel::Warn,
        ),
    }
}

pub(crate) fn reconcile_x11(
    action: MachineLifecycleAction,
    access: std::sync::Arc<crate::application::x11::X11AccessService>,
) {
    if !machine_action_can_change_x11_lifecycle(action) {
        return;
    }
    tokio::spawn(async move {
        match access.reconcile_after_machine_event().await {
            Ok(_) => {
                if let Err(error) = access.synchronize_reconcile_activation().await {
                    log::debug!(
                        "X11 lifecycle activation sync after machine action unavailable: {error}"
                    );
                }
            }
            Err(error) => {
                log::debug!("X11 lifecycle reconcile after machine action unavailable: {error}")
            }
        }
    });
}

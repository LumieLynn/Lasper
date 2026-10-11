//! Owns a wizard instance together with work started on its behalf.
//!
//! Wizard operations are asynchronous, but their results are meaningful only
//! while the corresponding wizard is open. Keeping the task handles beside
//! the wizard gives the overlay a real lifetime boundary: closing it aborts
//! pending discovery/validation work instead of leaving detached tasks behind.
//! Accepted host mutations finish independently of the overlay, matching the
//! deployment service and configuration apply lifetime rules.

use super::manager::Wizard;
use crate::tui::features::wizard::workflow::{
    contain_task, WizardCompletion, WizardEffect, WizardEvent, WizardServices,
};
use crate::tui::features::wizard::{StepAction, WizardInstanceId};
use crate::tui::foundation::status::StatusLevel;
use crate::tui::protocol::events::AppEvent;
use crate::tui::protocol::{AppMessage, WizardMessage};
use std::ops::{Deref, DerefMut};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

pub struct WizardSession {
    wizard: Wizard,
    tasks: WizardTasks,
    next_query: u64,
    pending_interface: Option<u64>,
    pending_preflight: Option<u64>,
}

impl WizardSession {
    pub fn new(wizard: Wizard) -> Self {
        Self {
            wizard,
            tasks: WizardTasks::default(),
            next_query: 0,
            pending_interface: None,
            pending_preflight: None,
        }
    }

    pub fn id(&self) -> WizardInstanceId {
        self.wizard.id()
    }

    fn track_task(&mut self, task: JoinHandle<()>) {
        self.tasks.track_read(task);
    }

    fn next_query(&mut self) -> u64 {
        self.next_query = self.next_query.wrapping_add(1).max(1);
        self.next_query
    }

    pub(crate) fn discover_hardware(
        &mut self,
        services: &WizardServices,
        tx: Option<Sender<AppEvent>>,
    ) {
        let Some(tx) = tx else {
            self.wizard.draft.passthrough.hardware_scanning = false;
            return;
        };
        let id = self.id();
        let preparation = services.preparation.clone();
        self.track_task(tokio::spawn(async move {
            let result = contain_task(
                "provisioning hardware discovery",
                preparation.discover_hardware(),
            )
            .await;
            let _ = tx
                .send(AppEvent::Wizard(WizardEvent::Completed {
                    id,
                    completion: WizardCompletion::Hardware(result),
                }))
                .await;
        }));
    }

    /// Execute work against narrow service handles, without borrowing App.
    pub(crate) fn dispatch(
        &mut self,
        action: StepAction,
        services: &WizardServices,
        tx: Option<Sender<AppEvent>>,
    ) -> WizardEffect {
        match action {
            StepAction::None | StepAction::Next | StepAction::Prev => WizardEffect::None,
            StepAction::Close => WizardEffect::Close,
            StepAction::Status(message, level) => WizardEffect::Status(message, level),
            StepAction::OpenDialog(dialog) => WizardEffect::OpenDialog(dialog),
            StepAction::CloseDialog => WizardEffect::CloseDialog,
            StepAction::ValidateInterface {
                name,
                is_bridge_mode,
            } => {
                let Some(tx) = tx else {
                    return self.dispatch_failed(false);
                };
                let query = self.next_query();
                self.pending_interface = Some(query);
                let id = self.id();
                let preparation = services.preparation.clone();
                self.track_task(tokio::spawn(async move {
                    let result = contain_task(
                        "provisioning interface validation",
                        preparation.validate_interface(&name, is_bridge_mode),
                    )
                    .await;
                    let _ = tx
                        .send(AppEvent::Wizard(WizardEvent::Completed {
                            id,
                            completion: WizardCompletion::Interface { query, result },
                        }))
                        .await;
                }));
                WizardEffect::None
            }
            StepAction::PreflightDeployment(request) => {
                let Some(tx) = tx else {
                    return self.dispatch_failed(true);
                };
                let query = self.next_query();
                self.pending_preflight = Some(query);
                let id = self.id();
                let service = services.provisioning.clone();
                self.track_task(tokio::spawn(async move {
                    let result =
                        contain_task("deployment preflight", service.preflight(&request)).await;
                    let _ = tx
                        .send(AppEvent::Wizard(WizardEvent::Completed {
                            id,
                            completion: WizardCompletion::Preflight {
                                query,
                                request: Box::new(request),
                                result,
                            },
                        }))
                        .await;
                }));
                WizardEffect::CloseDialog
            }
            StepAction::StartDeployment(submission) => {
                self.pending_preflight = None;
                match services.provisioning.start(submission) {
                    Ok(handle) => {
                        log::info!("[DEPLOY] accepted application job {}", handle.id());
                        self.wizard.start_deployment(handle);
                        WizardEffect::CloseDialog
                    }
                    Err(error) => {
                        let (error, submission) = error.into_parts();
                        self.wizard.draft.restore_submission(submission);
                        self.wizard.loading = false;
                        WizardEffect::Status(
                            format!("Deployment rejected: {error}"),
                            StatusLevel::Error,
                        )
                    }
                }
            }
            StepAction::ReleaseUnresolvedDeployment(deployment_id) => {
                let Some(tx) = tx else {
                    return self.dispatch_failed(false);
                };
                let id = self.id();
                let service = services.provisioning.clone();
                let operation = services.host_operations.begin();
                self.tasks.track_mutation(tokio::spawn(async move {
                    let _operation = operation;
                    let result = contain_task(
                        "deployment claim release",
                        service.release_unresolved(deployment_id, true),
                    )
                    .await;
                    let _ = tx
                        .send(AppEvent::Wizard(WizardEvent::Completed {
                            id,
                            completion: WizardCompletion::ClaimRelease {
                                deployment_id,
                                result,
                            },
                        }))
                        .await;
                }));
                WizardEffect::None
            }
        }
    }

    fn dispatch_failed(&mut self, preflight: bool) -> WizardEffect {
        if preflight {
            self.pending_preflight = None;
            self.wizard.preflight_dispatch_failed();
        } else {
            self.wizard.loading = false;
        }
        WizardEffect::Status(
            "Internal error: application event channel is unavailable".into(),
            StatusLevel::Error,
        )
    }

    /// Both instance and request identity must match before changing the draft.
    pub(crate) fn complete(
        &mut self,
        id: WizardInstanceId,
        completion: WizardCompletion,
    ) -> Option<StepAction> {
        if id != self.id() {
            return None;
        }
        let action = match completion {
            WizardCompletion::Hardware(result) => self.wizard.finish_hardware_discovery(result),
            WizardCompletion::Interface { query, result } => {
                if self.pending_interface != Some(query) {
                    return None;
                }
                self.pending_interface = None;
                self.wizard.finish_interface_validation(result)
            }
            WizardCompletion::Preflight {
                query,
                request,
                result,
            } => {
                if self.pending_preflight != Some(query) {
                    return None;
                }
                self.pending_preflight = None;
                self.wizard.finish_preflight(*request, result)
            }
            WizardCompletion::ClaimRelease {
                deployment_id,
                result,
            } => self.wizard.process_message(AppMessage::Wizard(
                WizardMessage::UnresolvedDeploymentReleaseFinished {
                    deployment_id,
                    error: result.err().map(|error| error.to_string()),
                },
            )),
        };
        Some(action)
    }

    #[cfg(test)]
    pub(crate) fn pending_preflight(&self) -> Option<u64> {
        self.pending_preflight
    }

    /// Cancel work owned by the session and wait until Tokio has observed the
    /// cancellation.  The synchronous `Drop` implementation remains as a
    /// fallback for panic/error paths, but normal UI closure uses this method
    /// so resources are not left pending for a later scheduler turn.
    pub async fn shutdown(&mut self) {
        self.tasks.shutdown().await;
    }
}

/// Read cancellation and mutation completion are different lifetime policies.
#[derive(Default)]
struct WizardTasks {
    reads: Vec<JoinHandle<()>>,
    mutations: Vec<JoinHandle<()>>,
}

impl WizardTasks {
    fn track_read(&mut self, task: JoinHandle<()>) {
        self.reads.retain(|task| !task.is_finished());
        self.reads.push(task);
    }

    fn track_mutation(&mut self, task: JoinHandle<()>) {
        self.mutations.retain(|task| !task.is_finished());
        self.mutations.push(task);
    }

    async fn shutdown(&mut self) {
        // Detach accepted mutations; dropping a JoinHandle does not abort it.
        self.mutations.clear();
        let tasks = std::mem::take(&mut self.reads);
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Deref for WizardSession {
    type Target = Wizard;

    fn deref(&self) -> &Self::Target {
        &self.wizard
    }
}

impl DerefMut for WizardSession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.wizard
    }
}

impl Drop for WizardTasks {
    fn drop(&mut self) {
        for task in &self.reads {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WizardTasks;
    use crate::application::HostOperationTracker;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn closing_cancels_reads_but_keeps_an_accepted_mutation_alive_until_completion() {
        struct Resource(Option<oneshot::Sender<()>>);
        impl Drop for Resource {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let mut tasks = WizardTasks::default();
        let (read_started_tx, read_started_rx) = oneshot::channel();
        let (read_dropped_tx, read_dropped_rx) = oneshot::channel();
        let resource = Resource(Some(read_dropped_tx));
        tasks.track_read(tokio::spawn(async move {
            let _resource = resource;
            let _ = read_started_tx.send(());
            std::future::pending::<()>().await;
        }));

        let tracker = HostOperationTracker::default();
        let operation = tracker.begin();
        let (finish_tx, finish_rx) = oneshot::channel();
        let (finished_tx, finished_rx) = oneshot::channel();
        tasks.track_mutation(tokio::spawn(async move {
            let _operation = operation;
            finish_rx.await.unwrap();
            drop(_operation);
            let _ = finished_tx.send(());
        }));
        read_started_rx.await.unwrap();
        tasks.shutdown().await;
        read_dropped_rx.await.unwrap();
        drop(tasks);
        assert_eq!(tracker.active_count(), 1);
        finish_tx.send(()).unwrap();
        finished_rx.await.unwrap();
        assert_eq!(tracker.active_count(), 0);
    }
}

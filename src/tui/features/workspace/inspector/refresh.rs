//! Revisioned background reads for the main detail panel.
//!
//! Input handlers only replace the pending ticket. The application loop runs
//! at most one read at a time and applies its result only when the ticket still
//! describes the visible target and pane.

use crate::application::inspection::{
    ImageUnitInspection, NspawnConfigInspection, ResourceInspectionService,
};
use crate::application::sessions::{JournalSessionHandle, SessionService};
use crate::application::{RuntimeCatalog, RuntimeQuery};
use crate::domain::inspection::MachineProperties;
use crate::domain::runtime::MachineEntry;
use crate::tui::features::workspace::inspector::{DetailPane, DetailTarget};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DetailRefreshTicket {
    pub revision: u64,
    pub target: DetailTarget,
    pub pane: DetailPane,
}

#[derive(Default)]
pub(crate) struct DetailRefreshState {
    next_revision: u64,
    pending: Option<DetailRefreshTicket>,
    in_flight: Option<DetailRefreshTicket>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl DetailRefreshState {
    pub fn request(&mut self, target: DetailTarget, pane: DetailPane) -> u64 {
        self.next_revision = self.next_revision.wrapping_add(1).max(1);
        let ticket = DetailRefreshTicket {
            revision: self.next_revision,
            target,
            pane,
        };
        self.pending = Some(ticket);
        self.next_revision
    }

    pub fn take_pending(&mut self) -> Option<DetailRefreshTicket> {
        if self.in_flight.is_some() {
            return None;
        }
        let ticket = self.pending.take()?;
        self.in_flight = Some(ticket.clone());
        Some(ticket)
    }

    /// Release only the matching task's slot. A queued refresh of the same
    /// target does not invalidate useful data from the read already running.
    pub fn finish(&mut self, revision: u64) -> bool {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|ticket| ticket.revision == revision)
        {
            self.in_flight = None;
            self.task.take();
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn track_task(&mut self, task: tokio::task::JoinHandle<()>) {
        self.task = Some(task);
    }

    pub(crate) fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.in_flight = None;
        self.pending = None;
    }

    pub(crate) async fn shutdown(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.cancel();
    }
}

impl Drop for DetailRefreshState {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(crate) enum PreparedDetailRefresh {
    Ready(DetailRefreshCompletion),
    Job(DetailRefreshJob),
}

#[derive(Clone)]
pub(crate) struct DetailRefreshServices {
    pub runtime_catalog: Arc<RuntimeCatalog>,
    pub session_service: Arc<SessionService>,
    pub resource_inspection: Arc<ResourceInspectionService>,
}

pub(crate) enum DetailRefreshWork {
    MachineProperties { name: String, entry: MachineEntry },
    Journal { name: String },
    MachineConfig { entry: MachineEntry },
    ImageConfig { name: String },
    ImageUnit { name: String },
}

pub(crate) struct DetailRefreshJob {
    pub ticket: DetailRefreshTicket,
    pub work: DetailRefreshWork,
}

impl DetailRefreshJob {
    pub async fn execute(self, services: DetailRefreshServices) -> DetailRefreshCompletion {
        let result = match self.work {
            DetailRefreshWork::MachineProperties { name, entry } => {
                DetailRefreshResult::MachineProperties(
                    services
                        .runtime_catalog
                        .inspect(&name, &entry)
                        .await
                        .map_err(|error| error.to_string()),
                )
            }
            DetailRefreshWork::Journal { name } => {
                let result = match crate::domain::machine::MachineName::new(&name) {
                    Ok(machine) => match services.session_service.open_journal(machine).await {
                        Ok(handle) => JournalRefreshResult::opened(handle),
                        Err(error) => JournalRefreshResult::failed(
                            error.to_string(),
                            error.hint().map(str::to_string),
                        ),
                    },
                    Err(error) => JournalRefreshResult::failed(error.to_string(), None),
                };
                DetailRefreshResult::Journal(result)
            }
            DetailRefreshWork::MachineConfig { entry } => {
                let config = services
                    .resource_inspection
                    .inspect_machine_nspawn_config(&entry)
                    .await;
                DetailRefreshResult::Config(config)
            }
            DetailRefreshWork::ImageConfig { name } => {
                let config = services
                    .resource_inspection
                    .inspect_image_nspawn_config(&name)
                    .await;
                DetailRefreshResult::Config(config)
            }
            DetailRefreshWork::ImageUnit { name } => DetailRefreshResult::ImageUnit(
                services.resource_inspection.inspect_image_unit(&name).await,
            ),
        };
        DetailRefreshCompletion {
            ticket: self.ticket,
            result,
        }
    }
}

#[derive(Debug)]
pub(crate) struct DetailRefreshCompletion {
    pub ticket: DetailRefreshTicket,
    pub result: DetailRefreshResult,
}

#[derive(Debug)]
pub(crate) enum DetailRefreshResult {
    Empty,
    Noop,
    MachineProperties(Result<RuntimeQuery<MachineProperties>, String>),
    Journal(JournalRefreshResult),
    Config(
        Result<
            Option<NspawnConfigInspection>,
            crate::application::inspection::ResourceInspectionError,
        >,
    ),
    ImageUnit(ImageUnitInspection),
}

pub(crate) struct JournalRefreshResult {
    pub handle: Option<JournalSessionHandle>,
    pub error: Option<String>,
    pub hint: Option<String>,
}

impl JournalRefreshResult {
    fn opened(handle: JournalSessionHandle) -> Self {
        Self {
            handle: Some(handle),
            error: None,
            hint: None,
        }
    }

    fn failed(error: String, hint: Option<String>) -> Self {
        Self {
            handle: None,
            error: Some(error),
            hint,
        }
    }
}

impl std::fmt::Debug for JournalRefreshResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JournalRefreshResult")
            .field("opened", &self.handle.is_some())
            .field("error", &self.error)
            .field("hint", &self.hint)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(target: &str, pane: DetailPane) -> DetailRefreshTicket {
        DetailRefreshTicket {
            revision: 0,
            target: DetailTarget::Machine(target.into()),
            pane,
        }
    }

    #[test]
    fn pending_requests_coalesce_to_the_latest_visible_detail() {
        let mut state = DetailRefreshState::default();
        state.request(
            ticket("first", DetailPane::MachineOverview).target,
            DetailPane::MachineOverview,
        );
        state.request(
            ticket("second", DetailPane::Config).target,
            DetailPane::Config,
        );

        let pending = state.take_pending().unwrap();
        assert_eq!(pending.target, DetailTarget::Machine("second".into()));
        assert_eq!(pending.pane, DetailPane::Config);
        assert!(!state.has_pending());
    }

    #[test]
    fn only_the_current_in_flight_revision_can_release_the_slot() {
        let mut state = DetailRefreshState::default();
        let revision = state.request(
            DetailTarget::Machine("machine".into()),
            DetailPane::MachineOverview,
        );
        let _ = state.take_pending().unwrap();

        assert!(!state.finish(revision.wrapping_add(1)));
        assert!(state.finish(revision));
    }

    #[test]
    fn pending_refresh_does_not_starve_the_running_read_of_the_same_target() {
        let mut state = DetailRefreshState::default();
        let target = DetailTarget::Machine("machine".into());
        let first = state.request(target.clone(), DetailPane::Details);
        state.take_pending().unwrap();
        let latest = state.request(target, DetailPane::Details);
        assert!(state.finish(first));
        assert_eq!(state.take_pending().unwrap().revision, latest);
        assert!(!state.finish(first));
        assert!(state.finish(latest));
    }

    #[tokio::test]
    async fn cancelling_a_read_releases_its_captured_resources_and_allows_the_next_target() {
        struct OnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let mut state = DetailRefreshState::default();
        let first = state.request(DetailTarget::Machine("first".into()), DetailPane::Details);
        state.take_pending().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let resource = OnDrop(Some(dropped_tx));
        state.track_task(tokio::spawn(async move {
            let _resource = resource;
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        }));
        started_rx.await.unwrap();
        state.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .unwrap()
            .unwrap();
        let latest = state.request(DetailTarget::Machine("second".into()), DetailPane::Config);
        assert_eq!(state.take_pending().unwrap().revision, latest);
        assert!(!state.finish(first));
        assert!(state.finish(latest));
    }
}

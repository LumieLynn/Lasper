//! Revisioned background reads for the main detail panel.
//!
//! Input handlers only replace the pending ticket. The application loop runs
//! at most one read at a time and applies its result only when the ticket still
//! describes the visible target and pane.

use crate::application::sessions::{JournalSessionHandle, SessionService};
use crate::nspawn::adapters::comm::inspection::MachineInspectionStore;
use crate::nspawn::adapters::config::systemd_unit::SystemdUnitInspection;
use crate::nspawn::adapters::config::{NspawnConfigStore, SystemdUnitStore};
use crate::nspawn::models::{ContainerEntry, MachineProperties};
use crate::nspawn::ops::{RuntimeCatalog, RuntimeQuery};
use crate::ui::views::detail_panel::{DetailPane, DetailTarget};
use std::path::PathBuf;
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

    /// Finish only the task which currently owns the in-flight slot.
    pub fn finish(&mut self, revision: u64) -> bool {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|ticket| ticket.revision == revision)
        {
            self.in_flight = None;
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[derive(Clone)]
pub(crate) struct DetailRefreshServices {
    pub runtime_catalog: Arc<RuntimeCatalog>,
    pub session_service: Arc<SessionService>,
    pub machine_inspection: MachineInspectionStore,
    pub nspawn: NspawnConfigStore,
    pub systemd_unit: SystemdUnitStore,
}

pub(crate) enum DetailRefreshWork {
    MachineProperties { name: String, entry: ContainerEntry },
    Journal { name: String },
    Config { name: String },
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
            DetailRefreshWork::Config { name } => {
                let config = services
                    .nspawn
                    .inspect(&name)
                    .await
                    .map(|config| {
                        config.map(|config| ConfigSnapshot {
                            path: config.path,
                            content: config.content,
                        })
                    })
                    .map_err(|error| error.to_string());
                DetailRefreshResult::Config(config)
            }
            DetailRefreshWork::ImageUnit { name } => {
                let properties = services
                    .machine_inspection
                    .inspect_static(&name)
                    .await
                    .map_err(|error| error.to_string());
                let unit = if matches!(properties, Ok(None)) {
                    None
                } else {
                    Some(
                        services
                            .systemd_unit
                            .read(&name)
                            .await
                            .map_err(|error| error.to_string()),
                    )
                };
                DetailRefreshResult::ImageUnit(ImageUnitRefreshResult { properties, unit })
            }
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
    Config(Result<Option<ConfigSnapshot>, String>),
    ImageOverview(MachineProperties),
    ImageUnit(ImageUnitRefreshResult),
}

#[derive(Debug)]
pub(crate) struct ConfigSnapshot {
    pub path: PathBuf,
    pub content: String,
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

#[derive(Debug)]
pub(crate) struct ImageUnitRefreshResult {
    pub properties: Result<Option<MachineProperties>, String>,
    pub unit: Option<Result<SystemdUnitInspection, String>>,
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
            ticket("first", DetailPane::Properties).target,
            DetailPane::Properties,
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
            DetailPane::Properties,
        );
        let _ = state.take_pending().unwrap();

        assert!(!state.finish(revision.wrapping_add(1)));
        assert!(state.finish(revision));
    }
}

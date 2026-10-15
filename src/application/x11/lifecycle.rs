//! Work selection and results for caller-owned X11 machine lifecycle cleanup.
//!
//! Targets come from persisted Lasper records, not desktop-wide discovery.
//! Keeping the watcher active and retrying one observation are separate
//! decisions: a record needing manual review still needs a watcher, but
//! repeating its failed ownership check cannot make mutation safe.

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct X11ReconcileTarget {
    pub(crate) display: u16,
    pub(crate) alternate_endpoint: bool,
    pub(crate) source: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum X11ReconcileRetry {
    #[default]
    None,
    /// A lifecycle event can arrive before machined removes its registration.
    RegisteredMachine,
    /// An absence needs its grace period, or a machine read was inconclusive.
    PendingObservation,
}

/// Result for one endpoint selected from local lifecycle records. Diagnostics
/// retain uncovered or unsafe work; a successful display is not proof that
/// another recorded endpoint was reconciled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct X11ReconcileReport {
    display: u16,
    revoked_record_ids: Vec<String>,
    pending_record_ids: Vec<String>,
    diagnostics: Vec<String>,
    retry: X11ReconcileRetry,
}

impl X11ReconcileReport {
    pub(crate) fn new(
        display: u16,
        revoked_record_ids: Vec<String>,
        pending_record_ids: Vec<String>,
        diagnostics: Vec<String>,
    ) -> Self {
        Self {
            display,
            revoked_record_ids,
            pending_record_ids,
            diagnostics,
            retry: X11ReconcileRetry::None,
        }
    }

    pub(crate) fn with_retry(mut self, retry: X11ReconcileRetry) -> Self {
        self.retry = retry;
        self
    }

    pub(crate) const fn display(&self) -> u16 {
        self.display
    }

    pub(crate) fn revoked_record_ids(&self) -> &[String] {
        &self.revoked_record_ids
    }

    pub(crate) fn pending_record_ids(&self) -> &[String] {
        &self.pending_record_ids
    }

    pub(crate) fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub(crate) const fn retry(&self) -> X11ReconcileRetry {
        self.retry
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.pending_record_ids.is_empty()
            && self.diagnostics.is_empty()
            && self.retry == X11ReconcileRetry::None
    }

    /// Keep successful mutations from earlier passes, while pending work and
    /// diagnostics describe the latest observation of this same endpoint.
    pub(crate) fn update_from_pass(&mut self, next: Self) {
        for record in next.revoked_record_ids {
            if !self.revoked_record_ids.contains(&record) {
                self.revoked_record_ids.push(record);
            }
        }
        self.pending_record_ids = next.pending_record_ids;
        self.diagnostics = next.diagnostics;
        self.retry = next.retry;
    }
}

//! Public control handle backed by the session driver's single command sender.

use super::{RuntimeInfo, StartOptions};
use crate::{
    BrokerSession, BrokerSessionError, Completion, SessionSnapshot, StartError, StartOutcome,
    WorkerObservation,
};
use std::fmt;
use tokio::{
    sync::{watch, Mutex},
    time::Instant,
};

/// Unique control owner. Drop wakes the driver to request cleanup without waiting
/// for its report. Data descriptors and snapshot receivers do not keep it alive.
pub struct Session {
    pub(super) inner: BrokerSession,
    pub(super) info: RuntimeInfo,
    pub(super) close_once: Mutex<bool>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl Session {
    pub fn runtime_info(&self) -> &RuntimeInfo {
        &self.info
    }

    pub fn observation(&self) -> &WorkerObservation {
        self.inner.observation()
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        self.inner.snapshot()
    }

    pub fn subscribe(&self) -> watch::Receiver<SessionSnapshot> {
        self.inner.subscribe()
    }

    pub async fn recheck(
        &mut self,
        deadline: Instant,
    ) -> Result<&WorkerObservation, BrokerSessionError> {
        self.inner.recheck_until(deadline).await
    }

    /// Rechecks identity before submitting Start. Acceptance does not confirm
    /// exec; Unknown cannot be retried safely. Cancelling after submission asks
    /// the driver to close the session because execution may already have begun.
    pub async fn start(
        &mut self,
        options: StartOptions,
        deadline: Instant,
    ) -> Result<StartOutcome, StartError> {
        self.inner.start_until(options, deadline).await
    }

    /// Requests cleanup once and waits for its report. Drain data descriptors
    /// concurrently so a command writing output can make progress.
    pub async fn close(&self) -> Completion {
        {
            let mut closed = self.close_once.lock().await;
            if !*closed {
                let snapshot = self.snapshot();
                if !matches!(
                    snapshot.phase,
                    crate::SessionPhase::Closing | crate::SessionPhase::Finished
                ) {
                    let _ = if snapshot.submission == crate::StartSubmission::NotSubmitted {
                        self.inner.cancel().await
                    } else {
                        self.inner.close().await
                    };
                }
                *closed = true;
            }
        }
        self.wait().await
    }

    /// Waits for the final report or the end of host observation. An error
    /// means the host could not confirm cleanup, not that guest cleanup failed.
    pub async fn wait(&self) -> Completion {
        self.inner.wait().await
    }
}

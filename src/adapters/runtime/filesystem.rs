//! Filesystem change hints; refresh policy belongs to the runtime application.

use crate::application::runtime::{RuntimeChangePort, RuntimeError, RuntimeResult};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

pub(super) struct FilesystemChanges {
    paths: Vec<PathBuf>,
}

impl FilesystemChanges {
    pub(super) fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }
}

#[async_trait::async_trait]
impl RuntimeChangePort for FilesystemChanges {
    async fn watch_changes(&self, changed: watch::Sender<()>) -> RuntimeResult<()> {
        // Only one pending invalidation is needed, regardless of event count.
        let (notify_tx, mut notify_rx) = mpsc::channel(1);
        let mut watcher = RecommendedWatcher::new(
            move |result: Result<Event, notify::Error>| {
                if let Ok(event) = result {
                    if matches!(
                        event.kind,
                        EventKind::Create(_)
                            | EventKind::Remove(_)
                            | EventKind::Modify(notify::event::ModifyKind::Name(_))
                    ) {
                        let _ = notify_tx.try_send(());
                    }
                }
            },
            Config::default(),
        )
        .map_err(|error| RuntimeError::failed(format!("create filesystem watcher: {error}")))?;

        for path in &self.paths {
            if path.exists() {
                if let Err(error) = watcher.watch(path, RecursiveMode::NonRecursive) {
                    log::warn!(
                        "Runtime filesystem watcher unavailable for {}: {error}",
                        path.display()
                    );
                }
            }
        }
        while notify_rx.recv().await.is_some() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            while notify_rx.try_recv().is_ok() {}
            if changed.send(()).is_err() {
                return Ok(());
            }
        }
        Ok(())
    }
}

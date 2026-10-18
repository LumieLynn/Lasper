//! One refresh loop owns snapshot publication, polling and failure recovery.

use super::{RuntimeCatalog, RuntimeChangePort, RuntimeQuery, RuntimeResult, RuntimeUpdate};
use crate::domain::runtime::RuntimeSnapshot;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(crate) struct RuntimeObservers {
    pub backend: Option<Arc<dyn RuntimeChangePort>>,
    pub filesystem: Option<Arc<dyn RuntimeChangePort>>,
}

/// Owns all observation tasks. Drop cancels them; shutdown also awaits release.
#[must_use = "keep the runtime observation alive until its consumer stops"]
pub struct RuntimeObservation {
    tasks: JoinSet<()>,
}

impl RuntimeObservation {
    pub(super) fn start(catalog: Arc<RuntimeCatalog>, tx: mpsc::Sender<RuntimeUpdate>) -> Self {
        let mut tasks = JoinSet::new();
        let (backend_done_tx, backend_done_rx) = oneshot::channel();
        let backend_running = catalog.observers.backend.is_some();

        if let Some(backend) = &catalog.observers.backend {
            let backend = Arc::clone(backend);
            let changed = catalog.invalidation_tx.clone();
            tasks.spawn(async move {
                let _ = backend_done_tx.send(backend.watch_changes(changed).await);
            });
        }
        if let Some(filesystem) = &catalog.observers.filesystem {
            let filesystem = Arc::clone(filesystem);
            let changed = catalog.invalidation_tx.clone();
            tasks.spawn(async move {
                if let Err(error) = filesystem.watch_changes(changed).await {
                    log::warn!(
                        "Runtime filesystem observation unavailable ({error}); continuing with periodic refresh"
                    );
                }
            });
        }

        let invalidations = catalog.invalidation_tx.subscribe();
        tasks.spawn(refresh_catalog(
            catalog,
            tx,
            invalidations,
            backend_done_rx,
            backend_running,
        ));
        Self { tasks }
    }

    pub async fn shutdown(mut self) {
        self.tasks.shutdown().await;
    }
}

async fn refresh_catalog(
    catalog: Arc<RuntimeCatalog>,
    tx: mpsc::Sender<RuntimeUpdate>,
    mut invalidations: watch::Receiver<()>,
    mut backend_done: oneshot::Receiver<RuntimeResult<()>>,
    mut backend_running: bool,
) {
    let mut state = RefreshState::default();
    let mut publish_unchanged = true;
    loop {
        // Coalesce changes already pending. A change during the query remains
        // unread and causes another pass, rather than being lost on completion.
        publish_unchanged |= invalidations.has_changed().unwrap_or(false);
        invalidations.borrow_and_update();
        let result = tokio::select! {
            _ = tx.closed() => return,
            result = catalog.snapshot() => result,
        };
        let heartbeat = backend_running && result.as_ref().is_ok_and(|query| query.route.is_dbus());
        let interval = if heartbeat {
            HEARTBEAT_INTERVAL
        } else {
            POLL_INTERVAL
        };
        if let Some(update) = state.update(result, publish_unchanged) {
            if tx.send(update).await.is_err() {
                return;
            }
        }

        tokio::select! {
            _ = tx.closed() => return,
            _ = tokio::time::sleep(interval) => publish_unchanged = heartbeat,
            changed = invalidations.changed() => {
                if changed.is_err() {
                    return;
                }
                publish_unchanged = true;
            }
            result = &mut backend_done, if backend_running => {
                backend_running = false;
                publish_unchanged = true;
                match result {
                    Ok(Err(error)) => log::warn!(
                        "Runtime backend observation unavailable ({error}); continuing with periodic refresh"
                    ),
                    _ => log::warn!("Runtime backend observation stopped; continuing with periodic refresh"),
                }
            }
        }
    }
}

#[derive(Default)]
struct RefreshState {
    previous: Option<RuntimeQuery<RuntimeSnapshot>>,
    consecutive_failures: u32,
}

impl RefreshState {
    fn update(
        &mut self,
        result: RuntimeResult<RuntimeQuery<RuntimeSnapshot>>,
        publish_unchanged: bool,
    ) -> Option<RuntimeUpdate> {
        match result {
            Ok(query) => {
                let recovered = self.consecutive_failures > 0;
                self.consecutive_failures = 0;
                let changed = self.previous.as_ref() != Some(&query);
                if changed {
                    self.previous = Some(query.clone());
                }
                // Requested refreshes and host changes also reconcile pending
                // lifecycle transitions. Only background polling may dedupe.
                if publish_unchanged || recovered || changed {
                    Some(RuntimeUpdate::Snapshot(query))
                } else {
                    None
                }
            }
            Err(error) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                Some(RuntimeUpdate::BackendFailure {
                    message: error.to_string(),
                    consecutive_failures: self.consecutive_failures,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::operations::ExecutionRoute;
    use crate::application::runtime::{MockRuntimePort, RuntimeError, RuntimePort};
    use crate::domain::inspection::MachineProperties;
    use crate::domain::machine::MachineName;
    use crate::domain::runtime::MachineEntry;

    type SnapshotReply = oneshot::Sender<RuntimeResult<RuntimeSnapshot>>;
    type Subscription = (watch::Sender<()>, oneshot::Sender<RuntimeResult<()>>);

    struct ControlledReads {
        route: ExecutionRoute,
        queries: mpsc::Sender<SnapshotReply>,
    }

    #[async_trait::async_trait]
    impl RuntimePort for ControlledReads {
        fn snapshot_route(&self) -> ExecutionRoute {
            self.route
        }

        fn inspection_route(&self) -> ExecutionRoute {
            self.route
        }

        async fn is_available(&self) -> bool {
            true
        }

        async fn snapshot(&self) -> RuntimeResult<RuntimeSnapshot> {
            let (reply, response) = oneshot::channel();
            self.queries.send(reply).await.unwrap();
            response.await.unwrap()
        }

        async fn list_machines(&self) -> RuntimeResult<Vec<MachineEntry>> {
            unreachable!("observation reads whole snapshots")
        }

        async fn inspect(
            &self,
            _: &MachineName,
            _: &MachineEntry,
        ) -> RuntimeResult<MachineProperties> {
            unreachable!("observation does not inspect individual machines")
        }

        async fn inspect_unit(&self, _: &MachineName) -> RuntimeResult<MachineProperties> {
            unreachable!("observation does not inspect individual units")
        }
    }

    struct ControlledChanges(mpsc::Sender<Subscription>);

    #[async_trait::async_trait]
    impl RuntimeChangePort for ControlledChanges {
        async fn watch_changes(&self, changed: watch::Sender<()>) -> RuntimeResult<()> {
            let (finish, done) = oneshot::channel();
            self.0.send((changed, finish)).await.unwrap();
            done.await.unwrap()
        }
    }

    fn catalog(
        route: ExecutionRoute,
        observers: RuntimeObservers,
    ) -> (Arc<RuntimeCatalog>, mpsc::Receiver<SnapshotReply>) {
        let (queries, rx) = mpsc::channel(1);
        let reads = Arc::new(ControlledReads { route, queries });
        let catalog = if route.is_dbus() {
            RuntimeCatalog::new(Some(reads), Arc::new(MockRuntimePort::new()), observers)
        } else {
            RuntimeCatalog::new(None, reads, observers)
        };
        (Arc::new(catalog), rx)
    }

    fn empty_snapshot() -> RuntimeSnapshot {
        RuntimeSnapshot::new(vec![], vec![])
    }

    fn query() -> RuntimeQuery<RuntimeSnapshot> {
        RuntimeQuery {
            value: empty_snapshot(),
            route: ExecutionRoute::LocalSystemdTools,
            fallback: None,
        }
    }

    #[test]
    fn publication_preserves_refreshes_route_changes_and_failure_recovery() {
        let mut state = RefreshState::default();
        assert!(state.update(Ok(query()), false).is_some());
        assert!(state.update(Ok(query()), false).is_none());
        assert!(state.update(Ok(query()), true).is_some());

        let mut changed_route = query();
        changed_route.route = ExecutionRoute::DirectDbus;
        assert!(state.update(Ok(changed_route.clone()), false).is_some());
        for count in 1..=2 {
            assert!(matches!(
                state.update(Err(RuntimeError::failed("query failed")), false),
                Some(RuntimeUpdate::BackendFailure { consecutive_failures, .. })
                    if consecutive_failures == count
            ));
        }
        assert!(state.update(Ok(changed_route.clone()), false).is_some());
        assert_eq!(state.consecutive_failures, 0);
        assert!(state.update(Ok(changed_route), false).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn changes_during_a_query_are_coalesced_into_one_followup() {
        let (catalog, mut queries) = catalog(
            ExecutionRoute::LocalSystemdTools,
            RuntimeObservers::default(),
        );
        let (tx, mut updates) = mpsc::channel(1);
        let observation = catalog.observe(tx);
        let first = queries.recv().await.unwrap();
        for _ in 0..1000 {
            catalog.invalidate();
        }
        first.send(Ok(empty_snapshot())).unwrap();
        assert!(matches!(
            updates.recv().await,
            Some(RuntimeUpdate::Snapshot(_))
        ));

        let next = tokio::time::timeout(Duration::from_secs(1), queries.recv())
            .await
            .expect("an in-flight change must not wait for the polling interval")
            .unwrap();
        next.send(Ok(empty_snapshot())).unwrap();
        assert!(matches!(
            updates.recv().await,
            Some(RuntimeUpdate::Snapshot(_))
        ));
        tokio::task::yield_now().await;
        assert!(queries.try_recv().is_err());
        observation.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_signal_source_keeps_working_queries_and_uses_periodic_refresh() {
        let (started_tx, mut started_rx) = mpsc::channel(1);
        let (catalog, mut queries) = catalog(
            ExecutionRoute::DirectDbus,
            RuntimeObservers {
                backend: Some(Arc::new(ControlledChanges(started_tx))),
                filesystem: None,
            },
        );
        let (tx, mut updates) = mpsc::channel(1);
        let observation = catalog.observe(tx);
        let (_, finish) = started_rx.recv().await.unwrap();
        queries
            .recv()
            .await
            .unwrap()
            .send(Ok(empty_snapshot()))
            .unwrap();
        updates.recv().await.unwrap();

        finish
            .send(Err(RuntimeError::unavailable("signal stream ended")))
            .unwrap();
        queries
            .recv()
            .await
            .unwrap()
            .send(Ok(empty_snapshot()))
            .unwrap();
        let Some(RuntimeUpdate::Snapshot(query)) = updates.recv().await else {
            panic!("losing notifications must not change a working query route");
        };
        assert_eq!(query.route, ExecutionRoute::DirectDbus);

        tokio::time::advance(POLL_INTERVAL).await;
        let polling = tokio::time::timeout(Duration::from_millis(1), queries.recv())
            .await
            .expect("a stopped observer must use the shorter polling interval")
            .unwrap();
        polling.send(Ok(empty_snapshot())).unwrap();
        observation.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_releases_pending_queries_and_all_host_subscriptions() {
        let (started_tx, mut started_rx) = mpsc::channel(2);
        let source = Arc::new(ControlledChanges(started_tx));
        let weak_source = Arc::downgrade(&source);
        let (catalog, mut queries) = catalog(
            ExecutionRoute::LocalSystemdTools,
            RuntimeObservers {
                backend: Some(source.clone()),
                filesystem: Some(source),
            },
        );
        let weak_catalog = Arc::downgrade(&catalog);
        let (tx, mut updates) = mpsc::channel(1);
        let observation = catalog.observe(tx);
        let _backend = started_rx.recv().await.unwrap();
        let _filesystem = started_rx.recv().await.unwrap();
        let pending = queries.recv().await.unwrap();

        drop(catalog);
        observation.shutdown().await;

        assert!(pending.is_closed());
        assert!(weak_catalog.upgrade().is_none());
        assert!(weak_source.upgrade().is_none());
        assert!(updates.recv().await.is_none());
    }

    #[tokio::test]
    async fn dropping_the_consumer_cancels_an_inflight_query() {
        let (catalog, mut queries) = catalog(
            ExecutionRoute::LocalSystemdTools,
            RuntimeObservers::default(),
        );
        let (tx, updates) = mpsc::channel(1);
        let observation = catalog.observe(tx);
        let mut pending = queries.recv().await.unwrap();
        drop(updates);

        tokio::time::timeout(Duration::from_secs(1), pending.closed())
            .await
            .unwrap();
        observation.shutdown().await;
    }
}

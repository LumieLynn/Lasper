//! Runtime projection, desktop ACL, and lifecycle orchestration.

use std::sync::Arc;
use std::time::Duration;

use crate::application::sessions::{SessionError, ShellTarget, X11ProjectionContext};
use crate::domain::x11::HostX11Socket;

use super::access::{
    X11AccessError, X11Authorization, X11AuthorizationRequest, X11DesktopAccessPort, X11Revocation,
    X11RevokeRequest,
};
use super::catalog::X11EndpointDiscoveryService;
use super::grants::X11AccessCheck;
use super::lifecycle::{X11ReconcileReport, X11ReconcileRetry, X11ReconcileTarget};
use super::session::{
    select_session_endpoints, X11SessionPreparation, X11SessionPreparationPort, X11SessionSelection,
};

pub(super) const MACHINE_LIFECYCLE_RETRY_DELAY: Duration = Duration::from_millis(2_100);
const MACHINE_LIFECYCLE_RECONCILE_ATTEMPTS: usize = 3;
/// Runtime projection capability required by X11 access management.
///
/// The implementation may be direct or elevated, but the X11 application
/// service must not know which session transport provides the probe.
#[async_trait::async_trait]
pub(crate) trait X11ProjectionPort: Send + Sync {
    async fn probe(
        &self,
        target: ShellTarget,
        host_socket: HostX11Socket,
    ) -> Result<X11ProjectionContext, SessionError>;
}

/// Combines runtime namespace evidence with a caller-owned X server query.
/// The desktop query always stays in the invoking user's process, even when
/// the projection half is routed through the elevated daemon.
pub struct X11AccessService {
    projection: Arc<dyn X11ProjectionPort>,
    endpoints: Arc<X11EndpointDiscoveryService>,
    desktop: Arc<dyn X11DesktopAccessPort>,
}

#[async_trait::async_trait]
impl X11SessionPreparationPort for X11AccessService {
    async fn prepare_session(
        &self,
        target: ShellTarget,
        selection: X11SessionSelection,
    ) -> Result<X11SessionPreparation, X11AccessError> {
        X11AccessService::prepare_session(self, target, selection).await
    }
}

impl X11AccessService {
    pub(crate) fn new(
        projection: Arc<dyn X11ProjectionPort>,
        endpoints: Arc<X11EndpointDiscoveryService>,
        desktop: Arc<dyn X11DesktopAccessPort>,
    ) -> Self {
        Self {
            projection,
            endpoints,
            desktop,
        }
    }

    pub async fn check(
        &self,
        target: ShellTarget,
        socket: HostX11Socket,
    ) -> Result<X11AccessCheck, X11AccessError> {
        let (projection, observation) = tokio::join!(
            self.projection.probe(target.clone(), socket.clone()),
            self.desktop.snapshot(&socket),
        );
        let projection = projection.map_err(X11AccessError::Projection)?;
        let observation = observation.map_err(X11AccessError::Desktop)?;
        Ok(X11AccessCheck::from_desktop_observation(
            target,
            projection,
            observation,
        ))
    }

    pub async fn authorize(
        &self,
        target: ShellTarget,
        socket: HostX11Socket,
    ) -> Result<X11Authorization, X11AccessError> {
        let projection = self
            .projection
            .probe(target.clone(), socket)
            .await
            .map_err(X11AccessError::Projection)?;
        let request = X11AuthorizationRequest::new(target.clone(), projection.clone());
        let desktop = self
            .desktop
            .ensure(&request)
            .await
            .map_err(X11AccessError::Desktop)?;
        let mut check =
            X11AccessCheck::from_desktop_observation(target, projection, desktop.observation);
        if let Err(error) = self.desktop.synchronize_reconcile_activation().await {
            check.push_diagnostic(format!(
                "external-stop X11 reconcile is unavailable: {error}"
            ));
        }
        Ok(X11Authorization {
            check,
            disposition: desktop.disposition,
        })
    }

    /// Prepare one explicitly requested Host X11 session. Endpoint discovery
    /// and projection probing are read-only; the desktop ACL is considered
    /// only after one startup-configured projection has been proven usable.
    pub async fn prepare_session(
        &self,
        target: ShellTarget,
        selection: X11SessionSelection,
    ) -> Result<X11SessionPreparation, X11AccessError> {
        let projection = self
            .resolve_session_projection(target.clone(), selection)
            .await?;

        self.prepare_resolved_session(target, projection).await
    }

    async fn prepare_resolved_session(
        &self,
        target: ShellTarget,
        projection: X11ProjectionContext,
    ) -> Result<X11SessionPreparation, X11AccessError> {
        let request =
            X11AuthorizationRequest::for_explicit_session(target.clone(), projection.clone());
        let desktop = self
            .desktop
            .ensure(&request)
            .await
            .map_err(X11AccessError::Desktop)?;
        let mut check = X11AccessCheck::from_desktop_observation(
            target.clone(),
            projection.clone(),
            desktop.observation,
        );
        if let Err(error) = self.desktop.synchronize_reconcile_activation().await {
            check.push_diagnostic(format!(
                "external-stop X11 reconcile is unavailable: {error}"
            ));
        }
        Ok(X11SessionPreparation::new(
            target,
            check,
            desktop.disposition,
        ))
    }

    async fn resolve_session_projection(
        &self,
        target: ShellTarget,
        selection: X11SessionSelection,
    ) -> Result<X11ProjectionContext, X11AccessError> {
        let catalog = self.endpoints.discover(&[]).await;
        let (display, candidates) =
            select_session_endpoints(catalog, selection).map_err(X11AccessError::Selection)?;

        let mut failures = Vec::new();
        let mut selected = None;
        for socket in candidates {
            match self.projection.probe(target.clone(), socket.clone()).await {
                Ok(projection) => {
                    selected = Some(projection);
                    break;
                }
                Err(error) => failures.push(format!("{}: {error}", socket.source().display())),
            }
        }
        selected.ok_or_else(|| {
            X11AccessError::Projection(SessionError::with_hint(
                format!(
                    "no usable startup-configured projection was found for X11 display :{display}: {}",
                    failures.join("; ")
                ),
                "Configure the selected X11 socket while the machine is stopped, then start or restart it.",
            ))
        })
    }

    pub async fn revoke(
        &self,
        target: ShellTarget,
        socket: HostX11Socket,
        record_id: String,
    ) -> Result<X11Revocation, X11AccessError> {
        let projection = self
            .projection
            .probe(target.clone(), socket)
            .await
            .map_err(X11AccessError::Projection)?;
        let request = X11RevokeRequest::new(target.clone(), projection.clone(), record_id);
        let desktop = self
            .desktop
            .revoke(&request)
            .await
            .map_err(X11AccessError::Desktop)?;
        if let Err(error) = self.desktop.synchronize_reconcile_activation().await {
            log::debug!("X11 lifecycle activation sync unavailable after revoke: {error}");
        }
        Ok(X11Revocation {
            check: X11AccessCheck::from_desktop_observation(
                target,
                projection,
                desktop.observation,
            ),
            disposition: desktop.disposition,
        })
    }

    /// Run one bounded, system-scope machine claim reconcile pass. The
    /// Targets come from persisted Lasper records; this never broad-scans all
    /// desktop X11 displays. Endpoint-local failures remain diagnostics so
    /// one stale record cannot prevent cleanup of another record.
    pub(crate) async fn reconcile(&self) -> Result<Vec<X11ReconcileReport>, X11AccessError> {
        let targets = self.reconcile_targets().await?;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.reconcile_pass(&targets).await)
    }

    async fn reconcile_targets(&self) -> Result<Vec<X11ReconcileTarget>, X11AccessError> {
        self.desktop
            .reconcile_targets()
            .await
            .map_err(X11AccessError::Desktop)
    }

    async fn reconcile_pass(&self, targets: &[X11ReconcileTarget]) -> Vec<X11ReconcileReport> {
        let mut reports = Vec::with_capacity(targets.len());
        for target in targets {
            let report = match self.desktop.reconcile(target).await {
                Ok(report) => report,
                Err(error) => X11ReconcileReport::new(
                    target.display,
                    Vec::new(),
                    Vec::new(),
                    vec![format!("{}: {error}", target.source.display())],
                )
                .with_retry(X11ReconcileRetry::PendingObservation),
            };
            if !report.revoked_record_ids().is_empty()
                || !report.pending_record_ids().is_empty()
                || !report.diagnostics().is_empty()
            {
                log::debug!(
                    "X11 lifecycle reconcile for :{}: revoked={}, pending={}, diagnostics={}",
                    report.display(),
                    report.revoked_record_ids().len(),
                    report.pending_record_ids().len(),
                    report.diagnostics().len(),
                );
                for diagnostic in report.diagnostics() {
                    log::debug!(
                        "X11 lifecycle reconcile :{}: {diagnostic}",
                        report.display()
                    );
                }
            }
            reports.push(report);
        }
        reports
    }

    /// Reconcile after a system machine lifecycle event. A stop command may
    /// return before machined removes its registration, while the first pass
    /// after removal only records a cleanup-pending timestamp. Three bounded
    /// passes allow both transitions: still-present -> pending -> grace elapsed.
    /// Neither the in-process stop path nor the user-manager path worker relies
    /// on a second filesystem event. A complete endpoint report is not enough
    /// to stop retries: it may describe a still-registered machine. Local claim
    /// state decides whether there is anything left to observe.
    pub(crate) async fn reconcile_after_machine_event(
        &self,
    ) -> Result<Vec<X11ReconcileReport>, X11AccessError> {
        let targets = self.reconcile_targets().await?;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut reports = self.reconcile_pass(&targets).await;
        for _ in 1..MACHINE_LIFECYCLE_RECONCILE_ATTEMPTS {
            if reports
                .iter()
                .all(|report| report.retry() == X11ReconcileRetry::None)
            {
                break;
            }
            tokio::time::sleep(MACHINE_LIFECYCLE_RETRY_DELAY).await;
            for (report, next) in reports.iter_mut().zip(self.reconcile_pass(&targets).await) {
                report.update_from_pass(next);
            }
        }
        Ok(reports)
    }

    pub(crate) async fn synchronize_reconcile_activation(&self) -> Result<(), X11AccessError> {
        self.desktop
            .synchronize_reconcile_activation()
            .await
            .map_err(X11AccessError::Desktop)
    }
}

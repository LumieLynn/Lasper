//! Runtime projection, desktop ACL, and lifecycle orchestration.

use std::sync::Arc;
use std::time::Duration;

use crate::application::sessions::{
    SessionError, ShellTarget, X11ProjectionContext, X11SessionContext,
};
use crate::domain::x11::HostX11Socket;

use super::access::{
    X11AccessError, X11Authorization, X11AuthorizationRequest, X11DesktopAccessPort,
    X11ReconcileReport, X11Revocation, X11RevokeRequest,
};
use super::catalog::X11EndpointDiscoveryService;
use super::grants::X11AccessCheck;
use super::session::{select_session_endpoints, X11SessionPreparation, X11SessionSelection};

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
        Ok(X11SessionPreparation {
            context: X11SessionContext::prepared(target, projection),
            check,
            disposition: desktop.disposition,
        })
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
    /// endpoint catalog is discovered in the invoking desktop process; the
    /// adapter only receives verified live X11 endpoints and never receives a
    /// target user-scope machine selector. Endpoint-local failures are
    /// returned as diagnostics so one broken display cannot prevent cleanup on
    /// another display.
    pub(crate) async fn reconcile(&self) -> Result<Vec<X11ReconcileReport>, X11AccessError> {
        let catalog = self.endpoints.discover(&[]).await;
        if catalog.sockets.is_empty() {
            return Err(X11AccessError::Selection(
                catalog
                    .diagnostics
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "no live local X11 endpoint was discovered".into()),
            ));
        }
        let mut reports = Vec::with_capacity(catalog.sockets.len());
        for socket in catalog.sockets {
            let report = match self.desktop.reconcile(&socket).await {
                Ok(report) => report,
                Err(error) => X11ReconcileReport::new(
                    socket.display(),
                    Vec::new(),
                    Vec::new(),
                    vec![error.to_string()],
                ),
            };
            reports.push(report);
        }
        Ok(reports)
    }

    /// Reconcile after a system machine lifecycle event. A stop command may
    /// return before machined removes its registration, while the first pass
    /// after removal only records a cleanup-pending timestamp. Three bounded
    /// passes cover both races: still-present -> pending -> grace elapsed.
    /// Neither the in-process stop path nor the user-manager path worker relies
    /// on a second filesystem event.
    pub(crate) async fn reconcile_after_machine_event(
        &self,
    ) -> Result<Vec<X11ReconcileReport>, X11AccessError> {
        let mut reports = self.reconcile().await?;
        for _ in 1..MACHINE_LIFECYCLE_RECONCILE_ATTEMPTS {
            tokio::time::sleep(MACHINE_LIFECYCLE_RETRY_DELAY).await;
            reports = self.reconcile().await?;
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

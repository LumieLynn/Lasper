//! Host-side X11 integration for provisioning, projection, and ACL lifecycle.
//!
//! The adapter is deliberately split by responsibility: discovery observes
//! endpoint evidence, transport speaks X11 and revalidates sockets, state owns
//! the user-runtime records, access coordinates ACL operations, lifecycle
//! reconciles system-scope machine claims, and activation installs the user
//! manager wake-up units. The application layer sees only the ports exposed
//! below.

mod access;
mod activation;
mod common;
mod discovery;
mod lifecycle;
mod state;
mod transport;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::application::x11::{
    X11AuthorizationRequest, X11DesktopAccessError, X11DesktopAccessPort, X11DesktopAuthorization,
    X11DesktopObservation, X11DesktopRevocation, X11EndpointCatalog, X11EndpointDiscoveryPort,
    X11ReconcileReport, X11RevokeRequest,
};
use crate::domain::x11::HostX11Socket;

pub(crate) struct HostX11EndpointDiscovery;

/// Discover host X11 endpoints for provisioning UI. Discovery is observation
/// only: it authenticates the invoking desktop connection but never changes
/// ACLs.
pub(crate) async fn discover_host_x11_sockets() -> X11EndpointCatalog {
    match tokio::task::spawn_blocking(|| discovery::discover_sync(&[])).await {
        Ok(catalog) => catalog,
        Err(error) => {
            let diagnostic = format!("X11 endpoint discovery stopped unexpectedly: {error}");
            log::warn!("{diagnostic}");
            X11EndpointCatalog {
                diagnostics: vec![diagnostic],
                ..Default::default()
            }
        }
    }
}

pub(crate) struct HostX11DesktopAccess {
    activation_backend: activation::ActivationBackend,
}

impl HostX11DesktopAccess {
    pub(crate) fn new(systemd_tools: bool) -> Self {
        Self {
            activation_backend: if systemd_tools {
                activation::ActivationBackend::SystemdTools
            } else {
                activation::ActivationBackend::Dbus
            },
        }
    }
}

#[async_trait::async_trait]
impl X11EndpointDiscoveryPort for HostX11EndpointDiscovery {
    async fn discover(&self, configured_sources: &[PathBuf]) -> X11EndpointCatalog {
        let configured_sources = configured_sources.to_vec();
        match tokio::task::spawn_blocking(move || discovery::discover_sync(&configured_sources))
            .await
        {
            Ok(catalog) => catalog,
            Err(error) => X11EndpointCatalog {
                diagnostics: vec![format!(
                    "X11 endpoint discovery stopped unexpectedly: {error}"
                )],
                ..Default::default()
            },
        }
    }
}

#[async_trait::async_trait]
impl X11DesktopAccessPort for HostX11DesktopAccess {
    async fn snapshot(
        &self,
        socket: &HostX11Socket,
    ) -> Result<X11DesktopObservation, X11DesktopAccessError> {
        let socket = socket.clone();
        tokio::task::spawn_blocking(move || access::snapshot_desktop_sync(&socket))
            .await
            .map_err(|error| {
                X11DesktopAccessError::new(format!("X11 ACL query task failed: {error}"))
            })?
            .map_err(X11DesktopAccessError::new)
    }

    async fn ensure(
        &self,
        request: &X11AuthorizationRequest,
    ) -> Result<X11DesktopAuthorization, X11DesktopAccessError> {
        let request = request.clone();
        tokio::task::spawn_blocking(move || access::ensure_access_sync(&request))
            .await
            .map_err(|error| {
                X11DesktopAccessError::new(format!("X11 authorization task failed: {error}"))
            })?
            .map_err(X11DesktopAccessError::new)
    }

    async fn revoke(
        &self,
        request: &X11RevokeRequest,
    ) -> Result<X11DesktopRevocation, X11DesktopAccessError> {
        let request = request.clone();
        tokio::task::spawn_blocking(move || access::revoke_access_sync(&request))
            .await
            .map_err(|error| {
                X11DesktopAccessError::new(format!("X11 revocation task failed: {error}"))
            })?
            .map_err(X11DesktopAccessError::new)
    }

    async fn reconcile(
        &self,
        socket: &HostX11Socket,
    ) -> Result<X11ReconcileReport, X11DesktopAccessError> {
        let socket = socket.clone();
        tokio::task::spawn_blocking(move || lifecycle::reconcile_sync(&socket))
            .await
            .map_err(|error| {
                X11DesktopAccessError::new(format!("X11 reconcile task failed: {error}"))
            })?
            .map_err(X11DesktopAccessError::new)
    }

    async fn synchronize_reconcile_activation(&self) -> Result<(), X11DesktopAccessError> {
        let has_active_claims = tokio::task::spawn_blocking(state::has_active_claims_sync)
            .await
            .map_err(|error| {
                X11DesktopAccessError::new(format!(
                    "X11 claim activation inspection task failed: {error}"
                ))
            })?
            .map_err(X11DesktopAccessError::new)?;
        activation::synchronize_system_machine_path_activation(
            self.activation_backend,
            has_active_claims,
        )
        .await
        .map_err(X11DesktopAccessError::new)
    }
}

/// Re-run authenticated X11 setup from the invoking desktop process and
/// require endpoint evidence to remain current.
pub(crate) async fn revalidate_for_desktop(socket: &HostX11Socket) -> Result<(), String> {
    let socket = socket.clone();
    tokio::task::spawn_blocking(move || {
        let current = discovery::inspect_endpoint(
            socket.display(),
            socket.alternate(),
            socket.source().to_path_buf(),
        )?;
        if current != socket {
            return Err(format!(
                "{} changed after X11 endpoint discovery",
                socket.source().display()
            ));
        }
        Ok(())
    })
    .await
    .map_err(|error| format!("X11 endpoint revalidation task failed: {error}"))?
}

/// Revalidate filesystem and peer-process evidence without relying on the
/// caller's Xauthority. The privileged side uses this after an authenticated
/// client has completed `revalidate_for_desktop`.
pub(crate) async fn projection_socket_identities(
    socket: &HostX11Socket,
) -> Result<Vec<(u64, u64)>, String> {
    let socket = socket.clone();
    tokio::task::spawn_blocking(move || {
        let selected = transport::inspect_endpoint_peer_only(
            socket.display(),
            socket.alternate(),
            socket.source().to_path_buf(),
        )?;
        if selected != socket {
            return Err(format!(
                "{} changed after X11 endpoint discovery",
                socket.source().display()
            ));
        }
        let mut identities = vec![(socket.revision().device, socket.revision().inode)];
        if socket.alternate() {
            let standard = transport::inspect_endpoint_peer_only(
                socket.display(),
                false,
                Path::new(common::X11_SOCKET_DIRECTORY).join(format!("X{}", socket.display())),
            )?;
            if standard.peer_identity() != socket.peer_identity() {
                return Err(format!(
                    "standard and alternate endpoints for :{} no longer belong to the same X server",
                    socket.display()
                ));
            }
            let identity = (standard.revision().device, standard.revision().inode);
            if !identities.contains(&identity) {
                identities.push(identity);
            }
        }
        Ok(identities)
    })
    .await
    .map_err(|error| format!("X11 projection evidence task failed: {error}"))?
}

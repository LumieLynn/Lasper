//! Read-only validation of startup-configured X11 socket projections.
//!
//! Host endpoint discovery and X server ACL control belong to the invoking
//! desktop user. This resolver runs only the bounded guest observation and
//! host-side machine-instance mapping needed by either runtime route.

use super::instance::MachineInstanceSnapshot;
use super::x11_probe::{
    X11ProjectionProbeObservation, X11ProjectionProbeRequest as MachineX11ProbeRequest,
    X11SocketAccess,
};
use super::{MachineSessionRequest, MachineSessionTransport};
use crate::adapters::config::NspawnConfigStore;
use crate::application::sessions::{
    SessionError, TerminalSessionHandle, X11FilesystemAccess, X11ProjectionContext,
    X11ProjectionProbeRequest,
};
use crate::domain::session::SessionSize;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) struct X11SessionResolver {
    machine: MachineSessionTransport,
    nspawn: NspawnConfigStore,
}

impl X11SessionResolver {
    pub(crate) fn new(machine: MachineSessionTransport, nspawn: NspawnConfigStore) -> Self {
        Self { machine, nspawn }
    }

    pub(crate) async fn probe(
        &self,
        request: X11ProjectionProbeRequest,
    ) -> Result<X11ProjectionContext, SessionError> {
        let expected_client_identities = projection_socket_identities(&request.host_socket).await?;
        let config = self
            .nspawn
            .inspect(request.target.machine().as_str())
            .await
            .map_err(|error| SessionError::new(format!("inspect startup X11 projection: {error}")))?
            .ok_or_else(projection_not_configured)?;
        let targets = config
            .x11_targets(&request.host_socket)
            .await
            .map_err(|error| {
                SessionError::new(format!("inspect startup X11 projection: {error}"))
            })?;
        if targets.is_empty() {
            return Err(projection_not_configured());
        }

        let client_path =
            PathBuf::from(format!("/tmp/.X11-unix/X{}", request.host_socket.display()));
        let instance =
            MachineInstanceSnapshot::observe(&self.machine, request.target.machine()).await?;
        let mut failure = projection_not_configured();
        for mount_target in targets {
            let probe = MachineX11ProbeRequest::new(
                request.target.machine().clone(),
                request.target.user().clone(),
                &mount_target,
                &client_path,
            )
            .map_err(|error| SessionError::new(format!("validate X11 probe paths: {error}")))?;
            let observation = run_probe(&self.machine, probe, request.probe_id).await?;
            match validate_projection(
                &observation,
                request.host_socket.revision(),
                &expected_client_identities,
                &mount_target,
                &client_path,
            ) {
                Ok(filesystem_access) => {
                    let identity = instance
                        .mapped_identity(observation.identity, observation.user_namespace)?;
                    let current_instance =
                        MachineInstanceSnapshot::observe(&self.machine, request.target.machine())
                            .await?;
                    if current_instance != instance {
                        return Err(SessionError::new(
                            "machine instance changed during X11 projection validation",
                        ));
                    }
                    let current_socket_identities =
                        projection_socket_identities(&request.host_socket).await?;
                    if current_socket_identities != expected_client_identities {
                        return Err(SessionError::new(
                            "host X11 endpoint changed during projection validation",
                        ));
                    }
                    return Ok(X11ProjectionContext::verified(
                        request.host_socket,
                        mount_target,
                        client_path,
                        filesystem_access,
                        identity,
                    ));
                }
                Err(error) => failure = error,
            }
        }
        Err(failure)
    }
}

async fn projection_socket_identities(
    socket: &crate::domain::x11::HostX11Socket,
) -> Result<Vec<(u64, u64)>, SessionError> {
    crate::adapters::platform::x11::projection_socket_identities(socket)
        .await
        .map_err(|error| SessionError::new(format!("revalidate host X11 endpoint: {error}")))
}

fn validate_projection(
    observation: &X11ProjectionProbeObservation,
    source_revision: crate::domain::x11::X11SocketRevision,
    expected_client_identities: &[(u64, u64)],
    mount_target: &Path,
    client_path: &Path,
) -> Result<X11FilesystemAccess, SessionError> {
    let mount_writable = validate_socket_access(
        observation.mount.access,
        observation.mount.identity,
        &[(source_revision.device, source_revision.inode)],
        "X11 mount target",
        mount_target,
    )?;
    let client_writable = validate_socket_access(
        observation.client.access,
        observation.client.identity,
        expected_client_identities,
        "X11 client path",
        client_path,
    )?;
    Ok(X11FilesystemAccess::observed(
        mount_writable,
        client_writable,
    ))
}

fn validate_socket_access(
    access: X11SocketAccess,
    identity: Option<(u64, u64)>,
    expected_identities: &[(u64, u64)],
    label: &str,
    path: &Path,
) -> Result<bool, SessionError> {
    let detail = match access {
        X11SocketAccess::Accessible | X11SocketAccess::Denied => {
            if identity.is_some_and(|identity| expected_identities.contains(&identity)) {
                return Ok(access == X11SocketAccess::Accessible);
            }
            "is not the selected host X11 server socket (the bind may be stale)"
        }
        X11SocketAccess::Missing => "is missing",
        X11SocketAccess::NotSocket => "is not a socket",
    };
    Err(SessionError::with_hint(
        format!("{label} {} {detail}", path.display()),
        "Check the configured bind and guest client path, then restart the machine if its startup configuration changed.",
    ))
}

async fn run_probe(
    machine: &MachineSessionTransport,
    request: MachineX11ProbeRequest,
    id: crate::domain::session::SessionId,
) -> Result<X11ProjectionProbeObservation, SessionError> {
    let size = SessionSize::new(80, 24).expect("fixed probe PTY size is valid");
    let mut handle: TerminalSessionHandle = machine
        .open_local(
            MachineSessionRequest::x11_projection_probe(request),
            id,
            size,
        )
        .await?;
    super::x11_probe::collect_x11_probe(&mut handle)
        .await
        .map_err(|error| SessionError::new(format!("X11 projection probe failed: {error}")))
}

fn projection_not_configured() -> SessionError {
    SessionError::with_hint(
        "the selected X11 endpoint is not declared as a bind in the machine's startup configuration",
        "Select the display in Configure while the machine is stopped, then start or restart it before checking X11 access.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::session::x11_probe::X11SocketObservation;

    fn observation(
        mount: X11SocketObservation,
        client: X11SocketObservation,
    ) -> X11ProjectionProbeObservation {
        X11ProjectionProbeObservation {
            identity: crate::application::sessions::ObservedGuestIdentity::new(1000, 1000),
            user_namespace: crate::application::sessions::ObservedNamespaceIdentity::new(1, 2),
            mount,
            client,
        }
    }

    #[test]
    fn projection_requires_both_mount_and_client_paths_to_match_the_server() {
        let accessible = |identity| X11SocketObservation {
            access: X11SocketAccess::Accessible,
            identity: Some(identity),
        };
        let revision = crate::domain::x11::X11SocketRevision {
            device: 10,
            inode: 20,
            ctime_seconds: 0,
            ctime_nanoseconds: 0,
        };
        assert!(validate_projection(
            &observation(accessible((10, 20)), accessible((10, 21))),
            revision,
            &[(10, 20), (10, 21)],
            Path::new("/mnt/X0_"),
            Path::new("/tmp/.X11-unix/X0"),
        )
        .is_ok());
        assert!(validate_projection(
            &observation(accessible((10, 22)), accessible((10, 21))),
            revision,
            &[(10, 20), (10, 21)],
            Path::new("/mnt/X0_"),
            Path::new("/tmp/.X11-unix/X0"),
        )
        .is_err());
        assert!(validate_projection(
            &observation(accessible((10, 20)), accessible((10, 22))),
            revision,
            &[(10, 20), (10, 21)],
            Path::new("/mnt/X0_"),
            Path::new("/tmp/.X11-unix/X0"),
        )
        .is_err());
    }

    #[test]
    fn projection_keeps_dac_failure_distinct_from_stale_identity() {
        let denied = X11SocketObservation {
            access: X11SocketAccess::Denied,
            identity: Some((10, 20)),
        };
        let missing = X11SocketObservation {
            access: X11SocketAccess::Missing,
            identity: None,
        };
        assert!(!validate_socket_access(
            denied.access,
            Some((10, 20)),
            &[(10, 20)],
            "target",
            Path::new("/socket")
        )
        .unwrap());
        assert!(validate_socket_access(
            denied.access,
            Some((10, 21)),
            &[(10, 20)],
            "target",
            Path::new("/socket")
        )
        .is_err());
        assert!(validate_socket_access(
            missing.access,
            None,
            &[(10, 20)],
            "target",
            Path::new("/socket")
        )
        .is_err());
    }
}

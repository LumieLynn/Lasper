use super::DeploymentError;
use crate::domain::provisioning::PrivateUsersMode;
use crate::domain::wayland::{
    ContainerUserIdentity, WaylandBindPolicy, WaylandGrant, WaylandGrantIntent,
};

pub(crate) fn validate_wayland_intent(
    intent: &WaylandGrantIntent,
    private_users: Option<PrivateUsersMode>,
) -> Result<(), DeploymentError> {
    for source in intent.sources() {
        if source.owner_uid() != source.session_uid() {
            return Err(DeploymentError::rejected(
                "Wayland socket is not owned by the selected host session user",
            ));
        }
    }
    resolve_wayland_bind_policy(private_users)?;
    Ok(())
}

pub(crate) fn resolve_wayland_grant(
    intent: WaylandGrantIntent,
    target: ContainerUserIdentity,
    private_users: Option<PrivateUsersMode>,
) -> Result<WaylandGrant, DeploymentError> {
    validate_wayland_intent(&intent, private_users)?;
    if intent.target_username() != target.username {
        return Err(DeploymentError::rejected(
            "resolved Wayland user does not match the requested target",
        ));
    }

    let mut sockets = Vec::with_capacity(intent.sources().len());
    for source in intent.sources() {
        let mode = source.mode();
        let Some(socket_access) = source.write_access_for(&target) else {
            return Err(DeploymentError::rejected(format!(
                "container user {} (uid {}, gid {}) cannot write host Wayland socket {} owned by {}:{} with mode {:04o}",
                target.username,
                target.uid,
                target.gid,
                source.display().as_str(),
                source.owner_uid(),
                source.owner_gid(),
                mode & 0o7777,
            )));
        };
        sockets.push(WaylandGrant::socket(source.clone(), socket_access));
    }

    WaylandGrant::resolved(
        target,
        sockets,
        intent.default_display().clone(),
        resolve_wayland_bind_policy(private_users)?,
    )
    .map_err(DeploymentError::rejected)
}

pub(crate) fn resolve_wayland_bind_policy(
    private_users: Option<PrivateUsersMode>,
) -> Result<WaylandBindPolicy, DeploymentError> {
    match private_users {
        Some(PrivateUsersMode::No) => Ok(WaylandBindPolicy::NoIdmap),
        None | Some(PrivateUsersMode::Yes | PrivateUsersMode::Pick) => {
            Ok(WaylandBindPolicy::Idmap)
        }
        Some(PrivateUsersMode::Managed) => Err(DeploymentError::rejected(
            "Wayland grants are not supported with PrivateUsers=managed because systemd-nspawn does not map ordinary bind mounts into the managed namespace",
        )),
        Some(PrivateUsersMode::Identity) => Err(DeploymentError::rejected(
            "Wayland grants are not supported with PrivateUsers=identity",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::wayland::{
        HostWaylandSocket, SocketRevision, WaylandDisplay, WaylandSocketAccess,
    };
    use std::path::PathBuf;

    fn socket(mode: u32, owner_uid: u32, owner_gid: u32) -> HostWaylandSocket {
        HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-1").unwrap(),
            PathBuf::from("/run/user/1001"),
            PathBuf::from("/run/user/1001/wayland-1"),
            owner_uid,
            owner_uid,
            owner_gid,
            mode,
            SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap()
    }

    fn intent(socket: HostWaylandSocket) -> WaylandGrantIntent {
        WaylandGrantIntent::new("lumie", vec![socket.clone()], socket.display().clone()).unwrap()
    }

    #[test]
    fn matching_numeric_owner_is_accepted_for_supported_modes() {
        for mode in [
            None,
            Some(PrivateUsersMode::No),
            Some(PrivateUsersMode::Yes),
            Some(PrivateUsersMode::Pick),
        ] {
            let grant = resolve_wayland_grant(
                intent(socket(0o755, 1001, 1001)),
                ContainerUserIdentity {
                    username: "lumie".into(),
                    uid: 1001,
                    gid: 1001,
                },
                mode,
            )
            .unwrap();
            assert_eq!(
                grant.sockets()[0].socket_access(),
                WaylandSocketAccess::Owner
            );
            assert_eq!(
                grant.bind_policy(),
                if mode == Some(PrivateUsersMode::No) {
                    WaylandBindPolicy::NoIdmap
                } else {
                    WaylandBindPolicy::Idmap
                }
            );
        }
    }

    #[test]
    fn mismatching_numeric_identity_without_dac_write_is_rejected_for_each_policy() {
        for mode in [
            Some(PrivateUsersMode::No),
            Some(PrivateUsersMode::Yes),
            Some(PrivateUsersMode::Pick),
        ] {
            let error = resolve_wayland_grant(
                intent(socket(0o755, 1001, 1001)),
                ContainerUserIdentity {
                    username: "lumie".into(),
                    uid: 1000,
                    gid: 1000,
                },
                mode,
            )
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("cannot write host Wayland socket"));
        }
    }

    #[test]
    fn group_and_other_write_access_are_modelled_explicitly() {
        let group = resolve_wayland_grant(
            intent(socket(0o720, 1001, 2000)),
            ContainerUserIdentity {
                username: "lumie".into(),
                uid: 1000,
                gid: 2000,
            },
            Some(PrivateUsersMode::No),
        )
        .unwrap();
        assert_eq!(
            group.sockets()[0].socket_access(),
            WaylandSocketAccess::Group
        );

        let other = resolve_wayland_grant(
            intent(socket(0o702, 1001, 2000)),
            ContainerUserIdentity {
                username: "lumie".into(),
                uid: 1000,
                gid: 3000,
            },
            Some(PrivateUsersMode::No),
        )
        .unwrap();
        assert_eq!(
            other.sockets()[0].socket_access(),
            WaylandSocketAccess::Other
        );
    }

    #[test]
    fn managed_and_identity_modes_are_explicitly_unsupported() {
        for mode in [PrivateUsersMode::Managed, PrivateUsersMode::Identity] {
            let error = validate_wayland_intent(&intent(socket(0o700, 1001, 1001)), Some(mode))
                .unwrap_err();
            assert!(error.to_string().contains("not supported"));
        }
    }
}

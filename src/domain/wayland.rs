use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};

/// A Wayland display basename such as `wayland-0`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct WaylandDisplay(String);

impl WaylandDisplay {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if !value.starts_with("wayland-")
            || value.chars().any(char::is_control)
            || Path::new(&value).file_name().and_then(|name| name.to_str()) != Some(value.as_str())
        {
            return Err(format!("invalid Wayland display name: {value:?}"));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WaylandDisplay {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Filesystem identity used to detect a socket being replaced after discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SocketRevision {
    pub device: u64,
    pub inode: u64,
    pub ctime_seconds: i64,
    pub ctime_nanoseconds: i64,
}

/// Evidence captured from one host session's Wayland socket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostWaylandSocket {
    display: WaylandDisplay,
    runtime_dir: PathBuf,
    canonical_path: PathBuf,
    session_uid: u32,
    owner_uid: u32,
    owner_gid: u32,
    mode: u32,
    revision: SocketRevision,
}

impl<'de> Deserialize<'de> for HostWaylandSocket {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Evidence {
            display: WaylandDisplay,
            runtime_dir: PathBuf,
            canonical_path: PathBuf,
            session_uid: u32,
            owner_uid: u32,
            owner_gid: u32,
            mode: u32,
            revision: SocketRevision,
        }

        let evidence = Evidence::deserialize(deserializer)?;
        Self::from_verified_parts(
            evidence.display,
            evidence.runtime_dir,
            evidence.canonical_path,
            evidence.session_uid,
            evidence.owner_uid,
            evidence.owner_gid,
            evidence.mode,
            evidence.revision,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl HostWaylandSocket {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_verified_parts(
        display: WaylandDisplay,
        runtime_dir: PathBuf,
        canonical_path: PathBuf,
        session_uid: u32,
        owner_uid: u32,
        owner_gid: u32,
        mode: u32,
        revision: SocketRevision,
    ) -> Result<Self, String> {
        if !runtime_dir.is_absolute() || !canonical_path.is_absolute() {
            return Err("Wayland evidence paths must be absolute".into());
        }
        if !canonical_path.starts_with(&runtime_dir) {
            return Err("Wayland socket must remain inside its runtime directory".into());
        }
        if owner_uid != session_uid {
            return Err("Wayland socket must be owned by its host session user".into());
        }
        Ok(Self {
            display,
            runtime_dir,
            canonical_path,
            session_uid,
            owner_uid,
            owner_gid,
            mode: mode & 0o7777,
            revision,
        })
    }

    pub fn display(&self) -> &WaylandDisplay {
        &self.display
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn session_uid(&self) -> u32 {
        self.session_uid
    }

    pub fn owner_uid(&self) -> u32 {
        self.owner_uid
    }

    pub fn owner_gid(&self) -> u32 {
        self.owner_gid
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn revision(&self) -> SocketRevision {
        self.revision
    }

    pub fn write_access_for(&self, target: &ContainerUserIdentity) -> Option<WaylandSocketAccess> {
        if target.uid == self.owner_uid {
            (self.mode & 0o200 != 0).then_some(WaylandSocketAccess::Owner)
        } else if target.gid == self.owner_gid {
            (self.mode & 0o020 != 0).then_some(WaylandSocketAccess::Group)
        } else {
            (self.mode & 0o002 != 0).then_some(WaylandSocketAccess::Other)
        }
    }
}

/// Wizard/application intent: expose one or more verified host displays to one user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaylandGrantIntent {
    target_username: String,
    sources: Vec<HostWaylandSocket>,
    default_display: WaylandDisplay,
}

impl<'de> Deserialize<'de> for WaylandGrantIntent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Intent {
            target_username: String,
            sources: Vec<HostWaylandSocket>,
            default_display: WaylandDisplay,
        }

        let intent = Intent::deserialize(deserializer)?;
        Self::new(
            intent.target_username,
            intent.sources,
            intent.default_display,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl WaylandGrantIntent {
    pub fn new(
        target_username: impl Into<String>,
        sources: Vec<HostWaylandSocket>,
        default_display: WaylandDisplay,
    ) -> Result<Self, String> {
        let target_username = target_username.into();
        if target_username.is_empty() {
            return Err("Wayland target username cannot be empty".into());
        }
        if sources.is_empty() {
            return Err("Wayland access requires at least one display socket".into());
        }

        let owner_uid = sources[0].owner_uid();
        let mut displays = std::collections::HashSet::new();
        let mut paths = std::collections::HashSet::new();
        for source in &sources {
            if source.owner_uid() != owner_uid {
                return Err(
                    "Wayland sockets assigned to one user must have the same owner UID".into(),
                );
            }
            if !displays.insert(source.display().clone()) {
                return Err("Wayland access contains a duplicate display".into());
            }
            if !paths.insert(source.canonical_path().to_path_buf()) {
                return Err("Wayland access contains a duplicate socket path".into());
            }
        }
        if !displays.contains(&default_display) {
            return Err("Default Wayland display must be one of the selected sockets".into());
        }

        Ok(Self {
            target_username,
            sources,
            default_display,
        })
    }

    pub fn target_username(&self) -> &str {
        &self.target_username
    }

    pub fn sources(&self) -> &[HostWaylandSocket] {
        &self.sources
    }

    pub fn default_display(&self) -> &WaylandDisplay {
        &self.default_display
    }

    pub fn required_uid(&self) -> u32 {
        self.sources[0].owner_uid()
    }
}

/// Numeric identity observed in the provisioned rootfs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerUserIdentity {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaylandSocketAccess {
    Owner,
    Group,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaylandBindPolicy {
    Idmap,
    NoIdmap,
}

/// One display in a grant resolved against the rootfs identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaylandSocketGrant {
    source: HostWaylandSocket,
    socket_access: WaylandSocketAccess,
}

impl WaylandSocketGrant {
    fn resolved(source: HostWaylandSocket, socket_access: WaylandSocketAccess) -> Self {
        Self {
            source,
            socket_access,
        }
    }

    pub fn source(&self) -> &HostWaylandSocket {
        &self.source
    }

    pub fn socket_access(&self) -> WaylandSocketAccess {
        self.socket_access
    }
}

/// A per-user, multi-display grant resolved against the rootfs identity and
/// effective nspawn policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaylandGrant {
    target: ContainerUserIdentity,
    sockets: Vec<WaylandSocketGrant>,
    default_display: WaylandDisplay,
    bind_policy: WaylandBindPolicy,
}

impl<'de> Deserialize<'de> for WaylandGrant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ResolvedGrant {
            target: ContainerUserIdentity,
            sockets: Vec<WaylandSocketGrant>,
            default_display: WaylandDisplay,
            bind_policy: WaylandBindPolicy,
        }

        let grant = ResolvedGrant::deserialize(deserializer)?;
        Self::resolved(
            grant.target,
            grant.sockets,
            grant.default_display,
            grant.bind_policy,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl WaylandGrant {
    pub(crate) fn resolved(
        target: ContainerUserIdentity,
        sockets: Vec<WaylandSocketGrant>,
        default_display: WaylandDisplay,
        bind_policy: WaylandBindPolicy,
    ) -> Result<Self, String> {
        if sockets.is_empty() {
            return Err("Resolved Wayland grant has no sockets".into());
        }
        let mut displays = std::collections::HashSet::new();
        let mut has_default = false;
        for socket in &sockets {
            if socket.source.write_access_for(&target) != Some(socket.socket_access) {
                return Err("Wayland grant does not match socket DAC permissions".into());
            }
            if !displays.insert(socket.source.display().clone()) {
                return Err("Resolved Wayland grant contains a duplicate display".into());
            }
            has_default |= socket.source.display() == &default_display;
        }
        if !has_default {
            return Err("Resolved Wayland default display is not granted".into());
        }
        Ok(Self {
            target,
            sockets,
            default_display,
            bind_policy,
        })
    }

    pub(crate) fn socket(
        source: HostWaylandSocket,
        socket_access: WaylandSocketAccess,
    ) -> WaylandSocketGrant {
        WaylandSocketGrant::resolved(source, socket_access)
    }

    pub fn target(&self) -> &ContainerUserIdentity {
        &self.target
    }

    pub fn sockets(&self) -> &[WaylandSocketGrant] {
        &self.sockets
    }

    pub fn default_display(&self) -> &WaylandDisplay {
        &self.default_display
    }

    pub fn bind_policy(&self) -> WaylandBindPolicy {
        self.bind_policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket(display: &str, path: &str, owner_uid: u32, inode: u64) -> HostWaylandSocket {
        HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new(display).unwrap(),
            format!("/run/user/{owner_uid}").into(),
            path.into(),
            owner_uid,
            owner_uid,
            owner_uid,
            0o700,
            SocketRevision {
                device: 1,
                inode,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap()
    }

    #[test]
    fn display_deserialization_preserves_constructor_invariants() {
        assert_eq!(
            serde_json::from_str::<WaylandDisplay>(r#""wayland-1""#)
                .unwrap()
                .as_str(),
            "wayland-1"
        );
        for invalid in [r#""../wayland-1""#, r#""wayland-1/other""#, r#""x11-0""#] {
            assert!(serde_json::from_str::<WaylandDisplay>(invalid).is_err());
        }
    }

    #[test]
    fn socket_evidence_rejects_unrelated_owner_and_escaped_path() {
        let revision = SocketRevision {
            device: 1,
            inode: 2,
            ctime_seconds: 3,
            ctime_nanoseconds: 4,
        };
        assert!(HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-0").unwrap(),
            "/run/user/1000".into(),
            "/run/user/1000/wayland-0".into(),
            1000,
            1001,
            1000,
            0o700,
            revision,
        )
        .is_err());
        assert!(HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-0").unwrap(),
            "/run/user/1000".into(),
            "/tmp/wayland-0".into(),
            1000,
            1000,
            1000,
            0o700,
            revision,
        )
        .is_err());
    }

    #[test]
    fn socket_evidence_deserialization_preserves_path_and_owner_invariants() {
        let escaped = serde_json::json!({
            "display": "wayland-0",
            "runtime_dir": "/run/user/1000",
            "canonical_path": "/etc/passwd",
            "session_uid": 1000,
            "owner_uid": 1000,
            "owner_gid": 1000,
            "mode": 0o700,
            "revision": {
                "device": 1,
                "inode": 2,
                "ctime_seconds": 3,
                "ctime_nanoseconds": 4
            }
        });
        assert!(serde_json::from_value::<HostWaylandSocket>(escaped).is_err());

        let unrelated_owner = serde_json::json!({
            "display": "wayland-0",
            "runtime_dir": "/run/user/1000",
            "canonical_path": "/run/user/1000/wayland-0",
            "session_uid": 1000,
            "owner_uid": 1001,
            "owner_gid": 1000,
            "mode": 0o700,
            "revision": {
                "device": 1,
                "inode": 2,
                "ctime_seconds": 3,
                "ctime_nanoseconds": 4
            }
        });
        assert!(serde_json::from_value::<HostWaylandSocket>(unrelated_owner).is_err());
    }

    #[test]
    fn dac_class_selection_never_falls_through_to_less_specific_bits() {
        let socket = HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-0").unwrap(),
            "/run/user/1000".into(),
            "/run/user/1000/wayland-0".into(),
            1000,
            1000,
            2000,
            0o022,
            SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap();
        assert_eq!(
            socket.write_access_for(&ContainerUserIdentity {
                username: "owner".into(),
                uid: 1000,
                gid: 2000,
            }),
            None
        );
        assert_eq!(
            socket.write_access_for(&ContainerUserIdentity {
                username: "group".into(),
                uid: 1001,
                gid: 2000,
            }),
            Some(WaylandSocketAccess::Group)
        );

        let group_without_write = HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-0").unwrap(),
            "/run/user/1000".into(),
            "/run/user/1000/wayland-0".into(),
            1000,
            1000,
            2000,
            0o002,
            SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap();
        assert_eq!(
            group_without_write.write_access_for(&ContainerUserIdentity {
                username: "group".into(),
                uid: 1001,
                gid: 2000,
            }),
            None
        );
    }

    #[test]
    fn resolved_grant_deserialization_preserves_dac_evidence() {
        let socket = HostWaylandSocket::from_verified_parts(
            WaylandDisplay::new("wayland-0").unwrap(),
            "/run/user/1000".into(),
            "/run/user/1000/wayland-0".into(),
            1000,
            1000,
            1000,
            0o700,
            SocketRevision {
                device: 1,
                inode: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
        )
        .unwrap();
        let display = socket.display().clone();
        let grant = WaylandGrant::resolved(
            ContainerUserIdentity {
                username: "alice".into(),
                uid: 1000,
                gid: 1000,
            },
            vec![WaylandGrant::socket(socket, WaylandSocketAccess::Owner)],
            display,
            WaylandBindPolicy::Idmap,
        )
        .unwrap();
        let mut value = serde_json::to_value(grant).unwrap();
        value["sockets"][0]["socket_access"] = serde_json::json!("other");

        assert!(serde_json::from_value::<WaylandGrant>(value).is_err());
    }

    #[test]
    fn multi_display_intent_requires_one_owner_unique_sources_and_selected_default() {
        let first = socket("wayland-0", "/run/user/1000/wayland-0", 1000, 1);
        let second = socket("wayland-1", "/run/user/1000/wayland-1", 1000, 2);
        let intent = WaylandGrantIntent::new(
            "alice",
            vec![first.clone(), second.clone()],
            second.display().clone(),
        )
        .unwrap();
        assert_eq!(intent.sources().len(), 2);
        assert_eq!(intent.default_display().as_str(), "wayland-1");

        assert!(WaylandGrantIntent::new(
            "alice",
            vec![first.clone(), first],
            WaylandDisplay::new("wayland-0").unwrap(),
        )
        .is_err());
        assert!(WaylandGrantIntent::new(
            "alice",
            vec![second],
            WaylandDisplay::new("wayland-9").unwrap(),
        )
        .is_err());
        assert!(WaylandGrantIntent::new(
            "alice",
            vec![socket("wayland-2", "/run/user/1001/wayland-2", 1001, 3,)],
            WaylandDisplay::new("wayland-2").unwrap(),
        )
        .is_ok());
        assert!(WaylandGrantIntent::new(
            "alice",
            vec![
                socket("wayland-0", "/run/user/1000/wayland-0", 1000, 1,),
                socket("wayland-2", "/run/user/1001/wayland-2", 1001, 3,),
            ],
            WaylandDisplay::new("wayland-0").unwrap(),
        )
        .is_err());
    }
}

use crate::domain::wayland::{WaylandBindPolicy, WaylandDisplay};
use std::path::{Path, PathBuf};

pub(crate) const CONTAINER_WAYLAND_ROOT: &str = "/run/lasper/wayland";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WaylandBind {
    source: PathBuf,
    target: PathBuf,
    policy: WaylandBindPolicy,
}

impl WaylandBind {
    pub(crate) fn new(
        source: impl Into<PathBuf>,
        target_uid: u32,
        display: &WaylandDisplay,
        policy: WaylandBindPolicy,
    ) -> Self {
        Self {
            source: source.into(),
            target: container_socket_path(target_uid, display),
            policy,
        }
    }

    pub(crate) fn source(&self) -> &Path {
        &self.source
    }

    pub(crate) fn target(&self) -> &Path {
        &self.target
    }

    pub(crate) fn policy(&self) -> WaylandBindPolicy {
        self.policy
    }
}

pub(crate) fn container_socket_path(uid: u32, display: &WaylandDisplay) -> PathBuf {
    Path::new(CONTAINER_WAYLAND_ROOT)
        .join(uid.to_string())
        .join(display.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_is_namespaced_by_numeric_container_identity_and_display() {
        let display = WaylandDisplay::new("wayland-2").unwrap();
        assert_eq!(
            container_socket_path(1001, &display),
            Path::new("/run/lasper/wayland/1001/wayland-2")
        );
    }
}

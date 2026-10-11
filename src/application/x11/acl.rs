//! Exact X server ACL observations.

pub(super) const SERVER_INTERPRETED_FAMILY: u8 = 5;
const LOCAL_USER_KIND: &[u8] = b"localuser";

/// Access-control mode reported by one X server's ListHosts reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11AccessControlMode {
    Enabled,
    Disabled,
    Unknown(u8),
}

impl X11AccessControlMode {
    pub(crate) const fn from_wire(value: u8) -> Self {
        match value {
            0 => Self::Disabled,
            1 => Self::Enabled,
            value => Self::Unknown(value),
        }
    }
}

/// One exact X11 host ACL entry. Raw protocol bytes are retained so a future
/// mutation path can only remove the representation that Lasper actually saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11AclEntry {
    family: u8,
    address: Vec<u8>,
}

impl X11AclEntry {
    pub(crate) fn from_wire(family: u8, address: Vec<u8>) -> Self {
        Self { family, address }
    }

    pub(crate) const fn family(&self) -> u8 {
        self.family
    }

    pub(crate) fn address(&self) -> &[u8] {
        &self.address
    }

    pub fn server_interpreted(&self) -> Option<(&str, &str)> {
        if self.family != SERVER_INTERPRETED_FAMILY {
            return None;
        }
        let separator = self.address.iter().position(|byte| *byte == 0)?;
        let kind = std::str::from_utf8(&self.address[..separator]).ok()?;
        let value = std::str::from_utf8(&self.address[separator + 1..]).ok()?;
        Some((kind, value))
    }

    fn is_numeric_local_user(&self, uid: u32) -> bool {
        let Some((kind, value)) = self.server_interpreted() else {
            return false;
        };
        kind.as_bytes() == LOCAL_USER_KIND && value == format!("#{uid}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11AclSnapshot {
    mode: X11AccessControlMode,
    entries: Vec<X11AclEntry>,
}

impl X11AclSnapshot {
    pub(crate) fn from_wire(mode: u8, entries: Vec<X11AclEntry>) -> Self {
        Self {
            mode: X11AccessControlMode::from_wire(mode),
            entries,
        }
    }

    pub const fn mode(&self) -> X11AccessControlMode {
        self.mode
    }

    pub fn entries(&self) -> &[X11AclEntry] {
        &self.entries
    }

    pub(crate) fn has_numeric_local_user(&self, uid: u32) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.is_numeric_local_user(uid))
    }

    pub(crate) fn contains(&self, entry: &X11AclEntry) -> bool {
        self.entries.contains(entry)
    }
}

//! Guest accounts and observed machine identities.

use crate::domain::machine::MachineName;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// A guest account name accepted by machine1's selected-user shell contract.
/// It is passed as one D-Bus/argv value after validation, never interpolated
/// into a shell command.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ValidatedGuestUserName(String);

impl ValidatedGuestUserName {
    pub fn new(value: impl Into<String>) -> Result<Self, GuestUserNameError> {
        let value = value.into();
        let bytes = value.as_bytes();
        if bytes.is_empty() {
            return Err(GuestUserNameError::Empty);
        }
        if bytes.len() > 32 {
            return Err(GuestUserNameError::TooLong);
        }
        if !value.is_ascii() {
            return Err(GuestUserNameError::NonAscii);
        }
        if bytes[0] == b'-' {
            return Err(GuestUserNameError::LeadingDash);
        }
        if bytes.iter().any(|byte| {
            byte.is_ascii_whitespace()
                || byte.is_ascii_control()
                || matches!(*byte, b'/' | b'\\' | b'@')
        }) {
            return Err(GuestUserNameError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ValidatedGuestUserName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for ValidatedGuestUserName {
    type Error = GuestUserNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ValidatedGuestUserName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GuestUserNameError {
    #[error("guest username cannot be empty")]
    Empty,
    #[error("guest username exceeds the 32-byte limit")]
    TooLong,
    #[error("guest username must be ASCII")]
    NonAscii,
    #[error("guest username cannot start with '-'")]
    LeadingDash,
    #[error("guest username contains whitespace, control, path-separator, or '@' characters")]
    InvalidCharacter,
}

/// A machine and guest account selected for execution or observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestTarget {
    machine: MachineName,
    user: ValidatedGuestUserName,
}

impl GuestTarget {
    pub fn new(machine: MachineName, user: ValidatedGuestUserName) -> Self {
        Self { machine, user }
    }

    pub fn machine(&self) -> &MachineName {
        &self.machine
    }

    pub fn user(&self) -> &ValidatedGuestUserName {
        &self.user
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservedGuestIdentity {
    uid: u32,
    gid: u32,
}

impl ObservedGuestIdentity {
    pub fn new(uid: u32, gid: u32) -> Self {
        Self { uid, gid }
    }

    pub fn uid(self) -> u32 {
        self.uid
    }

    pub fn gid(self) -> u32 {
        self.gid
    }
}

/// Kernel identity of one namespace observed through procfs. Device and inode
/// are kept together because an inode alone is not a complete file identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservedNamespaceIdentity {
    device: u64,
    inode: u64,
}

impl ObservedNamespaceIdentity {
    pub(crate) const fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }

    pub const fn device(self) -> u64 {
        self.device
    }

    pub const fn inode(self) -> u64 {
        self.inode
    }
}

/// Runtime instance against which a guest-to-host identity mapping was
/// observed. A later machine restart invalidates the evidence even if the
/// machine name is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservedMachineInstance {
    leader_pid: u32,
    leader_start_time: Option<std::num::NonZeroU64>,
    namespaces: Option<(ObservedNamespaceIdentity, ObservedNamespaceIdentity)>,
}

impl ObservedMachineInstance {
    pub(crate) const fn new(
        leader_pid: u32,
        pid_namespace: ObservedNamespaceIdentity,
        user_namespace: ObservedNamespaceIdentity,
    ) -> Self {
        Self {
            leader_pid,
            leader_start_time: None,
            namespaces: Some((pid_namespace, user_namespace)),
        }
    }

    pub(crate) const fn from_process(
        leader_pid: u32,
        leader_start_time: std::num::NonZeroU64,
        namespaces: Option<(ObservedNamespaceIdentity, ObservedNamespaceIdentity)>,
    ) -> Self {
        Self {
            leader_pid,
            leader_start_time: Some(leader_start_time),
            namespaces,
        }
    }

    pub const fn leader_pid(self) -> u32 {
        self.leader_pid
    }

    pub const fn leader_start_time(self) -> Option<std::num::NonZeroU64> {
        self.leader_start_time
    }

    pub const fn namespaces(
        self,
    ) -> Option<(ObservedNamespaceIdentity, ObservedNamespaceIdentity)> {
        self.namespaces
    }

    /// None means that the observations have no comparable generation evidence.
    /// It must not be treated as proof of a matching or replaced instance.
    pub fn compare(self, other: Self) -> Option<bool> {
        if self.leader_pid != other.leader_pid {
            return Some(false);
        }
        if let (Some(left), Some(right)) = (self.leader_start_time, other.leader_start_time) {
            if left != right {
                return Some(false);
            }
            return Some(match (self.namespaces, other.namespaces) {
                (Some(left), Some(right)) => left == right,
                _ => true,
            });
        }
        match (self.namespaces, other.namespaces) {
            (Some(left), Some(right)) => Some(left == right),
            _ => None,
        }
    }
}

/// A selected guest account and the UID/GID seen for it by the host kernel.
/// This is runtime evidence, not a prediction from `PrivateUsers=`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedGuestIdentity {
    guest: ObservedGuestIdentity,
    host_uid: u32,
    host_gid: u32,
    instance: ObservedMachineInstance,
}

impl MappedGuestIdentity {
    pub(crate) const fn verified(
        guest: ObservedGuestIdentity,
        host_uid: u32,
        host_gid: u32,
        instance: ObservedMachineInstance,
    ) -> Self {
        Self {
            guest,
            host_uid,
            host_gid,
            instance,
        }
    }

    pub const fn guest(self) -> ObservedGuestIdentity {
        self.guest
    }

    pub const fn host_uid(self) -> u32 {
        self.host_uid
    }

    pub const fn host_gid(self) -> u32 {
        self.host_gid
    }

    pub const fn instance(self) -> ObservedMachineInstance {
        self.instance
    }

    pub fn matches(self, other: Self) -> bool {
        self.guest == other.guest
            && self.host_uid == other.host_uid
            && self.host_gid == other.host_gid
            && self.instance.compare(other.instance) == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_user_name_accepts_numeric_and_common_system_accounts() {
        assert_eq!(
            ValidatedGuestUserName::new("1000").unwrap().as_str(),
            "1000"
        );
        assert_eq!(
            ValidatedGuestUserName::new("alice-1").unwrap().as_str(),
            "alice-1"
        );
        assert_eq!(
            ValidatedGuestUserName::new("nvidia$").unwrap().as_str(),
            "nvidia$"
        );
    }

    #[test]
    fn guest_user_name_rejects_target_and_argument_delimiters() {
        for value in [
            "",
            "-root",
            "alice@host",
            "alice/name",
            "alice\\name",
            "alice name",
            "alice\n",
        ] {
            assert!(ValidatedGuestUserName::new(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn guest_user_name_deserialization_reapplies_validation() {
        let user: ValidatedGuestUserName = serde_json::from_str(r#""alice""#).unwrap();
        assert_eq!(user.as_str(), "alice");
        assert!(serde_json::from_str::<ValidatedGuestUserName>(r#""../root""#).is_err());
    }

    #[test]
    fn process_generation_matches_across_authority_without_reusing_a_pid() {
        let namespace = ObservedNamespaceIdentity::new(1, 2);
        let start = std::num::NonZeroU64::new(77).unwrap();
        let privileged =
            ObservedMachineInstance::from_process(42, start, Some((namespace, namespace)));
        let unprivileged = ObservedMachineInstance::from_process(42, start, None);
        assert_eq!(privileged.compare(unprivileged), Some(true));
        let replacement =
            ObservedMachineInstance::from_process(42, std::num::NonZeroU64::new(78).unwrap(), None);
        assert_eq!(unprivileged.compare(replacement), Some(false));
        assert_eq!(privileged.compare(replacement), Some(false));
        let legacy = ObservedMachineInstance::new(42, namespace, namespace);
        assert_eq!(legacy.compare(privileged), Some(true));
        assert_eq!(legacy.compare(unprivileged), None);
        let changed_namespaces = ObservedMachineInstance::from_process(
            42,
            start,
            Some((namespace, ObservedNamespaceIdentity::new(1, 3))),
        );
        assert_eq!(privileged.compare(changed_namespaces), Some(false));
    }
}

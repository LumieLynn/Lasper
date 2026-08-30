//! Domain models for the machined runtime observation plane.
//!
//! These values describe what was observed from systemd-machined.  They do
//! not imply that Lasper owns or can mutate every observed machine.  Provider
//! classification is deliberately lossless: values not known to Lasper stay
//! available as their original strings instead of being treated as nspawn.

use super::machine::{MachineName, MachineNameError};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// The registration class reported by `org.freedesktop.machine1.Manager`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MachineClass {
    Container,
    Vm,
    Host,
    Unknown(String),
}

impl MachineClass {
    pub fn parse(value: impl Into<String>) -> Self {
        let value = value.into();
        match value.as_str() {
            "container" => Self::Container,
            "vm" => Self::Vm,
            "host" => Self::Host,
            _ => Self::Unknown(value),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Container => "container",
            Self::Vm => "vm",
            Self::Host => "host",
            Self::Unknown(value) => value,
        }
    }

    pub fn is_container(&self) -> bool {
        matches!(self, Self::Container)
    }
}

impl Serialize for MachineClass {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MachineClass {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::parse(String::deserialize(deserializer)?))
    }
}

impl fmt::Display for MachineClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The service/provider which registered a machine with machined.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MachineProvider {
    Nspawn,
    Vmspawn,
    Unknown(String),
}

impl MachineProvider {
    pub fn parse(value: impl Into<String>) -> Self {
        let value = value.into();
        match value.as_str() {
            "systemd-nspawn" => Self::Nspawn,
            "systemd-vmspawn" => Self::Vmspawn,
            _ => Self::Unknown(value),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Nspawn => "systemd-nspawn",
            Self::Vmspawn => "systemd-vmspawn",
            Self::Unknown(value) => value,
        }
    }

    pub fn is_nspawn(&self) -> bool {
        matches!(self, Self::Nspawn)
    }
}

/// The operation surface Lasper may use for a registered machine.
///
/// machined is a shared observation plane. A machine can therefore be
/// visible to Lasper without being an nspawn resource that Lasper may mutate.
/// The reason is intentionally semantic data; presentation maps it to text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MachineAccess {
    Nspawn,
    ReadOnly(ReadOnlyReason),
}

impl MachineAccess {
    pub fn is_nspawn(&self) -> bool {
        matches!(self, Self::Nspawn)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadOnlyReason {
    VirtualMachine,
    Host,
    UnknownClass(String),
    UnknownProvider(String),
    UnsupportedCombination { class: String, provider: String },
    InvalidIdentity,
}

impl Serialize for MachineProvider {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MachineProvider {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::parse(String::deserialize(deserializer)?))
    }
}

impl fmt::Display for MachineProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated, provider-aware identity for a registered machine.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MachineRuntimeIdentity {
    name: MachineName,
    class: MachineClass,
    provider: MachineProvider,
}

impl MachineRuntimeIdentity {
    pub fn new(
        name: impl Into<String>,
        class: impl Into<String>,
        service: impl Into<String>,
    ) -> Result<Self, RuntimeIdentityError> {
        Ok(Self {
            name: MachineName::new(name).map_err(RuntimeIdentityError::InvalidMachineName)?,
            class: MachineClass::parse(class),
            provider: MachineProvider::parse(service),
        })
    }

    pub fn name(&self) -> &MachineName {
        &self.name
    }

    pub fn class(&self) -> &MachineClass {
        &self.class
    }

    pub fn provider(&self) -> &MachineProvider {
        &self.provider
    }

    /// Only this exact pair is eligible for nspawn-specific operations.
    pub fn is_nspawn(&self) -> bool {
        self.class().is_container() && self.provider().is_nspawn()
    }

    pub fn access(&self) -> MachineAccess {
        if self.is_nspawn() {
            return MachineAccess::Nspawn;
        }

        let reason = match (self.class(), self.provider()) {
            (MachineClass::Vm, _) | (_, MachineProvider::Vmspawn) => ReadOnlyReason::VirtualMachine,
            (MachineClass::Host, _) => ReadOnlyReason::Host,
            (MachineClass::Unknown(class), _) => ReadOnlyReason::UnknownClass(class.clone()),
            (_, MachineProvider::Unknown(provider)) => {
                ReadOnlyReason::UnknownProvider(provider.clone())
            }
            (class, provider) => ReadOnlyReason::UnsupportedCombination {
                class: class.to_string(),
                provider: provider.to_string(),
            },
        };
        MachineAccess::ReadOnly(reason)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeIdentityError {
    InvalidMachineName(MachineNameError),
}

impl fmt::Display for RuntimeIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMachineName(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RuntimeIdentityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidMachineName(error) => Some(error),
        }
    }
}

/// The state exposed by the runtime observation plane.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum MachineState {
    Running,
    Starting,
    Exiting,
    /// A state not represented by Lasper's lifecycle projection.
    Unknown(String),
}

impl MachineState {
    pub fn from_systemd(value: impl Into<String>) -> Self {
        let value = value.into();
        match value.as_str() {
            "active" | "running" | "Running" => Self::Running,
            "activating" | "opening" | "starting" | "Starting" => Self::Starting,
            "deactivating" | "closing" | "exiting" | "Exiting" => Self::Exiting,
            _ => Self::Unknown(value),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Running => "running",
            Self::Starting => "starting",
            Self::Exiting => "exiting",
            Self::Unknown(value) => value,
        }
    }

    /// Whether the machine is a stable runtime target for lifecycle commands.
    pub fn accepts_runtime_actions(&self) -> bool {
        matches!(self, Self::Running)
    }
}

impl Serialize for MachineState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = match self {
            Self::Running => "Running",
            Self::Starting => "Starting",
            Self::Exiting => "Exiting",
            Self::Unknown(value) => value,
        };
        serializer.serialize_str(value)
    }
}

impl<'de> Deserialize<'de> for MachineState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_systemd(String::deserialize(deserializer)?))
    }
}

/// A machine registered with systemd-machined.
///
/// The raw `class` and `service` strings remain in the snapshot shape during
/// this migration so old daemon/client peers cannot silently reinterpret the
/// JSON contract. Call [`MachineEntry::identity`] at an application boundary
/// to obtain the validated semantic view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineEntry {
    pub name: String,
    pub class: String,
    pub service: String,
    pub state: MachineState,
    pub address: Option<String>,
    pub all_addresses: Vec<String>,
}

impl MachineEntry {
    pub const NSPAWN_CLASS: &'static str = "container";
    pub const NSPAWN_SERVICE: &'static str = "systemd-nspawn";

    pub fn optimistic_nspawn(name: impl Into<String>, state: MachineState) -> Self {
        Self {
            name: name.into(),
            class: Self::NSPAWN_CLASS.into(),
            service: Self::NSPAWN_SERVICE.into(),
            state,
            address: None,
            all_addresses: Vec::new(),
        }
    }

    pub fn identity(&self) -> Result<MachineRuntimeIdentity, RuntimeIdentityError> {
        MachineRuntimeIdentity::new(self.name.clone(), self.class.clone(), self.service.clone())
    }

    pub fn validated_name(&self) -> Result<MachineName, RuntimeIdentityError> {
        self.identity().map(|identity| identity.name().clone())
    }

    pub fn access(&self) -> MachineAccess {
        self.identity()
            .map(|identity| identity.access())
            .unwrap_or(MachineAccess::ReadOnly(ReadOnlyReason::InvalidIdentity))
    }
}

/// A persistent machine image known to systemd-machined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageEntry {
    pub name: String,
    pub image_type: String,
    pub readonly: bool,
    pub usage: Option<String>,
    /// The object returned by `org.freedesktop.machine1.Manager.ListImages`.
    /// This is a D-Bus address, not the image's backing filesystem path.
    pub dbus_object_path: Option<String>,
}

/// A validated systemd machine-image name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ImageName(String);

impl ImageName {
    pub fn new(name: impl Into<String>) -> Result<Self, ImageNameError> {
        let name = name.into();
        if ImageEntry::is_valid_name(&name) {
            Ok(Self(name))
        } else {
            Err(ImageNameError(name))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ImageName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<'de> Deserialize<'de> for ImageName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageNameError(String);

impl fmt::Display for ImageNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid image name {:?}", self.0)
    }
}

impl std::error::Error for ImageNameError {}

/// A point-in-time view of the machine manager's persistent state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub machines: Vec<MachineEntry>,
    pub images: Vec<ImageEntry>,
}

impl RuntimeSnapshot {
    pub fn new(mut machines: Vec<MachineEntry>, mut images: Vec<ImageEntry>) -> Self {
        for machine in &mut machines {
            machine.all_addresses.retain(|address| !address.is_empty());
            machine.all_addresses.sort();
            machine.all_addresses.dedup();
            machine.address = machine.all_addresses.first().cloned();
        }
        machines.sort();
        images.sort();
        Self { machines, images }
    }
}

/// Notification emitted by a status observer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusUpdate {
    Snapshot(RuntimeSnapshot),
    Dirty,
    BackendFailure {
        message: String,
        consecutive_failures: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageVisibility {
    Regular,
    Hidden,
}

impl ImageVisibility {
    pub fn label(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Hidden => "hidden",
        }
    }
}

impl ImageEntry {
    pub fn validated_name(&self) -> Result<ImageName, ImageNameError> {
        ImageName::new(self.name.clone())
    }

    pub fn is_protected_name(name: &str) -> bool {
        name == ".host"
    }

    pub fn is_hidden_name(name: &str) -> bool {
        name.starts_with('.')
    }

    pub fn visibility(&self) -> ImageVisibility {
        if Self::is_hidden_name(&self.name) {
            ImageVisibility::Hidden
        } else {
            ImageVisibility::Regular
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.visibility() == ImageVisibility::Hidden
    }

    pub fn removal_label(&self) -> &'static str {
        if Self::is_protected_name(&self.name) {
            "blocked: host image"
        } else {
            "available"
        }
    }

    /// Image names are path components, not necessarily machine names.
    pub fn is_valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 255
            && name != "."
            && name != ".."
            && !name.starts_with(".#")
            && !name.contains('/')
            && !name.bytes().any(|byte| byte < b' ' || byte == 0x7f)
    }
}

impl Ord for ImageEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name)
            .then(self.image_type.cmp(&other.image_type))
            .then(self.readonly.cmp(&other.readonly))
            .then(self.usage.cmp(&other.usage))
            .then(self.dbus_object_path.cmp(&other.dbus_object_path))
    }
}

impl PartialOrd for ImageEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MachineEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name)
            .then(self.class.cmp(&other.class))
            .then(self.service.cmp(&other.service))
            .then(self.state.cmp(&other.state))
            .then(self.address.cmp(&other.address))
            .then(self.all_addresses.cmp(&other.all_addresses))
    }
}

impl PartialOrd for MachineEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown_runtime_providers_are_lossless() {
        assert_eq!(MachineClass::parse("container"), MachineClass::Container);
        assert_eq!(
            MachineProvider::parse("systemd-nspawn"),
            MachineProvider::Nspawn
        );
        assert_eq!(
            MachineProvider::parse("libvirt"),
            MachineProvider::Unknown("libvirt".into())
        );
        assert_eq!(
            MachineProvider::Unknown("custom-service".into()).as_str(),
            "custom-service"
        );
    }

    #[test]
    fn nspawn_identity_requires_both_container_class_and_nspawn_provider() {
        let nspawn = MachineRuntimeIdentity::new("web", "container", "systemd-nspawn").unwrap();
        assert!(nspawn.is_nspawn());

        let vm = MachineRuntimeIdentity::new("web", "vm", "systemd-nspawn").unwrap();
        assert!(!vm.is_nspawn());

        let foreign = MachineRuntimeIdentity::new("web", "container", "libvirt").unwrap();
        assert!(!foreign.is_nspawn());
    }

    #[test]
    fn unknown_machine_states_round_trip_without_becoming_running() {
        let state = MachineState::from_systemd("maintenance");
        assert_eq!(state, MachineState::Unknown("maintenance".into()));
        assert!(!state.accepts_runtime_actions());
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"maintenance\"");
        assert_eq!(serde_json::from_str::<MachineState>(&json).unwrap(), state);
    }

    #[test]
    fn machine_entry_exposes_typed_identity_without_reinterpreting_wire_fields() {
        let entry = MachineEntry {
            name: "guest".into(),
            class: "container".into(),
            service: "systemd-nspawn".into(),
            state: MachineState::Running,
            address: None,
            all_addresses: Vec::new(),
        };
        assert!(entry.access().is_nspawn());
        assert_eq!(entry.identity().unwrap().name().as_str(), "guest");
        assert_eq!(entry.access(), MachineAccess::Nspawn);
    }

    #[test]
    fn non_nspawn_runtime_identities_are_read_only_with_lossless_reasons() {
        let vm = MachineRuntimeIdentity::new("guest", "vm", "systemd-vmspawn").unwrap();
        assert_eq!(
            vm.access(),
            MachineAccess::ReadOnly(ReadOnlyReason::VirtualMachine)
        );

        let foreign = MachineRuntimeIdentity::new("guest", "container", "libvirt").unwrap();
        assert_eq!(
            foreign.access(),
            MachineAccess::ReadOnly(ReadOnlyReason::UnknownProvider("libvirt".into()))
        );

        let host = MachineRuntimeIdentity::new("host", "host", "systemd-nspawn").unwrap();
        assert_eq!(host.access(), MachineAccess::ReadOnly(ReadOnlyReason::Host));
    }

    #[test]
    fn malformed_machine_entries_never_gain_nspawn_access() {
        let entry = MachineEntry {
            name: "not a machine".into(),
            class: "container".into(),
            service: "systemd-nspawn".into(),
            state: MachineState::Running,
            address: None,
            all_addresses: Vec::new(),
        };
        assert_eq!(
            entry.access(),
            MachineAccess::ReadOnly(ReadOnlyReason::InvalidIdentity)
        );
    }
}

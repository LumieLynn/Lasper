use crate::domain::inspection::{
    GROUP_DEPENDENCIES, GROUP_MACHINE, GROUP_SYSTEMD_SERVICE, GROUP_SYSTEMD_UNIT,
};

/// Keys that should be routed to the Dependencies group instead of a systemd
/// interface group.
const DEPENDENCY_KEYS: &[&str] = &[
    "After",
    "Before",
    "Wants",
    "WantedBy",
    "Requires",
    "RequiredBy",
    "Conflicts",
    "ConflictedBy",
    "Requisite",
    "BindsTo",
    "PartOf",
    "Upholds",
    "RequisiteOf",
    "BoundBy",
    "UpheldBy",
    "ConsistsOf",
    "OnSuccess",
    "OnSuccessOf",
    "OnFailure",
    "OnFailureOf",
    "Triggers",
    "TriggeredBy",
    "PropagatesReloadTo",
    "ReloadPropagatedFrom",
    "PropagatesStopTo",
    "StopPropagatedFrom",
    "JoinsNamespaceOf",
    "SliceOf",
    "RequiresMountsFor",
    "WantsMountsFor",
    // Deprecated aliases retained by systemd's vtable for compatibility.
    "RequiresOverridable",
    "RequisiteOverridable",
    "RequiredByOverridable",
    "RequisiteOfOverridable",
];

/// Base Unit properties from systemd's `bus_unit_vtable` (v261.2), with legacy aliases.
///
/// `systemctl show` requests `Properties.GetAll("")`, which merges the base
/// unit vtable with the type-specific vtables and their fallback vtables. It
/// does not include an interface name in its line-oriented output. Keeping
/// the base list here lets the tools backend follow the observed D-Bus boundary:
/// known base-unit properties belong to `Systemd Unit`; everything else from
/// a service object belongs to `Systemd Service`, including execution and
/// cgroup properties.
const UNIT_PROPERTY_KEYS: &[&str] = &[
    "Id",
    "Names",
    "Following",
    "Documentation",
    "Description",
    "AccessSELinuxContext",
    "LoadState",
    "ActiveState",
    "FreezerState",
    "SubState",
    "FragmentPath",
    "SourcePath",
    "DropInPaths",
    "UnitFileState",
    "UnitFilePreset",
    "StateChangeTimestamp",
    "InactiveExitTimestamp",
    "ActiveEnterTimestamp",
    "ActiveExitTimestamp",
    "InactiveEnterTimestamp",
    "CanStart",
    "CanStop",
    "CanReload",
    "CanIsolate",
    "CanClean",
    "CanFreeze",
    "CanLiveMount",
    "Job",
    "StopWhenUnneeded",
    "RefuseManualStart",
    "RefuseManualStop",
    "AllowIsolate",
    "DefaultDependencies",
    "SurviveFinalKillSignal",
    "OnSuccesJobMode",
    "OnSuccessJobMode",
    "OnFailureJobMode",
    "IgnoreOnIsolate",
    "NeedDaemonReload",
    "Markers",
    "JobTimeoutUSec",
    "JobRunningTimeoutUSec",
    "JobTimeoutAction",
    "JobTimeoutRebootArgument",
    "ConditionResult",
    "AssertResult",
    "ConditionTimestamp",
    "AssertTimestamp",
    "Conditions",
    "Asserts",
    "LoadError",
    "Transient",
    "Perpetual",
    "StartLimitIntervalUSec",
    "StartLimitBurst",
    "StartLimitAction",
    "FailureAction",
    "FailureActionExitStatus",
    "SuccessAction",
    "SuccessActionExitStatus",
    "RebootArgument",
    "InvocationID",
    "CollectMode",
    "Refs",
    "ActivationDetails",
    "DebugInvocation",
    // Hidden compatibility aliases from the Unit vtable.
    "StartLimitInterval",
    "StartLimitIntervalSec",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyScope {
    Machine,
    Unit,
    Service,
}

impl PropertyScope {
    pub(crate) fn interface(self) -> &'static str {
        match self {
            Self::Machine => "org.freedesktop.machine1.Machine",
            Self::Unit => "org.freedesktop.systemd1.Unit",
            Self::Service => "org.freedesktop.systemd1.Service",
        }
    }

    pub(crate) fn group(self, name: &str) -> &'static str {
        match self {
            Self::Machine => GROUP_MACHINE,
            Self::Unit if is_dependency(name) => GROUP_DEPENDENCIES,
            Self::Unit => GROUP_SYSTEMD_UNIT,
            Self::Service => GROUP_SYSTEMD_SERVICE,
        }
    }
}

fn is_dependency(name: &str) -> bool {
    DEPENDENCY_KEYS.contains(&name)
}

/// systemctl show emits flat properties for the service object. Classify its
/// base Unit properties using the same public vtable boundary; dual timestamps
/// also export a Monotonic companion. D-Bus ingestion uses the observed
/// interface instead, so newly introduced Unit fields keep their true origin.
pub(crate) fn tool_scope(name: &str) -> PropertyScope {
    let base = name.strip_suffix("Monotonic").unwrap_or(name);
    if is_dependency(name) || UNIT_PROPERTY_KEYS.contains(&base) {
        PropertyScope::Unit
    } else {
        PropertyScope::Service
    }
}

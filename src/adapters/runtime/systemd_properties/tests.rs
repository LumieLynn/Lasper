use super::*;
use crate::domain::inspection::{
    InspectionCompleteness, InspectionSource, PropertyArray, GROUP_DEPENDENCIES, GROUP_MACHINE,
    GROUP_SYSTEMD_SERVICE, GROUP_SYSTEMD_UNIT,
};
use zbus::zvariant::{Array, ObjectPath, StructureBuilder};

fn dbus_snapshot(scope: PropertyScope, values: Vec<(&str, Value<'_>)>) -> MachineProperties {
    let mut snapshot =
        MachineProperties::from_inspection(InspectionSource::Dbus, InspectionCompleteness::Full);
    append_dbus(
        &mut snapshot,
        scope,
        values
            .into_iter()
            .map(|(name, value)| (name.into(), OwnedValue::try_from(value).unwrap()))
            .collect(),
    );
    snapshot
}

#[test]
fn systemd_scalar_and_bitmap_values_match_show_output() {
    // Expected text follows bus-print-properties.c and systemctl-show.c:
    // semantic masks, counters, limits and durations have different rules.
    let fixtures = vec![
        (
            "CapabilityBoundingSet",
            Value::U64((1 << 0) | (1 << 21)),
            "cap_chown cap_sys_admin",
        ),
        (
            "EffectiveCPUs",
            Value::Array(Array::from(vec![255u8, 255, 15, 0, 0, 0, 0, 0])),
            "0-19",
        ),
        (
            "StartupAllowedCPUs",
            Value::Array(Array::from(vec![3u8])),
            "0-1",
        ),
        ("CPUAffinity", Value::Array(Array::from(vec![5u8])), "0 2"),
        (
            "InvocationID",
            Value::Array(Array::from(vec![0xaau8, 0xbb])),
            "aabb",
        ),
        ("UMask", Value::U32(0o22), "0022"),
        ("NUMAPolicy", Value::I32(-1), "n/a"),
        ("MountFlags", Value::U64(0), ""),
        ("MountFlags", Value::U64(libc::MS_PRIVATE), "private"),
        ("UID", Value::U32(u32::MAX), "[not set]"),
        ("CPUWeight", Value::U64(0), "idle"),
        ("IOWeight", Value::U64(0), "0"),
        ("MemoryCurrent", Value::U64(u64::MAX), "[not set]"),
        ("MemoryMax", Value::U64(u64::MAX), "infinity"),
        ("EffectiveTasksMax", Value::U64(u64::MAX), "infinity"),
        ("CPUUsageNSec", Value::U64(1_000_000_000), "1000000000"),
        ("CPUUsageNSec", Value::U64(u64::MAX), "[not set]"),
        ("IPIngressBytes", Value::U64(u64::MAX), "[no data]"),
        ("StartLimitIntervalUSec", Value::U64(10_000_000), "10s"),
        (
            "TimeoutStartUSec",
            Value::U64(90_000_001),
            "1min 30.000001s",
        ),
        ("ActiveEnterTimestampMonotonic", Value::U64(55), "55"),
        ("ActiveExitTimestamp", Value::U64(0), ""),
        (
            "UnknownCounter",
            Value::U64(u64::MAX),
            "18446744073709551615",
        ),
        (
            "StandardInputData",
            Value::Array(Array::from(vec![b'H', b'i'])),
            "SGk=",
        ),
        (
            "Environment",
            Value::Array(Array::from(vec![
                "",
                "/tmp/space name",
                "hello$\u{7}",
                "你好",
            ])),
            "\"\" \"/tmp/space name\" \"hello\\$\\a\" 你好",
        ),
    ];
    for (name, value, expected) in fixtures {
        let decoded = decode::decode(&value);
        assert_eq!(render::render(name, &decoded), expected, "{name}");
        assert_eq!(decoded.signature(), value.value_signature().to_string());
        assert_eq!(
            render::render(name, &PropertyValue::text(expected)),
            expected
        );
    }
}

#[test]
fn type_driven_rules_do_not_reinterpret_strings_or_unknown_structures() {
    let value = PropertyValue::String("2048".into());
    assert_eq!(render::render("MemoryMax", &value), "2048");
    assert_eq!(render::render("ExecMainPID", &PropertyValue::U32(42)), "42");
    let wrong_signature = PropertyValue::Structure(vec![PropertyValue::U32(7)]);
    assert_eq!(
        render::render("LoadError", &wrong_signature),
        "[unprintable]"
    );
    let empty = PropertyValue::Array(PropertyArray {
        element_signature: "y".into(),
        values: vec![],
    });
    assert_eq!(render::render("EffectiveCPUs", &empty), "");
}

#[test]
fn unsupported_systemd_compounds_match_tools_output() {
    let activation_details = PropertyValue::Array(PropertyArray {
        element_signature: "(ss)".into(),
        values: Vec::new(),
    });
    assert_eq!(
        render::render("ActivationDetails", &activation_details),
        "[unprintable]"
    );

    let quota_usage = PropertyValue::Structure(vec![
        PropertyValue::U64(u64::MAX),
        PropertyValue::U64(4_073_709_551_615),
    ]);
    assert_eq!(
        render::render("CacheDirectoryQuotaUsage", &quota_usage),
        "[unprintable]"
    );
    assert_eq!(
        render::render("TTYColumns", &PropertyValue::U16(120)),
        "[unprintable]"
    );
}

#[test]
fn compound_values_follow_systemd_bind_and_ip_policy_semantics() {
    let binds = PropertyValue::Array(PropertyArray {
        element_signature: "(ssbt)".into(),
        values: [
            ("/src", "/dest", true, libc::MS_REC),
            ("/other", "/other", false, 1),
            ("", "/unused", false, 0),
        ]
        .into_iter()
        .map(|(source, destination, ignore, flags)| {
            PropertyValue::Structure(vec![
                PropertyValue::String(source.into()),
                PropertyValue::String(destination.into()),
                PropertyValue::Bool(ignore),
                PropertyValue::U64(flags),
            ])
        })
        .collect(),
    });
    assert_eq!(
        render::render("BindReadOnlyPaths", &binds),
        "-/src:/dest:rbind /other:/other"
    );
    let address = "2001:db8::".parse::<std::net::Ipv6Addr>().unwrap();
    let policy = PropertyValue::Array(PropertyArray {
        element_signature: "(iayu)".into(),
        values: vec![PropertyValue::Structure(vec![
            PropertyValue::I32(libc::AF_INET6),
            PropertyValue::Array(PropertyArray {
                element_signature: "y".into(),
                values: address
                    .octets()
                    .into_iter()
                    .map(PropertyValue::U8)
                    .collect(),
            }),
            PropertyValue::U32(32),
        ])],
    });
    assert_eq!(render::render("IPAddressAllow", &policy), "2001:db8::/32");
}

#[test]
fn backend_snapshots_share_groups_and_keep_actual_interfaces() {
    let mut dbus = dbus_snapshot(
        PropertyScope::Unit,
        vec![
            ("ActiveState", Value::from("active")),
            ("ActiveEnterTimestampMonotonic", Value::U64(100)),
            (
                "After",
                Value::Array(Array::from(vec!["basic.target", "machines.target"])),
            ),
        ],
    );
    append_dbus(
        &mut dbus,
        PropertyScope::Service,
        HashMap::from([
            (
                "MainPID".into(),
                OwnedValue::try_from(Value::U32(42)).unwrap(),
            ),
            (
                "EffectiveCPUs".into(),
                OwnedValue::try_from(Value::Array(Array::from(vec![255u8, 255, 15]))).unwrap(),
            ),
        ]),
    );
    let mut tools = MachineProperties::default();
    for (name, text) in [
        ("ActiveState", "active"),
        ("ActiveEnterTimestampMonotonic", "100"),
        ("After", "basic.target machines.target"),
        ("MainPID", "42"),
        ("EffectiveCPUs", "0-19"),
    ] {
        append_tool_property(&mut tools, name.into(), text.into());
    }
    for group in [
        GROUP_SYSTEMD_UNIT,
        GROUP_SYSTEMD_SERVICE,
        GROUP_DEPENDENCIES,
    ] {
        for (name, entry) in dbus.get_group(group).unwrap() {
            assert_eq!(tools.get_display(group, name), Some(entry.display.as_str()));
            assert!(entry.interface.is_some());
            assert!(tools.get_group(group).unwrap()[name].interface.is_none());
        }
    }
    assert_eq!(dbus.systemd_display("MainPID"), Some("42"));
    let unknown = dbus_snapshot(
        PropertyScope::Unit,
        vec![("FutureUnitField", Value::U32(9))],
    );
    assert_eq!(
        unknown.get_display(GROUP_SYSTEMD_UNIT, "FutureUnitField"),
        Some("9")
    );
}

#[test]
fn snapshot_round_trip_preserves_nested_types_and_empty_array_signatures() {
    let structure = StructureBuilder::new()
        .add_field(7u32)
        .add_field(ObjectPath::try_from("/org/freedesktop/systemd1/job/7").unwrap())
        .build()
        .unwrap();
    let snapshot = dbus_snapshot(
        PropertyScope::Unit,
        vec![
            ("Job", Value::Structure(structure)),
            ("Names", Value::Array(Array::from(Vec::<String>::new()))),
            ("InvocationID", Value::Array(Array::from(Vec::<u8>::new()))),
        ],
    );
    let wire = serde_json::to_value(&snapshot).unwrap();
    let received: MachineProperties = serde_json::from_value(wire).unwrap();
    for name in ["Job", "Names", "InvocationID"] {
        let before = &snapshot.get_group(GROUP_SYSTEMD_UNIT).unwrap()[name];
        let after = &received.get_group(GROUP_SYSTEMD_UNIT).unwrap()[name];
        assert_eq!(after, before);
    }
    assert_eq!(
        received.get_group(GROUP_SYSTEMD_UNIT).unwrap()["Names"]
            .value
            .signature(),
        "as"
    );
    assert_eq!(
        received.get_group(GROUP_SYSTEMD_UNIT).unwrap()["InvocationID"]
            .value
            .signature(),
        "ay"
    );
    assert_eq!(received.get_display(GROUP_SYSTEMD_UNIT, "Job"), Some("7"));
}

#[test]
fn exec_status_is_formatted_as_a_command_without_touching_scalar_metadata() {
    let command = StructureBuilder::new()
        .add_field("/usr/bin/true")
        .add_field(vec!["/usr/bin/true"])
        .add_field(false)
        .add_field(0u64)
        .add_field(0u64)
        .add_field(0u64)
        .add_field(0u64)
        .add_field(42u32)
        .add_field(1i32)
        .add_field(0i32)
        .build()
        .unwrap();
    let mut commands = Array::new(command.signature());
    commands.append(Value::Structure(command)).unwrap();
    let snapshot = dbus_snapshot(
        PropertyScope::Service,
        vec![("ExecStart", Value::Array(commands))],
    );
    assert_eq!(snapshot.systemd_display("ExecStart"), Some(
        "{ path=/usr/bin/true ; argv[]=/usr/bin/true ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=42 ; code=exited ; status=0 }"
    ));
    let machine = dbus_snapshot(
        PropertyScope::Machine,
        vec![("Unit", Value::from("test.scope"))],
    );
    assert_eq!(
        machine.get_display(GROUP_MACHINE, "Unit"),
        Some("test.scope")
    );
}

#[tokio::test]
#[ignore = "requires a host systemd system bus; no units are modified"]
async fn live_systemd_properties_match_systemctl() {
    let connection = zbus::Connection::system().await.unwrap();
    let reply = connection
        .call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "GetUnit",
            &("systemd-journald.service",),
        )
        .await
        .unwrap();
    let path: zbus::zvariant::OwnedObjectPath = reply.body().deserialize().unwrap();
    let proxy = zbus::fdo::PropertiesProxy::builder(&connection)
        .destination("org.freedesktop.systemd1")
        .unwrap()
        .path(path)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut snapshot = MachineProperties::default();
    for scope in [PropertyScope::Unit, PropertyScope::Service] {
        let values = proxy
            .get_all(scope.interface().try_into().unwrap())
            .await
            .unwrap();
        append_dbus(&mut snapshot, scope, values);
    }
    let output = std::process::Command::new("systemctl")
        .args(["--no-pager", "--all", "show", "systemd-journald.service"])
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let tool_values: HashMap<_, _> = tool_properties(text.as_bytes()).into_iter().collect();
    // Compare every shared stable property, including its Unit/Service group.
    // Counters sampled by the two sequential requests can legitimately differ.
    let is_volatile = |name: &str| {
        matches!(name, "CPUUsageNSec" | "TasksCurrent" | "MemoryAvailable")
            || (name.starts_with("Memory") && (name.ends_with("Current") || name.ends_with("Peak")))
            || (name.starts_with("IO") && (name.ends_with("Bytes") || name.ends_with("Operations")))
            || (name.starts_with("IP") && (name.ends_with("Bytes") || name.ends_with("Packets")))
            || name.starts_with("WatchdogTimestamp")
    };
    let mut tools = MachineProperties::default();
    for (name, value) in &tool_values {
        append_tool_property(&mut tools, name.clone(), value.clone());
    }
    let mut compared = 0;
    let mut differences = Vec::new();
    for group in &snapshot.groups {
        for (name, entry) in &group.properties {
            if is_volatile(name) {
                continue;
            }
            let Some(expected) = tool_values.get(name.as_str()) else {
                continue;
            };
            if tools.get_display(&group.name, name).is_none() {
                differences.push(format!(
                    "{name}: tools did not classify it into {}",
                    group.name
                ));
            }
            compared += 1;
            if &entry.display != expected {
                differences.push(format!(
                    "{name}: expected {expected:?}, actual {:?}",
                    entry.display
                ));
            }
        }
    }
    assert!(
        compared >= 200,
        "only {compared} shared properties were queried"
    );
    assert!(differences.is_empty(), "{}", differences.join("\n"));
    println!("compared {compared} stable systemd properties");
}

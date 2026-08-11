//! Local, read-only inspection of systemd-machined runtime registration state.
//!
//! systemd exposes a small public `sd-login` API over this state directory, but
//! it does not expose the complete machine property set. This reader is used
//! only by explicit CLI mode so opening the details pane cannot invoke a D-Bus
//! client indirectly through `machinectl show` or `systemctl show`.

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{
    ContainerEntry, InspectionCompleteness, InspectionSource, MachineName, MachineProperties,
    GROUP_MACHINE,
};
use std::collections::HashMap;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MAX_RUNTIME_STATE_BYTES: u64 = 64 * 1024;

/// Enumerate runtime registrations without asking machined to inspect the
/// containers. This mirrors `sd_get_machine_names()` at the public API level:
/// names come from the runtime directory, while `unit:` helper symlinks and
/// invalid machine names are ignored.
pub(crate) async fn list_machines_at(path: PathBuf) -> Result<Vec<ContainerEntry>> {
    let display_path = path.clone();
    tokio::task::spawn_blocking(move || enumerate_machines(&path))
        .await
        .map_err(|error| {
            NspawnError::Runtime(format!("runtime machine enumeration task failed: {error}"))
        })?
        .map_err(|error| NspawnError::Io(display_path, error))
}

fn enumerate_machines(path: &Path) -> std::io::Result<Vec<ContainerEntry>> {
    let mut machines = Vec::new();
    let directory = match std::fs::read_dir(path) {
        Ok(directory) => directory,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(machines),
        Err(error) => return Err(error),
    };
    for entry in directory {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if MachineName::new(&name).is_err() {
            continue;
        }
        machines.push(ContainerEntry {
            name,
            state: crate::nspawn::models::ContainerState::Running,
            address: None,
            all_addresses: Vec::new(),
        });
    }
    machines.sort();
    Ok(machines)
}

/// Inspect one machine without contacting either systemd D-Bus service.
pub async fn inspect(name: &str, entry: &ContainerEntry) -> Result<MachineProperties> {
    let name =
        MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))?;
    inspect_at(
        crate::paths::runtime_machine_state(name.as_str()),
        name,
        entry,
    )
    .await
}

async fn inspect_at(
    path: PathBuf,
    name: MachineName,
    entry: &ContainerEntry,
) -> Result<MachineProperties> {
    let mut properties = entry_properties(entry);
    let display_path = path.display().to_string();
    let expected_name = name.into_string();
    let read = tokio::task::spawn_blocking(move || read_runtime_state(&path, &expected_name))
        .await
        .map_err(|error| {
            NspawnError::Runtime(format!("runtime state reader task failed: {error}"))
        })?;

    match read {
        Ok(fields) => {
            insert_runtime_fields(&mut properties, fields);
            properties.insert(GROUP_MACHINE, "RuntimeStateFile".into(), display_path);
        }
        Err(error) => {
            // A registration may disappear between the snapshot and detail
            // refresh. Keep the snapshot-derived fields useful, while making
            // permission and format failures visible instead of switching
            // transports and unexpectedly invoking polkit.
            let reason = if error.kind() == ErrorKind::NotFound {
                "registration changed before it could be inspected".to_string()
            } else {
                error.to_string()
            };
            properties.insert(
                GROUP_MACHINE,
                "RuntimeStateRead".into(),
                format!("unavailable ({display_path}): {reason}"),
            );
        }
    }

    Ok(properties)
}

fn entry_properties(entry: &ContainerEntry) -> MachineProperties {
    let mut properties = MachineProperties::from_inspection(
        InspectionSource::RuntimeState,
        InspectionCompleteness::RuntimeOnly,
    );
    properties.insert(GROUP_MACHINE, "Name".into(), entry.name.clone());
    properties.insert(
        GROUP_MACHINE,
        "State".into(),
        entry.state.label().to_string(),
    );
    if !entry.all_addresses.is_empty() {
        properties.insert(
            GROUP_MACHINE,
            "IPAddresses".into(),
            entry.all_addresses.join(", "),
        );
    }
    properties
}

fn read_runtime_state(
    path: &Path,
    expected_name: &str,
) -> std::io::Result<HashMap<String, String>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "runtime state is not a regular file",
        ));
    }
    if metadata.len() > MAX_RUNTIME_STATE_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "runtime state exceeds the 64 KiB limit",
        ));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_RUNTIME_STATE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RUNTIME_STATE_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "runtime state changed beyond the 64 KiB limit while reading",
        ));
    }
    let content = std::str::from_utf8(&bytes).map_err(|error| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("runtime state is not UTF-8: {error}"),
        )
    })?;
    let fields = parse_runtime_state(content)?;
    if fields.get("NAME").map(String::as_str) != Some(expected_name) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "runtime state NAME does not match the requested machine",
        ));
    }
    Ok(fields)
}

fn parse_runtime_state(content: &str) -> std::io::Result<HashMap<String, String>> {
    let mut fields = HashMap::new();
    for (line_number, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidData,
                format!("invalid assignment on line {}", line_number + 1),
            )
        })?;
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("invalid field name on line {}", line_number + 1),
            ));
        }
        fields.insert(key.to_string(), decode_env_value(value.trim())?);
    }
    Ok(fields)
}

fn decode_env_value(value: &str) -> std::io::Result<String> {
    let mut decoded = String::with_capacity(value.len());
    let mut chars = value.chars();
    let mut quote = None;

    while let Some(ch) = chars.next() {
        match quote {
            Some(delimiter) if ch == delimiter => quote = None,
            Some('\'') => decoded.push(ch),
            Some('"') if ch == '\\' => {
                let escaped = chars.next().ok_or_else(|| {
                    std::io::Error::new(ErrorKind::InvalidData, "trailing escape in quoted value")
                })?;
                decoded.push(match escaped {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    other => other,
                });
            }
            Some(_) => decoded.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch == '\\' => {
                decoded.push(chars.next().ok_or_else(|| {
                    std::io::Error::new(ErrorKind::InvalidData, "trailing escape in value")
                })?);
            }
            None => decoded.push(ch),
        }
    }

    if quote.is_some() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "unterminated quote in runtime state",
        ));
    }
    Ok(decoded)
}

fn insert_runtime_fields(properties: &mut MachineProperties, mut fields: HashMap<String, String>) {
    const FIELD_MAP: &[(&str, &str)] = &[
        ("NAME", "Name"),
        ("UID", "UID"),
        ("SCOPE", "Unit"),
        ("SUBGROUP", "Subgroup"),
        ("SERVICE", "Service"),
        ("ROOT", "RootDirectory"),
        ("ID", "Id"),
        ("LEADER", "Leader"),
        ("LEADER_PIDFDID", "LeaderPIDFDId"),
        ("SUPERVISOR", "Supervisor"),
        ("SUPERVISOR_PIDFDID", "SupervisorPIDFDId"),
        ("CLASS", "Class"),
        ("REALTIME", "Timestamp"),
        ("MONOTONIC", "TimestampMonotonic"),
        ("NETIF", "NetworkInterfaces"),
        ("VSOCK_CID", "VSockCID"),
        ("SSH_ADDRESS", "SSHAddress"),
        ("SSH_PRIVATE_KEY_PATH", "SSHPrivateKeyPath"),
        ("CONTROL_ADDRESS", "ControlAddress"),
    ];

    for (source, destination) in FIELD_MAP {
        if let Some(value) = fields.remove(*source).filter(|value| !value.is_empty()) {
            let value = if *source == "REALTIME" {
                value
                    .parse::<u64>()
                    .ok()
                    .map(|value| {
                        crate::nspawn::adapters::comm::formatting::format_property(
                            destination,
                            &zbus::zvariant::Value::U64(value),
                        )
                    })
                    .unwrap_or(value)
            } else {
                value
            };
            properties.insert(GROUP_MACHINE, (*destination).into(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::ContainerState;
    use std::os::unix::fs::symlink;

    fn entry(name: &str) -> ContainerEntry {
        ContainerEntry {
            name: name.into(),
            state: ContainerState::Running,
            address: Some("10.0.0.2".into()),
            all_addresses: vec!["10.0.0.2".into()],
        }
    }

    fn machine_value<'a>(properties: &'a MachineProperties, key: &str) -> Option<&'a str> {
        properties
            .groups
            .iter()
            .find(|group| group.name == GROUP_MACHINE)
            .and_then(|group| group.properties.get(key))
            .map(String::as_str)
    }

    #[tokio::test]
    async fn runtime_state_is_normalized_without_a_command_backend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-machine");
        std::fs::write(
            &path,
            "# This is private data. Do not parse.\n\
             NAME=test-machine\n\
             SCOPE=systemd-nspawn@test-machine.service\n\
             SERVICE=systemd-nspawn\n\
             ROOT=\"/var/lib/machines/test machine\"\n\
             LEADER=4242\n\
             CLASS=container\n\
             NETIF=\"2 3\"\n",
        )
        .unwrap();

        let properties = inspect_at(
            path,
            MachineName::new("test-machine").unwrap(),
            &entry("test-machine"),
        )
        .await
        .unwrap();

        assert_eq!(properties.source, InspectionSource::RuntimeState);
        assert_eq!(properties.completeness, InspectionCompleteness::RuntimeOnly);
        assert_eq!(machine_value(&properties, "State"), Some("running"));
        assert_eq!(machine_value(&properties, "Leader"), Some("4242"));
        assert_eq!(machine_value(&properties, "Class"), Some("container"));
        assert_eq!(
            machine_value(&properties, "RootDirectory"),
            Some("/var/lib/machines/test machine")
        );
        assert_eq!(machine_value(&properties, "IPAddresses"), Some("10.0.0.2"));
    }

    #[tokio::test]
    async fn runtime_enumeration_uses_regular_valid_registration_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("z-machine"), "NAME=z-machine\n").unwrap();
        std::fs::write(dir.path().join("a-machine"), "NAME=a-machine\n").unwrap();
        std::fs::write(dir.path().join("invalid:name"), "ignored\n").unwrap();
        symlink("a-machine", dir.path().join("unit:machine-a.scope")).unwrap();
        std::fs::create_dir(dir.path().join("directory-entry")).unwrap();

        let machines = list_machines_at(dir.path().to_path_buf()).await.unwrap();

        assert_eq!(
            machines
                .iter()
                .map(|machine| machine.name.as_str())
                .collect::<Vec<_>>(),
            ["a-machine", "z-machine"]
        );
        assert!(machines
            .iter()
            .all(|machine| machine.all_addresses.is_empty()));
    }

    #[tokio::test]
    async fn missing_runtime_directory_is_an_empty_machine_list() {
        let dir = tempfile::tempdir().unwrap();

        let machines = list_machines_at(dir.path().join("not-created"))
            .await
            .unwrap();

        assert!(machines.is_empty());
    }

    #[tokio::test]
    async fn missing_state_returns_snapshot_fields_and_a_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let properties = inspect_at(
            dir.path().join("missing"),
            MachineName::new("missing").unwrap(),
            &entry("missing"),
        )
        .await
        .unwrap();

        assert_eq!(machine_value(&properties, "Name"), Some("missing"));
        assert!(machine_value(&properties, "RuntimeStateRead")
            .unwrap()
            .contains("registration changed"));
    }

    #[tokio::test]
    async fn runtime_state_symlinks_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("test-machine");
        std::fs::write(&target, "NAME=test-machine\n").unwrap();
        symlink(&target, &link).unwrap();

        let properties = inspect_at(
            link,
            MachineName::new("test-machine").unwrap(),
            &entry("test-machine"),
        )
        .await
        .unwrap();

        assert!(machine_value(&properties, "RuntimeStateRead").is_some());
        assert!(machine_value(&properties, "RuntimeStateFile").is_none());
    }

    #[test]
    fn state_name_must_match_the_requested_machine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-machine");
        std::fs::write(&path, "NAME=other-machine\n").unwrap();

        let error = read_runtime_state(&path, "test-machine").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }
}

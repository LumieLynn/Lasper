//! Local, read-only access to systemd-machined runtime registration state.
//!
//! systemd exposes a small public `sd-login` API over this state directory, but
//! it does not expose the complete machine property set. This module therefore
//! supplies registration enumeration and process identity evidence only; the
//! Details pane uses the selected runtime source (D-Bus or systemd tools).

use crate::adapters::error::{NspawnError, Result};
use crate::application::sessions::{ObservedMachineInstance, ObservedNamespaceIdentity};
use crate::domain::machine::MachineName;
use crate::domain::runtime::{MachineAddressObservation, MachineEntry, MachineState};
use std::collections::HashMap;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_RUNTIME_STATE_BYTES: u64 = 64 * 1024;

/// Read the leader PID from machined's registration for a validated machine.
/// The same bounded, no-symlink parser used by systemd tools inspection protects this
/// value before it is used as a namespace target.
pub(crate) fn leader_pid(name: &MachineName) -> std::io::Result<u32> {
    leader_pid_at(
        &crate::paths::runtime_machine_state(name.as_str()),
        name.as_str(),
    )
}

pub(crate) fn leader_pid_at(path: &Path, expected_name: &str) -> std::io::Result<u32> {
    let fields = read_runtime_state(path, expected_name)?;
    parse_leader_pid(&fields)
}

/// Read the kernel identity of the machine currently registered under a name.
/// The registration supplies the leader PID; procfs supplies its start time
/// and, when permitted, namespace identities. A PID alone is not a revision.
pub(crate) fn machine_instance_at(
    path: &Path,
    expected_name: &str,
) -> std::io::Result<ObservedMachineInstance> {
    let fields = read_runtime_state(path, expected_name)?;
    let leader = parse_leader_pid(&fields)?;
    let before = observe_machine_instance(leader)?;

    // Read the registration again after procfs inspection. A replacement can
    // reuse a PID, so the process identity and the registration leader must
    // both remain stable across the observation.
    let current_fields = read_runtime_state(path, expected_name)?;
    if parse_leader_pid(&current_fields)? != leader {
        return Err(std::io::Error::new(
            ErrorKind::NotFound,
            "machine leader changed during registration inspection",
        ));
    }
    let after = observe_machine_instance(leader)?;
    if before != after {
        return Err(std::io::Error::new(
            ErrorKind::NotFound,
            "machine process identity changed during registration inspection",
        ));
    }

    Ok(before)
}

/// Namespace symlinks require ptrace access; process start time does not.
/// Always pin the observation to host procfs evidence, never to guest output.
pub(crate) fn observe_machine_instance(leader: u32) -> std::io::Result<ObservedMachineInstance> {
    observe_machine_instance_at(leader, &PathBuf::from(format!("/proc/{leader}")))
}

fn observe_machine_instance_at(
    leader: u32,
    proc: &Path,
) -> std::io::Result<ObservedMachineInstance> {
    let before = crate::adapters::process_identity::process_start_time(proc)?;
    let namespaces = match namespace_pair(proc) {
        Ok(namespaces) => Some(namespaces),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => None,
        Err(error) => return Err(error),
    };
    let after = crate::adapters::process_identity::process_start_time(proc)?;
    if before != after {
        return Err(std::io::Error::new(
            ErrorKind::NotFound,
            "machine leader changed during process inspection",
        ));
    }
    Ok(ObservedMachineInstance::from_process(
        leader, before, namespaces,
    ))
}

fn parse_leader_pid(fields: &HashMap<String, String>) -> std::io::Result<u32> {
    let value = fields.get("LEADER").ok_or_else(|| {
        std::io::Error::new(ErrorKind::InvalidData, "runtime state has no LEADER field")
    })?;
    let pid = value.parse::<u32>().map_err(|_| {
        std::io::Error::new(ErrorKind::InvalidData, "runtime state LEADER is not a PID")
    })?;
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "runtime state LEADER is outside the Linux PID range",
        ));
    }
    Ok(pid)
}

fn namespace_pair(
    proc: &Path,
) -> std::io::Result<(ObservedNamespaceIdentity, ObservedNamespaceIdentity)> {
    Ok((
        namespace_identity(&proc.join("ns/pid"))?,
        namespace_identity(&proc.join("ns/user"))?,
    ))
}

fn namespace_identity(path: &Path) -> std::io::Result<ObservedNamespaceIdentity> {
    let metadata = std::fs::metadata(path)?;
    Ok(ObservedNamespaceIdentity::new(
        metadata.dev(),
        metadata.ino(),
    ))
}

/// Enumerate runtime registrations without asking machined to inspect the
/// containers. This mirrors `sd_get_machine_names()` at the public API level:
/// names come from the runtime directory, while `unit:` helper symlinks and
/// malformed registration names are ignored. The observation grammar accepts
/// systemd's trailing-dot hostname form; the stricter `MachineName` type is
/// still enforced when an operation needs a mutable nspawn identity.
pub(crate) async fn list_machines_at(path: PathBuf) -> Result<Vec<MachineEntry>> {
    let display_path = path.clone();
    tokio::task::spawn_blocking(move || enumerate_machines(&path))
        .await
        .map_err(|error| {
            NspawnError::Runtime(format!("runtime machine enumeration task failed: {error}"))
        })?
        .map_err(|error| NspawnError::Io(display_path, error))
}

fn enumerate_machines(path: &Path) -> std::io::Result<Vec<MachineEntry>> {
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
        if validate_observed_machine_name(&name).is_err() {
            continue;
        }
        let fields = match read_runtime_state(&entry.path(), &name) {
            Ok(fields) => fields,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                log::warn!(
                    "Ignoring unreadable machined registration {}: {}",
                    name,
                    error
                );
                continue;
            }
        };
        let class = fields.get("CLASS").cloned().unwrap_or_default();
        let service = fields.get("SERVICE").cloned().unwrap_or_default();
        machines.push(MachineEntry {
            name,
            class: class.into(),
            service: service.into(),
            state: MachineState::Running,
            addresses: MachineAddressObservation::Unsupported(
                "runtime-state observation does not query addresses".into(),
            ),
        });
    }
    machines.sort();
    Ok(machines)
}

fn validate_observed_machine_name(name: &str) -> std::io::Result<()> {
    if MachineName::new(name).is_ok() {
        return Ok(());
    }

    let without_trailing_dot = name.strip_suffix('.').unwrap_or_default();
    if !without_trailing_dot.is_empty() && MachineName::new(without_trailing_dot).is_ok() {
        return Ok(());
    }

    Err(std::io::Error::new(
        ErrorKind::InvalidData,
        format!("invalid systemd machine registration name {name:?}"),
    ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn leader_pid_uses_the_validated_runtime_registration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test-machine");
        std::fs::write(&path, "NAME=test-machine\nLEADER=4242\n").unwrap();

        assert_eq!(leader_pid_at(&path, "test-machine").unwrap(), 4242);
        assert!(leader_pid_at(&path, "another-machine").is_err());

        std::fs::write(&path, "NAME=test-machine\nLEADER=0\n").unwrap();
        assert!(leader_pid_at(&path, "test-machine").is_err());
    }

    #[test]
    fn machine_instance_does_not_require_pidfd_registration_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test-machine");
        std::fs::write(
            &path,
            format!("NAME=test-machine\nLEADER={}\n", std::process::id()),
        )
        .unwrap();
        let instance = machine_instance_at(&path, "test-machine").unwrap();
        assert_eq!(instance.leader_pid(), std::process::id());
        assert!(instance.leader_start_time().is_some());
        assert_eq!(
            instance.namespaces(),
            Some(namespace_pair(Path::new("/proc/self")).unwrap())
        );
    }

    #[test]
    fn process_generation_remains_observable_without_ptrace_access_to_namespaces() {
        let proc = Path::new("/proc/1");
        let expected_namespaces = match namespace_pair(proc) {
            Ok(namespaces) => Some(namespaces),
            Err(error) if error.kind() == ErrorKind::PermissionDenied => None,
            Err(error) => panic!("unexpected namespace observation failure: {error}"),
        };
        let instance = observe_machine_instance(1).unwrap();
        assert_eq!(instance.namespaces(), expected_namespaces);
        assert_eq!(
            instance.leader_start_time(),
            Some(crate::adapters::process_identity::process_start_time(proc).unwrap()),
        );
    }

    #[test]
    fn missing_namespaces_are_not_treated_as_a_permission_limited_observation() {
        let proc = tempfile::tempdir().unwrap();
        let fields = std::iter::repeat_n("0", 18).collect::<Vec<_>>().join(" ");
        std::fs::write(
            proc.path().join("stat"),
            format!("42 (machine) S {fields} 77 0\n"),
        )
        .unwrap();
        assert_eq!(
            observe_machine_instance_at(42, proc.path())
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound,
        );
    }

    #[tokio::test]
    async fn runtime_enumeration_uses_regular_valid_registration_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("z-machine"),
            "NAME=z-machine\nCLASS=container\nSERVICE=systemd-nspawn\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a-machine"),
            "NAME=a-machine\nCLASS=container\nSERVICE=systemd-nspawn\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("foreign-vm"),
            "NAME=foreign-vm\nCLASS=vm\nSERVICE=libvirt\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("trailing-dot."),
            "NAME=trailing-dot.\nCLASS=container\nSERVICE=systemd-nspawn\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("invalid:name"), "ignored\n").unwrap();
        symlink("a-machine", dir.path().join("unit:machine-a.scope")).unwrap();
        std::fs::create_dir(dir.path().join("directory-entry")).unwrap();

        let machines = list_machines_at(dir.path().to_path_buf()).await.unwrap();

        assert_eq!(
            machines
                .iter()
                .map(|machine| machine.name.as_str())
                .collect::<Vec<_>>(),
            ["a-machine", "foreign-vm", "trailing-dot.", "z-machine"]
        );
        assert!(machines
            .iter()
            .all(|machine| matches!(machine.addresses, MachineAddressObservation::Unsupported(_))));
        let foreign = machines
            .iter()
            .find(|machine| machine.name == "foreign-vm")
            .unwrap();
        assert_eq!(foreign.class.as_str(), "vm");
        assert_eq!(foreign.service.as_str(), "libvirt");
    }

    #[tokio::test]
    async fn missing_runtime_directory_is_an_empty_machine_list() {
        let dir = tempfile::tempdir().unwrap();

        let machines = list_machines_at(dir.path().join("not-created"))
            .await
            .unwrap();

        assert!(machines.is_empty());
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

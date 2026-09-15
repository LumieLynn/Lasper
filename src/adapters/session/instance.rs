//! Host-side observation of one running machine's user namespace mapping.
//!
//! The selected runtime route resolves the leader PID. Procfs is then read
//! from the host namespace so guest-provided output cannot choose an arbitrary
//! host UID for a later desktop authorization operation.

use crate::application::sessions::{
    MappedGuestIdentity, ObservedGuestIdentity, ObservedMachineInstance, ObservedNamespaceIdentity,
    SessionError,
};
use crate::domain::machine::MachineName;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::MachineSessionTransport;

const MAX_ID_MAP_BYTES: u64 = 32 * 1024;
const MAX_ID_MAP_EXTENTS: usize = 340;
const OBSERVATION_ATTEMPTS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MachineInstanceSnapshot {
    revision: ObservedMachineInstance,
    uid_map: Vec<IdMapExtent>,
    gid_map: Vec<IdMapExtent>,
}

impl MachineInstanceSnapshot {
    pub(crate) async fn observe(
        transport: &MachineSessionTransport,
        machine: &MachineName,
    ) -> Result<Self, SessionError> {
        for _ in 0..OBSERVATION_ATTEMPTS {
            let leader = transport.machine_leader(machine).await?;
            let snapshot = tokio::task::spawn_blocking(move || read_instance(leader))
                .await
                .map_err(|error| {
                    SessionError::new(format!("machine instance inspection task failed: {error}"))
                })?
                .map_err(|error| {
                    SessionError::new(format!(
                        "inspect machine {} identity mapping: {error}",
                        machine.as_str()
                    ))
                })?;
            let current_leader = transport.machine_leader(machine).await?;
            if current_leader == leader {
                return Ok(snapshot);
            }
        }
        Err(SessionError::new(format!(
            "machine {} restarted while its identity mapping was inspected",
            machine.as_str()
        )))
    }

    pub(crate) fn mapped_identity(
        &self,
        guest: ObservedGuestIdentity,
        probe_user_namespace: ObservedNamespaceIdentity,
    ) -> Result<MappedGuestIdentity, SessionError> {
        if probe_user_namespace != self.revision.user_namespace() {
            return Err(SessionError::new(
                "guest identity probe ran in a different user namespace than the machine leader",
            ));
        }
        let host_uid = map_id(&self.uid_map, guest.uid()).ok_or_else(|| {
            SessionError::new(format!(
                "guest uid {} is not mapped into the host user namespace",
                guest.uid()
            ))
        })?;
        let host_gid = map_id(&self.gid_map, guest.gid()).ok_or_else(|| {
            SessionError::new(format!(
                "guest gid {} is not mapped into the host user namespace",
                guest.gid()
            ))
        })?;
        Ok(MappedGuestIdentity::verified(
            guest,
            host_uid,
            host_gid,
            self.revision,
        ))
    }
}

fn read_instance(leader: u32) -> std::io::Result<MachineInstanceSnapshot> {
    let proc = PathBuf::from(format!("/proc/{leader}"));
    let before = namespace_pair(&proc)?;
    let uid_map = read_id_map(&proc.join("uid_map"))?;
    let gid_map = read_id_map(&proc.join("gid_map"))?;
    let after = namespace_pair(&proc)?;
    if before != after {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "machine namespaces changed during inspection",
        ));
    }
    Ok(MachineInstanceSnapshot {
        revision: ObservedMachineInstance::new(leader, before.0, before.1),
        uid_map,
        gid_map,
    })
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IdMapExtent {
    inside: u64,
    outside: u64,
    length: u64,
}

fn read_id_map(path: &Path) -> std::io::Result<Vec<IdMapExtent>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_ID_MAP_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ID_MAP_BYTES {
        return Err(invalid_data("identity map exceeded its size limit"));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid_data("identity map is not valid UTF-8"))?;
    parse_id_map(text)
}

fn parse_id_map(text: &str) -> std::io::Result<Vec<IdMapExtent>> {
    let mut extents = Vec::new();
    for line in text.lines() {
        if extents.len() >= MAX_ID_MAP_EXTENTS {
            return Err(invalid_data("identity map has too many extents"));
        }
        let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 {
            return Err(invalid_data("identity map row does not have three fields"));
        }
        let inside = parse_map_field(fields[0])?;
        let outside = parse_map_field(fields[1])?;
        let length = parse_map_field(fields[2])?;
        if length == 0
            || inside
                .checked_add(length)
                .is_none_or(|end| end > 1_u64 << 32)
            || outside
                .checked_add(length)
                .is_none_or(|end| end > 1_u64 << 32)
        {
            return Err(invalid_data(
                "identity map row is outside the UID/GID range",
            ));
        }
        extents.push(IdMapExtent {
            inside,
            outside,
            length,
        });
    }
    if extents.is_empty() {
        return Err(invalid_data("identity map is empty"));
    }
    Ok(extents)
}

fn parse_map_field(value: &str) -> std::io::Result<u64> {
    value
        .parse()
        .map_err(|_| invalid_data("identity map contains a non-numeric field"))
}

fn map_id(extents: &[IdMapExtent], id: u32) -> Option<u32> {
    let id = u64::from(id);
    let mut mapped = None;
    for extent in extents {
        if id < extent.inside || id >= extent.inside + extent.length {
            continue;
        }
        let value = extent.outside + (id - extent.inside);
        let value = u32::try_from(value).ok()?;
        if value == u32::MAX || mapped.replace(value).is_some() {
            return None;
        }
    }
    mapped
}

fn invalid_data(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_multi_extent_guest_ids_without_assuming_one_shift() {
        let map = parse_id_map("0 1437401088 65536\n70000 200000 100\n").unwrap();
        assert_eq!(map_id(&map, 1000), Some(1_437_402_088));
        assert_eq!(map_id(&map, 70_042), Some(200_042));
        assert_eq!(map_id(&map, 65_536), None);
    }

    #[test]
    fn rejects_overflow_empty_and_reserved_host_ids() {
        assert!(parse_id_map("").is_err());
        assert!(parse_id_map("0 4294967295 2\n").is_err());
        let reserved = parse_id_map("0 4294967295 1\n").unwrap();
        assert_eq!(map_id(&reserved, 0), None);
    }

    #[test]
    fn rejects_malformed_or_unbounded_map_rows() {
        assert!(parse_id_map("0 1000\n").is_err());
        assert!(parse_id_map("0 uid 1\n").is_err());
        assert!(parse_id_map("0 1000 0\n").is_err());
        let excessive = std::iter::repeat_n("0 0 1", MAX_ID_MAP_EXTENTS + 1)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse_id_map(&excessive).is_err());
    }
}

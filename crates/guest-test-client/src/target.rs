//! Queries the host system bus for machine registrations and hosting-unit cgroups.
//! A registration is usable only if its leader pidfd matches machined's identity.

use crate::paths;
use lasper_guest_host_transport::{MachineRegistration, MachineTargetSource};
use std::{
    collections::HashMap,
    io,
    num::NonZeroU64,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::MetadataExt,
    path::PathBuf,
};
use tokio::time::{timeout, Duration};
use zbus::{
    zvariant::{OwnedObjectPath, OwnedValue},
    Connection,
};

const MACHINE: &str = "org.freedesktop.machine1";
const SYSTEMD: &str = "org.freedesktop.systemd1";
const MACHINE_IFACE: &str = "org.freedesktop.machine1.Machine";
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct HostMachines(Connection);

#[derive(Debug, PartialEq)]
struct Registration {
    path: OwnedObjectPath,
    name: String,
    unit: String,
    pid: u32,
    pidfd_id: u64,
}

impl HostMachines {
    pub async fn connect() -> io::Result<Self> {
        timeout(QUERY_TIMEOUT, Connection::system())
            .await
            .map_err(other)?
            .map(Self)
            .map_err(other)
    }

    async fn machine(&self, name: &str) -> io::Result<Registration> {
        let response = timeout(
            QUERY_TIMEOUT,
            self.0.call_method(
                Some(MACHINE),
                paths::MACHINE_MANAGER,
                Some("org.freedesktop.machine1.Manager"),
                "GetMachine",
                &(name,),
            ),
        )
        .await
        .map_err(other)?
        .map_err(other)?;
        let path: OwnedObjectPath = response.body().deserialize().map_err(other)?;
        // Direct GetAll keeps registration checks independent of proxy caches.
        let response = timeout(
            QUERY_TIMEOUT,
            self.0.call_method(
                Some(MACHINE),
                path.as_str(),
                Some("org.freedesktop.DBus.Properties"),
                "GetAll",
                &(MACHINE_IFACE,),
            ),
        )
        .await
        .map_err(other)?
        .map_err(other)?;
        let mut properties: HashMap<String, OwnedValue> =
            response.body().deserialize().map_err(other)?;
        let name = String::try_from(take(&mut properties, "Name")?).map_err(other)?;
        let unit = String::try_from(take(&mut properties, "Unit")?).map_err(other)?;
        let pid = u32::try_from(take(&mut properties, "Leader")?).map_err(other)?;
        let pidfd_id = u64::try_from(take(&mut properties, "LeaderPIDFDId")?).map_err(other)?;
        if pidfd_id == 0 {
            return Err(other(
                "machined has no pinned LeaderPIDFDId; refusing numeric-PID-only acquisition",
            ));
        }
        Ok(Registration {
            path,
            name,
            unit,
            pid,
            pidfd_id,
        })
    }
}

impl MachineTargetSource for HostMachines {
    async fn registration(&self, machine: &str) -> io::Result<MachineRegistration> {
        let before = self.machine(machine).await?;
        let start = start_time(&std::fs::read(format!(
            "{}/{}/stat",
            paths::PROC_ROOT,
            before.pid
        ))?)?;
        // SAFETY: pidfd_open takes only numeric arguments.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, before.pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pidfd_open returned a new owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        // On supported 64-bit Linux, pidfs st_ino is the 64-bit pidfd identity
        // exported by machined. Metadata on /proc/self/fd follows this pinned FD.
        let inode = std::fs::metadata(format!("{}/{}", paths::PROC_SELF_FD, fd.as_raw_fd()))?.ino();
        if inode != before.pidfd_id || self.machine(machine).await? != before {
            return Err(other(
                "machine registration changed or leader pidfd identity mismatched",
            ));
        }
        MachineRegistration::new(before.name, before.unit, before.pid, start, fd)
    }

    async fn control_group(&self, unit: &str) -> io::Result<PathBuf> {
        // ControlGroup belongs to the Service or Scope interface. machined can
        // register a container under either hosting unit type.
        let interface = if unit.ends_with(".service") {
            "org.freedesktop.systemd1.Service"
        } else if unit.ends_with(".scope") {
            "org.freedesktop.systemd1.Scope"
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported machine hosting unit: {unit}"),
            ));
        };
        let response = timeout(
            QUERY_TIMEOUT,
            self.0.call_method(
                Some(SYSTEMD),
                paths::SYSTEM_MANAGER,
                Some("org.freedesktop.systemd1.Manager"),
                "GetUnit",
                &(unit,),
            ),
        )
        .await
        .map_err(other)?
        .map_err(other)?;
        let path: OwnedObjectPath = response.body().deserialize().map_err(other)?;
        let response = timeout(
            QUERY_TIMEOUT,
            self.0.call_method(
                Some(SYSTEMD),
                path.as_str(),
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(interface, "ControlGroup"),
            ),
        )
        .await
        .map_err(other)?
        .map_err(other)?;
        let value: OwnedValue = response.body().deserialize().map_err(other)?;
        Ok(PathBuf::from(String::try_from(value).map_err(other)?))
    }
}

fn take(properties: &mut HashMap<String, OwnedValue>, name: &str) -> io::Result<OwnedValue> {
    properties
        .remove(name)
        .ok_or_else(|| other(format!("machined is missing required property {name}")))
}
fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
fn start_time(stat: &[u8]) -> io::Result<NonZeroU64> {
    // comm can contain spaces and ')' characters; the last ')' ends field 2.
    stat.iter()
        .rposition(|byte| *byte == b')')
        .and_then(|end| {
            stat[end + 1..]
                .split(u8::is_ascii_whitespace)
                .filter(|value| !value.is_empty())
                .nth(19)
        })
        .and_then(|value| std::str::from_utf8(value).ok()?.parse().ok())
        .and_then(NonZeroU64::new)
        .ok_or_else(|| other("invalid proc stat start time"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Manager;
    #[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
    impl Manager {
        fn get_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
            let path = match name {
                "fixture.scope" => "/org/freedesktop/systemd1/unit/fixture_scope",
                "fixture.service" => "/org/freedesktop/systemd1/unit/fixture_service",
                _ => return Err(zbus::fdo::Error::Failed("unexpected hosting unit".into())),
            };
            Ok(OwnedObjectPath::try_from(path).unwrap())
        }
    }
    struct Scope;
    #[zbus::interface(name = "org.freedesktop.systemd1.Scope")]
    impl Scope {
        #[zbus(property)]
        fn control_group(&self) -> &str {
            "/machine.slice/fixture.scope"
        }
    }
    struct Service;
    #[zbus::interface(name = "org.freedesktop.systemd1.Service")]
    impl Service {
        #[zbus(property)]
        fn control_group(&self) -> &str {
            "/machine.slice/fixture.service"
        }
    }

    #[tokio::test]
    async fn control_group_uses_the_hosting_units_type_specific_interface() {
        timeout(QUERY_TIMEOUT, async {
            // Exercise the actual GetUnit + Properties.Get wire requests. The
            // fixture exposes ControlGroup only on its real systemd interface;
            // querying generic Unit must fail, just as it does on a host.
            let (server, client) = tokio::net::UnixStream::pair().unwrap();
            let server = zbus::connection::Builder::unix_stream(server)
                .server(zbus::Guid::generate())
                .unwrap()
                .p2p()
                .serve_at("/org/freedesktop/systemd1", Manager)
                .unwrap()
                .serve_at("/org/freedesktop/systemd1/unit/fixture_scope", Scope)
                .unwrap()
                .serve_at("/org/freedesktop/systemd1/unit/fixture_service", Service)
                .unwrap()
                .name(SYSTEMD)
                .unwrap()
                .build();
            let client = zbus::connection::Builder::unix_stream(client).p2p().build();
            let (server, client) = tokio::try_join!(server, client).unwrap();
            let source = HostMachines(client);
            assert_eq!(
                source.control_group("fixture.scope").await.unwrap(),
                PathBuf::from("/machine.slice/fixture.scope")
            );
            assert_eq!(
                source.control_group("fixture.service").await.unwrap(),
                PathBuf::from("/machine.slice/fixture.service")
            );
            assert_eq!(
                source
                    .control_group("fixture.slice")
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
            drop(server);
        })
        .await
        .expect("host cgroup D-Bus fixture timed out");
    }

    #[test]
    fn proc_stat_comm_may_contain_parentheses_and_spaces() {
        let fields = format!("S {} 12345 0", "0 ".repeat(18));
        assert_eq!(
            start_time(format!("42 (name ) with spaces) {fields}").as_bytes())
                .unwrap()
                .get(),
            12345
        );
        let mut non_utf8 = b"42 (\xff) name) ".to_vec();
        non_utf8.extend_from_slice(fields.as_bytes());
        assert_eq!(start_time(&non_utf8).unwrap().get(), 12345);
        assert!(start_time(b"42 (broken)").is_err());
    }
    #[test]
    fn real_proc_start_time_is_nonzero() {
        assert!(start_time(&std::fs::read("/proc/self/stat").unwrap()).is_ok());
    }
}

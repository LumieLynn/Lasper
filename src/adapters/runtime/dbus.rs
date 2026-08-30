#![allow(clippy::type_complexity)]

use crate::adapters::error::{NspawnError, Result};
use crate::adapters::lifecycle::error::map_image_control_error;
use crate::adapters::runtime::source::RuntimeSource;
use crate::application::image_lifecycle::ImageControlOutcome;
use crate::domain::inspection::{
    InspectionCompleteness, InspectionSource, MachineProperties, GROUP_MACHINE,
};
use crate::domain::machine::{AllowedSignal, MachineName};
use crate::domain::runtime::{ImageEntry, ImageName, MachineEntry, MachineState, StatusUpdate};
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;
use zbus::proxy::MethodFlags;
use zbus::zvariant::{self, OwnedObjectPath};
use zbus::{proxy, Connection};

type EnableUnitFilesBody<'a> = (Vec<&'a str>, bool, bool);

const DBUS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DBUS_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const DBUS_MUTATION_TIMEOUT: Duration = Duration::from_secs(60);

fn enable_unit_files_body(unit: &str) -> EnableUnitFilesBody<'_> {
    (vec![unit], false, false)
}

#[proxy(
    interface = "org.freedesktop.machine1.Manager",
    default_service = "org.freedesktop.machine1",
    default_path = "/org/freedesktop/machine1"
)]
trait Manager {
    fn list_machines(&self) -> zbus::Result<Vec<(String, String, String, OwnedObjectPath)>>;
    fn list_images(
        &self,
    ) -> zbus::Result<Vec<(String, String, bool, u64, u64, u64, OwnedObjectPath)>>;
    fn get_machine(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    fn get_image(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    #[zbus(allow_interactive_auth)]
    fn terminate_machine(&self, name: &str) -> zbus::Result<()>;
    #[zbus(allow_interactive_auth)]
    fn kill_machine(&self, name: &str, who: &str, signal: i32) -> zbus::Result<()>;
    fn get_machine_addresses(&self, name: &str) -> zbus::Result<Vec<(i32, Vec<u8>)>>;
    #[zbus(allow_interactive_auth)]
    fn remove_image(&self, name: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn machine_new(&self, machine: String, path: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal)]
    fn machine_removed(&self, machine: String, path: OwnedObjectPath) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.machine1.Machine",
    default_service = "org.freedesktop.machine1"
)]
trait Machine {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    fn get_addresses(&self) -> zbus::Result<Vec<(i32, Vec<u8>)>>;
}

#[derive(Clone)]
pub struct DbusBackend {
    conn: std::sync::Arc<tokio::sync::Mutex<ConnectionCache<Connection>>>,
}

struct ConnectionCache<T> {
    current: Option<(u64, T)>,
    next_generation: u64,
}

impl<T: Clone> ConnectionCache<T> {
    fn lease(&self) -> Option<(u64, T)> {
        self.current
            .as_ref()
            .map(|(generation, value)| (*generation, value.clone()))
    }

    fn insert(&mut self, value: T) -> (u64, T) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        self.current = Some((generation, value.clone()));
        (generation, value)
    }

    fn invalidate(&mut self, generation: u64) {
        if self
            .current
            .as_ref()
            .is_some_and(|(current, _)| *current == generation)
        {
            self.current = None;
        }
    }
}

impl<T> Default for ConnectionCache<T> {
    fn default() -> Self {
        Self {
            current: None,
            next_generation: 0,
        }
    }
}

impl DbusBackend {
    pub fn new() -> Self {
        Self {
            conn: std::sync::Arc::new(tokio::sync::Mutex::new(ConnectionCache::default())),
        }
    }

    pub async fn connection(&self) -> Option<Connection> {
        self.connection_lease()
            .await
            .map(|(_, connection)| connection)
    }

    async fn connection_lease(&self) -> Option<(u64, Connection)> {
        if let Some(lease) = self.conn.lock().await.lease() {
            return Some(lease);
        }

        // Do not hold the cache mutex while waiting for a late or unavailable
        // system bus. A second cache check closes the parallel-connect race.
        let connection =
            match tokio::time::timeout(DBUS_CONNECT_TIMEOUT, Connection::system()).await {
                Ok(Ok(connection)) => connection,
                Ok(Err(error)) => {
                    log::debug!("System D-Bus connection unavailable: {error}");
                    return None;
                }
                Err(_) => {
                    log::debug!(
                        "System D-Bus connection timed out after {}s",
                        DBUS_CONNECT_TIMEOUT.as_secs()
                    );
                    return None;
                }
            };
        let mut cache = self.conn.lock().await;
        Some(cache.lease().unwrap_or_else(|| cache.insert(connection)))
    }

    async fn invalidate_connection(&self, generation: u64) {
        self.conn.lock().await.invalidate(generation);
    }

    async fn observe_result<T>(&self, generation: u64, result: &zbus::Result<T>) {
        if result.as_ref().is_err_and(is_connection_error) {
            self.invalidate_connection(generation).await;
        }
    }

    async fn query_with_deadline<T, F>(
        &self,
        generation: u64,
        label: &str,
        future: F,
    ) -> zbus::Result<T>
    where
        F: Future<Output = zbus::Result<T>>,
    {
        let result = match tokio::time::timeout(DBUS_QUERY_TIMEOUT, future).await {
            Ok(result) => result,
            Err(_) => {
                log::warn!(
                    "D-Bus query {label} exceeded its {}s deadline",
                    DBUS_QUERY_TIMEOUT.as_secs()
                );
                self.invalidate_connection(generation).await;
                Err(zbus::Error::Failure(format!(
                    "D-Bus query {label} timed out after {}s",
                    DBUS_QUERY_TIMEOUT.as_secs()
                )))
            }
        };
        self.observe_result(generation, &result).await;
        result
    }

    async fn mutation_with_deadline<T, F>(
        &self,
        generation: u64,
        label: &str,
        future: F,
    ) -> Result<T>
    where
        F: Future<Output = zbus::Result<T>>,
    {
        match tokio::time::timeout(DBUS_MUTATION_TIMEOUT, future).await {
            Ok(result) => {
                self.observe_result(generation, &result).await;
                result.map_err(NspawnError::Dbus)
            }
            Err(_) => {
                log::warn!(
                    "D-Bus mutation {label} exceeded its {}s deadline; its outcome is unknown",
                    DBUS_MUTATION_TIMEOUT.as_secs()
                );
                self.invalidate_connection(generation).await;
                Err(NspawnError::SystemOperationOutcomeUnknown(format!(
                    "D-Bus mutation {label} timed out after {}s; reconcile host state before retrying",
                    DBUS_MUTATION_TIMEOUT.as_secs()
                )))
            }
        }
    }

    async fn manager_proxy(&self) -> Option<(u64, ManagerProxy<'static>)> {
        let (generation, connection) = self.connection_lease().await?;
        let result = self
            .query_with_deadline(generation, "manager proxy", ManagerProxy::new(&connection))
            .await;
        result.ok().map(|proxy| (generation, proxy))
    }

    pub(crate) async fn remove_image_outcome(&self, image: &ImageName) -> ImageControlOutcome {
        let Some((generation, proxy)) = self.manager_proxy().await else {
            return ImageControlOutcome::NotAttempted {
                reason: "systemd-machined D-Bus endpoint is unavailable".into(),
            };
        };
        // RemoveImage is intentionally exempt from the short-mutation
        // deadline: machined reports completion only after its potentially
        // long-running removal helper exits. The image operation holds its
        // resource claim while this future is pending.
        let result = proxy.remove_image(image.as_str()).await;
        self.observe_result(generation, &result).await;
        match result {
            Ok(()) => ImageControlOutcome::Removed,
            Err(error) => map_image_control_error(NspawnError::Dbus(error)),
        }
    }

    /// Call a method on `org.freedesktop.systemd1.Manager` with
    /// `AllowInteractiveAuth` set, so polkit can trigger the desktop
    /// environment's authentication agent (the same path `machinectl` uses).
    async fn call_systemd1<B, R>(&self, method: &str, body: &B) -> Result<()>
    where
        B: serde::Serialize + zvariant::DynamicType,
        R: serde::de::DeserializeOwned + zvariant::Type,
    {
        let (generation, conn) = self
            .connection_lease()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let proxy_result = self
            .query_with_deadline(
                generation,
                "systemd1 manager proxy",
                zbus::proxy::Proxy::new(
                    &conn,
                    "org.freedesktop.systemd1",
                    "/org/freedesktop/systemd1",
                    "org.freedesktop.systemd1.Manager",
                ),
            )
            .await;
        let proxy = proxy_result.map_err(NspawnError::Dbus)?;
        let result = self
            .mutation_with_deadline(
                generation,
                method,
                proxy.call_with_flags(method, MethodFlags::AllowInteractiveAuth.into(), body),
            )
            .await?;
        let _: R = result.ok_or_else(|| {
            NspawnError::Dbus(zbus::Error::Failure(format!(
                "no reply from systemd1.Manager.{}",
                method
            )))
        })?;
        Ok(())
    }

    pub(crate) async fn start(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let unit = name.systemd_nspawn_unit();
        self.call_systemd1::<_, OwnedObjectPath>("StartUnit", &(&unit, "fail"))
            .await
    }

    pub(crate) async fn terminate(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        self.mutation_with_deadline(
            generation,
            "TerminateMachine",
            proxy.terminate_machine(name.as_str()),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn poweroff(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        self.mutation_with_deadline(
            generation,
            "KillMachine(poweroff)",
            proxy.kill_machine(name.as_str(), "leader", libc::SIGRTMIN() + 4),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn reboot(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        self.mutation_with_deadline(
            generation,
            "KillMachine(reboot)",
            proxy.kill_machine(name.as_str(), "leader", libc::SIGINT),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn enable(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let unit = name.systemd_nspawn_unit();
        self.call_systemd1::<_, (bool, Vec<(String, String, String)>)>(
            "EnableUnitFiles",
            &enable_unit_files_body(&unit),
        )
        .await
    }

    pub(crate) async fn disable(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let unit = name.systemd_nspawn_unit();
        let files: Vec<&str> = vec![&unit];
        self.call_systemd1::<_, Vec<(String, String, String)>>("DisableUnitFiles", &(files, false))
            .await
    }

    pub(crate) async fn kill(&self, name: &str, signal: AllowedSignal) -> Result<()> {
        let name = parse_machine_name(name)?;
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        self.mutation_with_deadline(
            generation,
            "KillMachine",
            proxy.kill_machine(name.as_str(), "all", allowed_signal_number(signal)),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn remove(&self, name: &str) -> Result<()> {
        if ImageEntry::is_protected_name(name) {
            return Err(NspawnError::ProtectedImage(name.into()));
        }
        let name = parse_image_name(name)?;
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        // See remove_image_outcome: this is an explicitly long operation and
        // must not turn normal slow removal into a fabricated timeout failure.
        let result = proxy.remove_image(name.as_str()).await;
        self.observe_result(generation, &result).await;
        result.map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub(crate) async fn reload_daemon(&self) -> Result<()> {
        self.call_systemd1::<_, ()>("Reload", &()).await
    }
}

fn allowed_signal_number(signal: AllowedSignal) -> i32 {
    match signal {
        AllowedSignal::Terminate => libc::SIGTERM,
        AllowedSignal::Kill => libc::SIGKILL,
    }
}

#[async_trait::async_trait]
impl RuntimeSource for DbusBackend {
    async fn is_available(&self) -> bool {
        self.connection().await.is_some()
    }

    async fn list_machines(&self) -> Result<Vec<MachineEntry>> {
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No DBus Connection".into())))?;
        let result = self
            .query_with_deadline(generation, "ListMachines", proxy.list_machines())
            .await;
        let machines = result.map_err(NspawnError::Dbus)?;
        let mut entries = Vec::new();
        for (name, class, service, _path) in machines {
            if name == ".host" {
                continue;
            }
            let address_result = self
                .query_with_deadline(
                    generation,
                    "GetMachineAddresses",
                    proxy.get_machine_addresses(&name),
                )
                .await;
            let addrs = match address_result {
                Ok(addresses) => addresses,
                Err(error) => {
                    log::warn!(
                        "D-Bus GetMachineAddresses failed for machine '{}'; retaining the machine without address data: {}",
                        name,
                        error
                    );
                    Vec::new()
                }
            };
            let all_addresses: Vec<String> = addrs
                .into_iter()
                .map(|(family, data)| {
                    crate::adapters::runtime::formatting::format_ip_address(family, &data)
                })
                .collect();
            entries.push(MachineEntry {
                name,
                class: class.into(),
                service: service.into(),
                state: MachineState::Running,
                address: all_addresses.first().cloned().filter(|s| !s.is_empty()),
                all_addresses,
            });
        }
        entries.sort();
        Ok(entries)
    }

    async fn list_images(&self) -> Result<Vec<ImageEntry>> {
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No DBus Connection".into())))?;
        let result = self
            .query_with_deadline(generation, "ListImages", proxy.list_images())
            .await;
        let images = result.map_err(NspawnError::Dbus)?;
        let mut images = images
            .into_iter()
            .map(
                |(name, image_type, readonly, _crtime, _mtime, usage, object_path)| ImageEntry {
                    name,
                    image_type,
                    readonly,
                    usage: (usage != u64::MAX)
                        .then(|| crate::adapters::runtime::formatting::format_size(usage)),
                    dbus_object_path: Some(object_path.to_string()),
                },
            )
            .collect::<Vec<_>>();
        images.sort();
        Ok(images)
    }

    async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        let name = parse_machine_name(name)?;
        let (generation, conn) = self
            .connection_lease()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;

        let mut props = MachineProperties::from_inspection(
            InspectionSource::Dbus,
            InspectionCompleteness::Full,
        );

        // 1) Try machine1 properties (only works for running/registered machines)
        let machine_result = self
            .query_with_deadline(
                generation,
                "machine properties",
                get_machine1_properties(&conn, &name),
            )
            .await;
        if let Ok(m1_props) = machine_result {
            let group = props.get_group_mut(GROUP_MACHINE);
            for (k, v) in m1_props {
                group.insert(k, v);
            }
        }

        // 2) Supplement with systemd1 unit properties (works even when machine isn't registered)
        let systemd_result = self
            .query_with_deadline(
                generation,
                "systemd unit properties",
                get_systemd1_properties(&conn, &name),
            )
            .await;
        if let Ok(sd_props) = systemd_result {
            for (k, v) in sd_props {
                crate::adapters::runtime::formatting::insert_systemd_property(&mut props, k, v);
            }
        }

        if props.groups.is_empty() {
            Err(NspawnError::Dbus(zbus::Error::Failure(
                "No properties found".into(),
            )))
        } else {
            Ok(props)
        }
    }

    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()> {
        use futures_util::StreamExt;
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No DBus Connection".into())))?;

        let new_result = self
            .query_with_deadline(
                generation,
                "machine-new subscription",
                proxy.receive_machine_new(),
            )
            .await;
        let mut new_stream = new_result.map_err(NspawnError::Dbus)?;
        let removed_result = self
            .query_with_deadline(
                generation,
                "machine-removed subscription",
                proxy.receive_machine_removed(),
            )
            .await;
        let mut rm_stream = removed_result.map_err(NspawnError::Dbus)?;

        loop {
            tokio::select! {
                event = new_stream.next() => {
                    if event.is_none() {
                        self.invalidate_connection(generation).await;
                        return Err(NspawnError::Dbus(zbus::Error::Failure(
                            "machine-new signal stream closed".into(),
                        )));
                    }
                    if tx.send(StatusUpdate::Dirty).await.is_err() {
                        return Ok(());
                    }
                }
                event = rm_stream.next() => {
                    if event.is_none() {
                        self.invalidate_connection(generation).await;
                        return Err(NspawnError::Dbus(zbus::Error::Failure(
                            "machine-removed signal stream closed".into(),
                        )));
                    }
                    if tx.send(StatusUpdate::Dirty).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn is_connection_error(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::InputOutput(_) | zbus::Error::Handshake(_) => true,
        zbus::Error::FDO(error) => match error.as_ref() {
            zbus::fdo::Error::ZBus(error) => is_connection_error(error),
            _ => false,
        },
        _ => false,
    }
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn parse_image_name(name: &str) -> Result<ImageName> {
    ImageName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

async fn get_machine1_properties(
    conn: &Connection,
    name: &MachineName,
) -> zbus::Result<HashMap<String, String>> {
    let proxy = ManagerProxy::new(conn).await?;
    let path = proxy.get_machine(name.as_str()).await?;
    let b = zbus::fdo::PropertiesProxy::builder(conn)
        .destination("org.freedesktop.machine1")?
        .path(path)?;
    let props_proxy = b.build().await?;
    let interface: zbus::names::InterfaceName =
        "org.freedesktop.machine1.Machine".try_into().unwrap();
    let all_props = props_proxy.get_all(Some(interface).into()).await?;
    let mut map = HashMap::new();
    for (k, v) in all_props {
        let val = crate::adapters::runtime::formatting::format_property(&k, &v.into());
        map.insert(k, val);
    }
    Ok(map)
}

async fn get_systemd1_properties(
    conn: &Connection,
    name: &MachineName,
) -> zbus::Result<HashMap<String, String>> {
    let unit = name.systemd_nspawn_unit();
    let reply = conn
        .call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "LoadUnit",
            &(&unit,),
        )
        .await?;
    let unit_path = reply
        .body()
        .deserialize::<zbus::zvariant::OwnedObjectPath>()?;
    let b = zbus::fdo::PropertiesProxy::builder(conn)
        .destination("org.freedesktop.systemd1")?
        .path(unit_path)?;
    let props_proxy = b.build().await?;

    let interface: zbus::names::InterfaceName = "org.freedesktop.systemd1.Unit".try_into().unwrap();
    let all_props = props_proxy.get_all(Some(interface).into()).await?;
    let mut map = HashMap::new();
    for (k, v) in all_props {
        let val = crate::adapters::runtime::formatting::format_property(&k, &v.into());
        map.insert(k, val);
    }

    // Also fetch Service interface properties
    let svc_interface: zbus::names::InterfaceName =
        "org.freedesktop.systemd1.Service".try_into().unwrap();
    if let Ok(svc_props) = props_proxy.get_all(Some(svc_interface).into()).await {
        for (k, v) in svc_props {
            let val = crate::adapters::runtime::formatting::format_property(&k, &v.into());
            map.insert(k, val);
        }
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_connection_failure_does_not_clear_a_new_generation() {
        let mut cache = ConnectionCache::default();
        let (stale_generation, _) = cache.insert("stale");
        let (current_generation, _) = cache.insert("current");

        cache.invalidate(stale_generation);

        assert_eq!(cache.lease(), Some((current_generation, "current")));
        cache.invalidate(current_generation);
        assert_eq!(cache.lease(), None);
    }

    #[test]
    fn backend_clones_share_one_connection_cache() {
        let backend = DbusBackend::new();
        let clone = backend.clone();

        assert!(std::sync::Arc::ptr_eq(&backend.conn, &clone.conn));
    }

    #[test]
    fn only_transport_failures_invalidate_connections() {
        let io_error = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        )));

        assert!(is_connection_error(&io_error));
        assert!(!is_connection_error(&zbus::Error::Failure(
            "service unavailable".into()
        )));
    }

    #[test]
    fn enable_unit_files_uses_systemd_manager_signature() {
        let body = enable_unit_files_body("systemd-nspawn@test.service");
        let message = zbus::Message::method("/org/freedesktop/systemd1", "EnableUnitFiles")
            .unwrap()
            .build(&body)
            .unwrap();

        assert_eq!(message.body().signature().unwrap().as_str(), "asbb");
        assert_eq!(body.0, ["systemd-nspawn@test.service"]);
        assert!(!body.1, "runtime must remain disabled");
        assert!(!body.2, "force must remain disabled");
    }

    #[test]
    fn image_names_are_not_validated_as_machine_names() {
        assert_eq!(
            parse_image_name(".oci-sha256:abc").unwrap().as_str(),
            ".oci-sha256:abc"
        );
        assert_eq!(
            parse_image_name("Ubuntu Resolute 镜像").unwrap().as_str(),
            "Ubuntu Resolute 镜像"
        );
        assert!(parse_image_name(".#temporary").is_err());
        assert!(parse_image_name("../escape").is_err());
    }
}

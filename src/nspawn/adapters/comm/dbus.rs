#![allow(clippy::type_complexity)]

use crate::nspawn::adapters::comm::backend::ContainerBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{
    ContainerEntry, ContainerState, ImageEntry, ImageName, InspectionCompleteness,
    InspectionSource, MachineName, MachineProperties, StatusUpdate,
};
use std::collections::HashMap;
use zbus::proxy::MethodFlags;
use zbus::zvariant::{self, OwnedObjectPath};
use zbus::{proxy, Connection};

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
    conn: std::sync::Arc<tokio::sync::OnceCell<Option<Connection>>>,
}

impl DbusBackend {
    pub fn new() -> Self {
        Self {
            conn: std::sync::Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    pub async fn connection(&self) -> Option<Connection> {
        let conn_opt = self
            .conn
            .get_or_init(|| async { Connection::system().await.ok() })
            .await;
        conn_opt.clone()
    }

    pub async fn manager_proxy(&self) -> Option<ManagerProxy<'static>> {
        let conn = self.connection().await?;
        ManagerProxy::new(&conn).await.ok()
    }

    /// Call a method on `org.freedesktop.systemd1.Manager` with
    /// `AllowInteractiveAuth` set, so polkit can trigger the desktop
    /// environment's authentication agent (the same path `machinectl` uses).
    async fn call_systemd1<B, R>(&self, method: &str, body: &B) -> Result<()>
    where
        B: serde::Serialize + zvariant::DynamicType,
        R: serde::de::DeserializeOwned + zvariant::Type,
    {
        let conn = self
            .connection()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let proxy = zbus::proxy::Proxy::new(
            &conn,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await
        .map_err(NspawnError::Dbus)?;
        let _: R = proxy
            .call_with_flags(method, MethodFlags::AllowInteractiveAuth.into(), body)
            .await
            .map_err(NspawnError::Dbus)?
            .ok_or_else(|| {
                NspawnError::Dbus(zbus::Error::Failure(format!(
                    "no reply from systemd1.Manager.{}",
                    method
                )))
            })?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ContainerBackend for DbusBackend {
    async fn is_available(&self) -> bool {
        self.connection().await.is_some()
    }

    async fn list_machines(&self) -> Result<Vec<ContainerEntry>> {
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No DBus Connection".into())))?;
        let machines = proxy.list_machines().await.map_err(NspawnError::Dbus)?;
        let mut entries = Vec::new();
        for (name, _class, _service, _path) in machines {
            if name == ".host" {
                continue;
            }
            let addrs = proxy.get_machine_addresses(&name).await.unwrap_or_default();
            let all_addresses: Vec<String> = addrs
                .into_iter()
                .map(|(family, data)| {
                    crate::nspawn::adapters::comm::formatting::format_ip_address(family, &data)
                })
                .collect();
            entries.push(ContainerEntry {
                name,
                state: ContainerState::Running,
                address: all_addresses.first().cloned().filter(|s| !s.is_empty()),
                all_addresses,
            });
        }
        entries.sort();
        Ok(entries)
    }

    async fn list_images(&self) -> Result<Vec<ImageEntry>> {
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No DBus Connection".into())))?;
        let images = proxy.list_images().await.map_err(NspawnError::Dbus)?;
        let mut images = images
            .into_iter()
            .map(
                |(name, image_type, readonly, _crtime, _mtime, usage, object_path)| ImageEntry {
                    name,
                    image_type,
                    readonly,
                    usage: (usage != u64::MAX)
                        .then(|| crate::nspawn::adapters::comm::formatting::format_size(usage)),
                    object_path: Some(object_path.to_string()),
                },
            )
            .collect::<Vec<_>>();
        images.sort();
        Ok(images)
    }

    async fn start(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let unit = name.systemd_nspawn_unit();
        self.call_systemd1::<_, OwnedObjectPath>("StartUnit", &(&unit, "fail"))
            .await
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        proxy
            .terminate_machine(name.as_str())
            .await
            .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let sig = libc::SIGRTMIN() + 4;
        proxy
            .kill_machine(name.as_str(), "leader", sig)
            .await
            .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        proxy
            .kill_machine(name.as_str(), "leader", libc::SIGINT)
            .await
            .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    async fn enable(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let unit = name.systemd_nspawn_unit();
        let files: Vec<(&str, bool)> = vec![(&unit, false)];
        self.call_systemd1::<_, (bool, Vec<(String, String, String)>)>(
            "EnableUnitFiles",
            &(files, false),
        )
        .await
    }

    async fn disable(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let unit = name.systemd_nspawn_unit();
        let files: Vec<&str> = vec![&unit];
        self.call_systemd1::<_, Vec<(String, String, String)>>("DisableUnitFiles", &(files, false))
            .await
    }

    async fn kill(&self, name: &str, signal: crate::nspawn::models::AllowedSignal) -> Result<()> {
        let name = parse_machine_name(name)?;
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        proxy
            .kill_machine(name.as_str(), "all", signal.as_raw())
            .await
            .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    async fn remove(&self, name: &str) -> Result<()> {
        if ImageEntry::is_protected_name(name) {
            return Err(NspawnError::Validation(
                "the .host image cannot be removed".into(),
            ));
        }
        let name = parse_image_name(name)?;
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        proxy
            .remove_image(name.as_str())
            .await
            .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        let name = parse_machine_name(name)?;
        let conn = self
            .connection()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;

        let mut props = MachineProperties::from_inspection(
            InspectionSource::Dbus,
            InspectionCompleteness::Full,
        );

        // 1) Try machine1 properties (only works for running/registered machines)
        if let Ok(m1_props) = get_machine1_properties(&conn, &name).await {
            let group = props.get_group_mut(crate::nspawn::models::GROUP_MACHINE);
            for (k, v) in m1_props {
                group.insert(k, v);
            }
        }

        // 2) Supplement with systemd1 unit properties (works even when machine isn't registered)
        if let Ok(sd_props) = get_systemd1_properties(&conn, &name).await {
            for (k, v) in sd_props {
                crate::nspawn::adapters::comm::formatting::insert_systemd_property(
                    &mut props, k, v,
                );
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

    async fn reload_daemon(&self) -> Result<()> {
        self.call_systemd1::<_, ()>("Reload", &()).await
    }

    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()> {
        use futures_util::StreamExt;
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No DBus Connection".into())))?;

        let mut new_stream = proxy
            .receive_machine_new()
            .await
            .map_err(NspawnError::Dbus)?;
        let mut rm_stream = proxy
            .receive_machine_removed()
            .await
            .map_err(NspawnError::Dbus)?;

        loop {
            tokio::select! {
                event = new_stream.next() => {
                    if event.is_none() {
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
        let val = crate::nspawn::adapters::comm::formatting::format_property(&k, &v.into());
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
        let val = crate::nspawn::adapters::comm::formatting::format_property(&k, &v.into());
        map.insert(k, val);
    }

    // Also fetch Service interface properties
    let svc_interface: zbus::names::InterfaceName =
        "org.freedesktop.systemd1.Service".try_into().unwrap();
    if let Ok(svc_props) = props_proxy.get_all(Some(svc_interface).into()).await {
        for (k, v) in svc_props {
            let val = crate::nspawn::adapters::comm::formatting::format_property(&k, &v.into());
            map.insert(k, val);
        }
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

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

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, ContainerState, MachineProperties};
use std::collections::HashMap;
use zbus::zvariant::OwnedObjectPath;
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
    fn terminate_machine(&self, name: &str) -> zbus::Result<()>;
    fn kill_machine(&self, name: &str, who: &str, signal: i32) -> zbus::Result<()>;
    fn get_machine_addresses(&self, name: &str) -> zbus::Result<Vec<(i32, Vec<u8>)>>;
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
pub struct DbusProvider {
    conn: std::sync::Arc<tokio::sync::OnceCell<Option<Connection>>>,
}

impl DbusProvider {
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

    pub async fn is_available(&self) -> bool {
        self.connection().await.is_some()
    }

    pub async fn list_all(&self) -> Result<Vec<ContainerEntry>> {
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No DBus Connection".into())))?;
        let machines = proxy.list_machines().await.map_err(NspawnError::Dbus)?;
        let images = proxy.list_images().await.map_err(NspawnError::Dbus)?;

        let mut entries = Vec::new();
        let mut running_names = HashMap::new();

        for (name, _class, _service, _path) in machines {
            if name == ".host" {
                continue;
            }
            let addrs = proxy.get_machine_addresses(&name).await.unwrap_or_default();
            let formatted: Vec<String> = addrs
                .into_iter()
                .map(|(family, data)| crate::nspawn::utils::format_ip_address(family, &data))
                .collect();
            running_names.insert(name, formatted);
        }

        for (name, img_type, readonly, _cr, _mod, usage, _path) in images {
            if name == ".host" {
                continue;
            }
            let addrs = running_names.get(&name).cloned().unwrap_or_default();
            let state = if running_names.contains_key(&name) {
                ContainerState::Running
            } else {
                ContainerState::Off
            };

            entries.push(ContainerEntry {
                name,
                state,
                image_type: Some(img_type),
                readonly,
                usage: if usage == u64::MAX {
                    None
                } else {
                    Some(crate::nspawn::utils::format_size(usage))
                },
                address: addrs.first().cloned().filter(|s: &String| !s.is_empty()),
                all_addresses: addrs,
            });
        }

        for (name, addrs) in running_names {
            if name == ".host" {
                continue;
            }
            if !entries.iter().any(|e: &ContainerEntry| e.name == name) {
                entries.push(ContainerEntry {
                    name: name.clone(),
                    state: ContainerState::Running,
                    image_type: None,
                    readonly: false,
                    usage: None,
                    address: addrs.first().cloned().filter(|s: &String| !s.is_empty()),
                    all_addresses: addrs,
                });
            }
        }

        entries.sort();

        Ok(entries)
    }

    pub async fn start(&self, name: &str) -> Result<()> {
        let conn = self
            .connection()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let unit = format!("systemd-nspawn@{}.service", name);
        conn.call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "StartUnit",
            &(&unit, "fail"),
        )
        .await
        .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub async fn terminate(&self, name: &str) -> Result<()> {
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        proxy
            .terminate_machine(name)
            .await
            .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub async fn poweroff(&self, name: &str) -> Result<()> {
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let sig = libc::SIGRTMIN() + 4;
        proxy
            .kill_machine(name, "leader", sig)
            .await
            .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub async fn reboot(&self, name: &str) -> Result<()> {
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        proxy
            .kill_machine(name, "leader", libc::SIGINT)
            .await
            .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub async fn kill(&self, name: &str, signal: &str) -> Result<()> {
        let proxy = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let sig = signal.parse::<i32>().unwrap_or(15);
        proxy
            .kill_machine(name, "all", sig)
            .await
            .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        let conn = self
            .connection()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;

        let mut props = MachineProperties::default();

        // 1) Try machine1 properties (only works for running/registered machines)
        if let Ok(m1_props) = get_machine1_properties(&conn, name).await {
            let group = props.get_group_mut("Machine");
            for (k, v) in m1_props {
                group.insert(k, v);
            }
        }

        // 2) Supplement with systemd1 unit properties (works even when machine isn't registered)
        if let Ok(sd_props) = get_systemd1_properties(&conn, name).await {
            for (k, v) in sd_props {
                if matches!(
                    k.as_str(),
                    "After"
                        | "Before"
                        | "Wants"
                        | "WantedBy"
                        | "Requires"
                        | "RequiredBy"
                        | "Conflicts"
                        | "ConflictedBy"
                ) {
                    if !v.is_empty() && v != "[]" {
                        props.insert("Dependencies", k, v);
                    }
                } else {
                    props.insert("Systemd Unit", k, v);
                }
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

    pub async fn reload_daemon(&self) -> Result<()> {
        let conn = self
            .connection()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        conn.call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "Reload",
            &(),
        )
        .await
        .map_err(NspawnError::Dbus)?;
        Ok(())
    }

    /// Block and watch for machine start/stop events. Sends a signal to tx whenever a change occurs.
    pub async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<()>) -> Result<()> {
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
                Some(_) = new_stream.next() => {
                    let _ = tx.send(()).await;
                }
                Some(_) = rm_stream.next() => {
                    let _ = tx.send(()).await;
                }
            }
        }
    }
}

async fn get_machine1_properties(
    conn: &Connection,
    name: &str,
) -> zbus::Result<HashMap<String, String>> {
    let proxy = ManagerProxy::new(conn).await?;
    let path = proxy.get_machine(name).await?;
    let b = zbus::fdo::PropertiesProxy::builder(conn)
        .destination("org.freedesktop.machine1")?
        .path(path)?;
    let props_proxy = b.build().await?;
    let interface: zbus::names::InterfaceName =
        "org.freedesktop.machine1.Machine".try_into().unwrap();
    let all_props = props_proxy.get_all(Some(interface).into()).await?;
    let mut map = HashMap::new();
    for (k, v) in all_props {
        let val = crate::nspawn::utils::format_property(&k, &v.into());
        map.insert(k, val);
    }
    Ok(map)
}

async fn get_systemd1_properties(
    conn: &Connection,
    name: &str,
) -> zbus::Result<HashMap<String, String>> {
    let unit = format!("systemd-nspawn@{}.service", name);
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
        let val = crate::nspawn::utils::format_property(&k, &v.into());
        map.insert(k, val);
    }
    Ok(map)
}

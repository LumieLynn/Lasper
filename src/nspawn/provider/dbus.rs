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
    fn list_images(&self) -> zbus::Result<Vec<(String, String, bool, u64, u64, u64, OwnedObjectPath)>>;
    fn get_machine(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    fn get_image(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    fn terminate_machine(&self, name: &str) -> zbus::Result<()>;
    fn kill_machine(&self, name: &str, who: &str, signal: i32) -> zbus::Result<()>;
    fn get_machine_addresses(&self, name: &str) -> zbus::Result<Vec<(i32, Vec<u8>)>>;
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
        let proxy = self.manager_proxy().await.ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No DBus Connection".into())))?;
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
                .map(|(family, data)| format_address(family, &data))
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
                    Some(format_size(usage))
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

        entries.sort_by(|a, b| {
            let a_run = a.state.is_running();
            let b_run = b.state.is_running();
            b_run.cmp(&a_run).then(a.name.cmp(&b.name))
        });

        Ok(entries)
    }

    pub async fn start(&self, name: &str) -> Result<()> {
        let conn = self.connection().await.ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let unit = format!("systemd-nspawn@{}.service", name);
        conn.call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "StartUnit",
            &(&unit, "fail"),
        )
        .await.map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub async fn terminate(&self, name: &str) -> Result<()> {
        let proxy = self.manager_proxy().await.ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        proxy.terminate_machine(name).await.map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub async fn poweroff(&self, name: &str) -> Result<()> {
        let proxy = self.manager_proxy().await.ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let sig = libc::SIGRTMIN() + 4;
        proxy.kill_machine(name, "leader", sig).await.map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub async fn reboot(&self, name: &str) -> Result<()> {
        let proxy = self.manager_proxy().await.ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        proxy.kill_machine(name, "leader", libc::SIGINT).await.map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub async fn kill(&self, name: &str, signal: &str) -> Result<()> {
        let proxy = self.manager_proxy().await.ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let sig = signal.parse::<i32>().unwrap_or(15);
        proxy.kill_machine(name, "all", sig).await.map_err(NspawnError::Dbus)?;
        Ok(())
    }

    pub async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        let conn = self.connection().await.ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let mut map = HashMap::new();

        // 1) Try machine1 properties (only works for running/registered machines)
        if let Ok(m1_props) = get_machine1_properties(&conn, name).await {
            map.extend(m1_props);
        }

        // 2) Supplement with systemd1 unit properties (works even when machine isn't registered)
        if let Ok(sd_props) = get_systemd1_properties(&conn, name).await {
            for (k, v) in sd_props {
                map.entry(k).or_insert(v);
            }
        }

        if map.is_empty() {
            Err(NspawnError::Dbus(zbus::Error::Failure("No properties found".into())))
        } else {
            Ok(MachineProperties {
                properties: map,
                ..Default::default()
            })
        }
    }

    pub async fn reload_daemon(&self) -> Result<()> {
        let conn = self.connection().await.ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        conn.call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "Reload",
            &(),
        )
        .await.map_err(NspawnError::Dbus)?;
        Ok(())
    }
}

async fn get_machine1_properties(conn: &Connection, name: &str) -> zbus::Result<HashMap<String, String>> {
    let proxy = ManagerProxy::new(conn).await?;
    let path = proxy.get_machine(name).await?;
    let b = zbus::fdo::PropertiesProxy::builder(conn)
        .destination("org.freedesktop.machine1")?
        .path(path)?;
    let props_proxy = b.build().await?;
    let interface: zbus::names::InterfaceName = "org.freedesktop.machine1.Machine".try_into().unwrap();
    let all_props = props_proxy.get_all(Some(interface).into()).await?;
    let mut map = HashMap::new();
    for (k, v) in all_props {
        map.insert(k, value_to_string(v.into()));
    }
    Ok(map)
}

async fn get_systemd1_properties(conn: &Connection, name: &str) -> zbus::Result<HashMap<String, String>> {
    let unit = format!("systemd-nspawn@{}.service", name);
    let reply = conn.call_method(
        Some("org.freedesktop.systemd1"),
        "/org/freedesktop/systemd1",
        Some("org.freedesktop.systemd1.Manager"),
        "LoadUnit",
        &(&unit,),
    ).await?;
    let unit_path = reply.body().deserialize::<zbus::zvariant::OwnedObjectPath>()?;
    let b = zbus::fdo::PropertiesProxy::builder(conn)
        .destination("org.freedesktop.systemd1")?
        .path(unit_path)?;
    let props_proxy = b.build().await?;
    let interface: zbus::names::InterfaceName = "org.freedesktop.systemd1.Unit".try_into().unwrap();
    let all_props = props_proxy.get_all(Some(interface).into()).await?;
    let mut map = HashMap::new();
    for (k, v) in all_props {
        map.insert(k, value_to_string(v.into()));
    }
    Ok(map)
}

fn format_address(family: i32, data: &[u8]) -> String {
    match family {
        libc::AF_INET => {
            if data.len() == 4 {
                format!("{}.{}.{}.{}", data[0], data[1], data[2], data[3])
            } else {
                String::new()
            }
        }
        libc::AF_INET6 => {
            if data.len() == 16 {
                let mut s = String::new();
                for i in 0..8 {
                    if i > 0 {
                        s.push(':');
                    }
                    s.push_str(&format!("{:x}", u16::from_be_bytes([data[i * 2], data[i * 2 + 1]])));
                }
                s
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

fn format_size(bytes: u64) -> String {
    const KI_B: u64 = 1024;
    const MI_B: u64 = KI_B * 1024;
    const GI_B: u64 = MI_B * 1024;
    const TI_B: u64 = GI_B * 1024;

    if bytes >= TI_B {
         format!("{:.1}T", bytes as f64 / TI_B as f64)
    } else if bytes >= GI_B {
         format!("{:.1}G", bytes as f64 / GI_B as f64)
    } else if bytes >= MI_B {
         format!("{:.1}M", bytes as f64 / MI_B as f64)
    } else if bytes >= KI_B {
         format!("{:.1}K", bytes as f64 / KI_B as f64)
    } else {
         format!("{}B", bytes)
    }
}

fn value_to_string(v: zbus::zvariant::Value<'_>) -> String {
    use zbus::zvariant::Value;
    match v {
        Value::Str(s) => s.as_str().to_string(),
        Value::U32(n) => n.to_string(),
        Value::U64(n) => n.to_string(),
        Value::I32(n) => n.to_string(),
        Value::I64(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::ObjectPath(p) => p.as_str().to_string(),
        Value::Signature(s) => s.as_str().to_string(),
        _ => format!("{:?}", v),
    }
}

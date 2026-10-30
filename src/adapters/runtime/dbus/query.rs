//! Read-only machine/image catalog, process identity, and property queries.

#![allow(clippy::type_complexity)]

use crate::adapters::error::{NspawnError, Result};
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::runtime::systemd_properties::{self, format_ip_address, PropertyScope};
use crate::domain::inspection::{
    InspectionCompleteness, InspectionSource, MachineProperties, GROUP_SYSTEMD_UNIT,
};
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, MachineAddressObservation, MachineEntry, MachineState};
use std::collections::HashMap;
use zbus::zvariant;
use zbus::Connection;

use super::connection::is_connection_error;
use super::protocol::{MachineProxy, ManagerProxy};
use super::{parse_machine_name, DbusBackend};

impl DbusBackend {
    pub(crate) async fn machine_leader(&self, name: &MachineName) -> Result<u32> {
        let (generation, connection) = self
            .connection_lease()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let leader = self
            .query_with_deadline(
                generation,
                "machine leader",
                get_machine_leader(&connection, name),
            )
            .await
            .map_err(NspawnError::Dbus)?;
        if leader == 0 || leader > i32::MAX as u32 {
            return Err(NspawnError::Runtime(format!(
                "machine {} reported an invalid leader PID {leader}",
                name.as_str()
            )));
        }
        Ok(leader)
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
        for (name, class, service, path) in machines {
            if name == ".host" {
                continue;
            }
            let state_result = self
                .query_with_deadline(generation, "machine State", async {
                    let machine = MachineProxy::builder(proxy.inner().connection())
                        .path(path)?
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build()
                        .await?;
                    machine.state().await
                })
                .await;
            let state = match state_result {
                Ok(state) => MachineState::from_systemd(state),
                // A registration can disappear between ListMachines and Get.
                Err(error) if is_missing_machine(&error) => continue,
                Err(error) if is_connection_error(&error) => {
                    return Err(NspawnError::Dbus(error));
                }
                Err(error) => {
                    log::warn!("D-Bus State is unavailable for machine '{name}': {error}");
                    MachineState::Unknown("unavailable".into())
                }
            };
            let class = crate::domain::runtime::MachineClass::from(class);
            let service = crate::domain::runtime::MachineProvider::from(service);
            let addresses = if class.is_container() {
                let address_result = self
                    .query_with_deadline(
                        generation,
                        "GetMachineAddresses",
                        proxy.get_machine_addresses(&name),
                    )
                    .await;
                match address_result {
                    Ok(addrs) => MachineAddressObservation::available(
                        addrs
                            .into_iter()
                            .map(|(family, data)| format_ip_address(family, &data)),
                    ),
                    Err(error) => {
                        let observation = classify_address_error(error);
                        log::warn!(
                            "D-Bus GetMachineAddresses did not provide data for machine '{}': {}",
                            name,
                            observation.diagnostic_value()
                        );
                        observation
                    }
                }
            } else {
                MachineAddressObservation::Unsupported(format!(
                    "address observation is not available for machine class {class}"
                ))
            };
            entries.push(MachineEntry {
                name,
                class,
                service,
                state,
                addresses,
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
                    usage: (usage != u64::MAX).then(|| ImageEntry::format_usage(usage)),
                    dbus_object_path: Some(object_path.to_string()),
                },
            )
            .collect::<Vec<_>>();
        images.sort();
        Ok(images)
    }

    async fn get_properties(
        &self,
        name: &str,
        include_nspawn_unit: bool,
    ) -> Result<MachineProperties> {
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
            systemd_properties::append_dbus(&mut props, PropertyScope::Machine, m1_props);
        }

        // Only an exact nspawn identity may be associated with the generated
        // systemd-nspawn@ unit. Foreign machined registrations can share the
        // same name without referring to that unit.
        if include_nspawn_unit {
            let systemd_result = self
                .query_with_deadline(
                    generation,
                    "systemd unit properties",
                    get_systemd1_properties(&conn, &name),
                )
                .await;
            if let Ok(sd_props) = systemd_result {
                for (scope, values) in sd_props {
                    systemd_properties::append_dbus(&mut props, scope, values);
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

    async fn get_unit_properties(&self, machine: &MachineName) -> Result<MachineProperties> {
        let (generation, conn) = self
            .connection_lease()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let values = self
            .query_with_deadline(
                generation,
                "image unit properties",
                get_systemd1_properties(&conn, machine),
            )
            .await
            .map_err(NspawnError::Dbus)?;
        let mut properties = MachineProperties::from_inspection(
            InspectionSource::Dbus,
            InspectionCompleteness::Full,
        );
        for (scope, values) in values {
            systemd_properties::append_dbus(&mut properties, scope, values);
        }
        if properties.get_display(GROUP_SYSTEMD_UNIT, "LoadState") == Some("not-found") {
            return Err(NspawnError::Dbus(zbus::Error::Failure(format!(
                "Unit {} was not found (LoadState=not-found)",
                machine.systemd_nspawn_unit()
            ))));
        }
        Ok(properties)
    }
}

const NO_PRIVATE_NETWORKING_ERROR: &str = "org.freedesktop.machine1.NoPrivateNetworking";
const NOT_SUPPORTED_ERROR: &str = "org.freedesktop.DBus.Error.NotSupported";

fn classify_address_error(error: zbus::Error) -> MachineAddressObservation {
    match error {
        zbus::Error::MethodError(name, detail, _)
            if name.as_str() == NO_PRIVATE_NETWORKING_ERROR =>
        {
            MachineAddressObservation::NotPrivate(
                detail.unwrap_or_else(|| "machine does not use private networking".into()),
            )
        }
        zbus::Error::MethodError(name, detail, _) if name.as_str() == NOT_SUPPORTED_ERROR => {
            MachineAddressObservation::Unsupported(detail.unwrap_or_else(|| {
                "address observation is supported only for container machines".into()
            }))
        }
        zbus::Error::Unsupported => MachineAddressObservation::Unsupported(
            "the D-Bus client does not support address observation".into(),
        ),
        error => MachineAddressObservation::Unavailable(error.to_string()),
    }
}

fn is_missing_machine(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.UnknownObject" | "org.freedesktop.machine1.NoSuchMachine"
        ),
        zbus::Error::FDO(error) => matches!(error.as_ref(), zbus::fdo::Error::UnknownObject(_)),
        _ => false,
    }
}

async fn get_machine1_properties(
    conn: &Connection,
    name: &MachineName,
) -> zbus::Result<HashMap<String, zvariant::OwnedValue>> {
    let proxy = ManagerProxy::new(conn).await?;
    let path = proxy.get_machine(name.as_str()).await?;
    let b = zbus::fdo::PropertiesProxy::builder(conn)
        .destination("org.freedesktop.machine1")?
        .path(path)?;
    let props_proxy = b.build().await?;
    let interface: zbus::names::InterfaceName =
        "org.freedesktop.machine1.Machine".try_into().unwrap();
    Ok(props_proxy.get_all(interface).await?)
}

async fn get_machine_leader(conn: &Connection, name: &MachineName) -> zbus::Result<u32> {
    let manager = ManagerProxy::new(conn).await?;
    let path = manager.get_machine(name.as_str()).await?;
    let machine = MachineProxy::builder(conn).path(path)?.build().await?;
    machine.leader().await
}

async fn get_systemd1_properties(
    conn: &Connection,
    name: &MachineName,
) -> zbus::Result<Vec<(PropertyScope, HashMap<String, zvariant::OwnedValue>)>> {
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
    let proxy = zbus::fdo::PropertiesProxy::builder(conn)
        .destination("org.freedesktop.systemd1")?
        .path(unit_path)?
        .build()
        .await?;
    // Service fallback vtables (exec, cgroup, kill) are registered under the
    // Service interface. Fetching the two interfaces separately preserves
    // their origin without losing any of those properties.
    let unit_interface = PropertyScope::Unit.interface().try_into()?;
    let service_interface = PropertyScope::Service.interface().try_into()?;
    let (unit_values, service_values) = tokio::join!(
        proxy.get_all(unit_interface),
        proxy.get_all(service_interface),
    );
    let mut groups = vec![(PropertyScope::Unit, unit_values?)];
    match service_values {
        Ok(values) => groups.push((PropertyScope::Service, values)),
        // An unloaded/nonexistent service may expose only its base Unit.
        Err(zbus::fdo::Error::UnknownInterface(_)) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedObjectPath;
    fn method_error(name: &str, detail: &str) -> zbus::Error {
        let error_name = zbus::names::ErrorName::try_from(name)
            .expect("test error name")
            .to_owned()
            .into();
        let message = zbus::Message::method_call("/test", "Failure")
            .expect("test method message")
            .build(&())
            .expect("test message body");
        zbus::Error::MethodError(error_name, Some(detail.into()), message)
    }

    #[test]
    fn address_observation_classifies_provider_limits_without_text_matching() {
        let no_private = classify_address_error(method_error(
            NO_PRIVATE_NETWORKING_ERROR,
            "machine does not use private networking",
        ));
        assert!(matches!(
            no_private,
            MachineAddressObservation::NotPrivate(reason)
                if reason == "machine does not use private networking"
        ));

        let virtual_machine = classify_address_error(method_error(
            NOT_SUPPORTED_ERROR,
            "address data is only supported on container machines",
        ));
        assert!(matches!(
            virtual_machine,
            MachineAddressObservation::Unsupported(reason)
                if reason == "address data is only supported on container machines"
        ));

        let failure = classify_address_error(zbus::Error::Failure("temporary failure".into()));
        assert_eq!(
            failure,
            MachineAddressObservation::Unavailable("temporary failure".into())
        );
    }

    mod machine_snapshots {
        use super::*;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, BufReader};

        struct Registry;

        #[zbus::interface(name = "org.freedesktop.machine1.Manager")]
        impl Registry {
            fn list_machines(&self) -> Vec<(String, String, String, OwnedObjectPath)> {
                ["tracked", "gone", "unreadable"]
                    .into_iter()
                    .map(|name| {
                        (
                            name.into(),
                            "container".into(),
                            "systemd-nspawn".into(),
                            OwnedObjectPath::try_from(format!(
                                "/org/freedesktop/machine1/machine/{name}"
                            ))
                            .unwrap(),
                        )
                    })
                    .collect()
            }

            fn get_machine_addresses(&self, _name: &str) -> Vec<(i32, Vec<u8>)> {
                Vec::new()
            }
        }

        struct Registration {
            state: Arc<parking_lot::Mutex<String>>,
            readable: bool,
        }

        #[zbus::interface(name = "org.freedesktop.machine1.Machine")]
        impl Registration {
            #[zbus(property)]
            fn state(&self) -> zbus::fdo::Result<String> {
                if self.readable {
                    Ok(self.state.lock().clone())
                } else {
                    Err(zbus::fdo::Error::AccessDenied("test denies State".into()))
                }
            }
        }

        #[tokio::test]
        #[ignore = "requires dbus-daemon; uses an isolated session bus"]
        async fn list_reads_live_states_and_handles_disappearing_or_unreadable_registrations() {
            let mut bus = tokio::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut address = String::new();
            tokio::time::timeout(
                Duration::from_secs(5),
                BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address),
            )
            .await
            .unwrap()
            .unwrap();

            let state = Arc::new(parking_lot::Mutex::new("opening".into()));
            let _server = zbus::connection::Builder::address(address.trim())
                .unwrap()
                .name("org.freedesktop.machine1")
                .unwrap()
                .serve_at("/org/freedesktop/machine1", Registry)
                .unwrap()
                .serve_at(
                    "/org/freedesktop/machine1/machine/tracked",
                    Registration {
                        state: state.clone(),
                        readable: true,
                    },
                )
                .unwrap()
                .serve_at(
                    "/org/freedesktop/machine1/machine/unreadable",
                    Registration {
                        state: state.clone(),
                        readable: false,
                    },
                )
                .unwrap()
                .build()
                .await
                .unwrap();
            let connection = zbus::connection::Builder::address(address.trim())
                .unwrap()
                .build()
                .await
                .unwrap();
            let backend = DbusBackend::new();
            let _ = backend.conn.lock().await.insert(connection);

            for (reported, expected) in [
                ("opening", MachineState::Starting),
                ("running", MachineState::Running),
                ("closing", MachineState::Exiting),
                ("future-state", MachineState::Unknown("future-state".into())),
            ] {
                *state.lock() = reported.into();
                let machines = RuntimeSource::list_machines(&backend).await.unwrap();
                assert_eq!(machines.len(), 2, "a vanished object must leave the list");
                assert_eq!(machines[0].name, "tracked");
                assert_eq!(machines[0].state, expected);
                assert_eq!(machines[1].name, "unreadable");
                assert_eq!(
                    machines[1].state,
                    MachineState::Unknown("unavailable".into())
                );
            }
            bus.kill().await.unwrap();
            bus.wait().await.unwrap();
        }
    }
}

//! Native user-manager client. The path and its service are one transient job.

use std::collections::HashMap;

use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::{
    ActivationSpec, UnitManager, UnitState, COMMAND_TIMEOUT, PATH_DESCRIPTION, PATH_UNIT,
    SERVICE_UNIT,
};

const DESTINATION: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";

pub(super) struct UserManager {
    connection: zbus::Connection,
}

impl UserManager {
    pub(super) async fn connect() -> Result<Self, String> {
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            let connection = zbus::Connection::session().await?;
            let manager = Self { connection };
            manager
                .proxy()
                .await?
                .call::<_, _, ()>("Subscribe", &())
                .await?;
            Ok::<_, zbus::Error>(manager)
        })
        .await
        .map_err(|_| "connect to the user systemd manager timed out".to_owned())?
        .map_err(|error| format!("connect to the user systemd manager: {error}"))
    }

    async fn proxy(&self) -> zbus::Result<zbus::Proxy<'_>> {
        zbus::Proxy::new(
            &self.connection,
            DESTINATION,
            MANAGER_PATH,
            MANAGER_INTERFACE,
        )
        .await
    }

    async fn job<B>(&self, method: &str, body: &B) -> Result<(), String>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType + Sync,
    {
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            let proxy = self.proxy().await.map_err(|error| error.to_string())?;
            // Listen before submitting the job: even an immediately completed
            // path activation must have its result checked.
            let mut jobs = proxy
                .receive_signal("JobRemoved")
                .await
                .map_err(|error| error.to_string())?;
            let job: OwnedObjectPath = proxy
                .call(method, body)
                .await
                .map_err(|error| error.to_string())?;
            while let Some(message) = jobs.next().await {
                let (_, path, unit, result): (u32, OwnedObjectPath, String, String) = message
                    .body()
                    .deserialize()
                    .map_err(|error| error.to_string())?;
                if path == job {
                    return if result == "done" {
                        Ok(())
                    } else {
                        Err(format!("{unit} job finished with {result}"))
                    };
                }
            }
            Err("user manager closed the job result stream".into())
        })
        .await
        .map_err(|_| format!("user manager {method} timed out; the job outcome is unknown"))?
        .map_err(|error| format!("user manager {method}: {error}"))
    }

    async fn call(&self, method: &str, unit: Option<&str>) -> Result<(), String> {
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            let proxy = self.proxy().await?;
            match unit {
                Some(unit) => proxy.call::<_, _, ()>(method, &(unit,)).await,
                None => proxy.call::<_, _, ()>(method, &()).await,
            }
        })
        .await
        .map_err(|_| format!("user manager {method} timed out"))?
        .map_err(|error| format!("user manager {method}: {error}"))
    }
}

#[async_trait::async_trait]
impl UnitManager for UserManager {
    async fn inspect(&self, unit: &str) -> Result<Option<UnitState>, String> {
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            let proxy = self.proxy().await?;
            let path: OwnedObjectPath = match proxy.call("GetUnit", &(unit,)).await {
                Ok(path) => path,
                Err(zbus::Error::MethodError(name, _, _))
                    if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
                {
                    return Ok(None)
                }
                Err(error) => return Err(error),
            };
            let reply = self
                .connection
                .call_method(
                    Some(DESTINATION),
                    path.as_str(),
                    Some("org.freedesktop.DBus.Properties"),
                    "GetAll",
                    &("org.freedesktop.systemd1.Unit",),
                )
                .await?;
            let mut properties: HashMap<String, OwnedValue> = reply.body().deserialize()?;
            let load_state: String = take_property(&mut properties, "LoadState")?;
            if load_state == "not-found" {
                return Ok(None);
            }
            Ok(Some(UnitState {
                transient: take_property(&mut properties, "Transient")?,
                description: take_property(&mut properties, "Description")?,
                active_state: take_property(&mut properties, "ActiveState")?,
            }))
        })
        .await
        .map_err(|_| format!("inspect user unit {unit} timed out"))?
        .map_err(|error: zbus::Error| format!("inspect user unit {unit}: {error}"))
    }

    async fn create(&self, spec: &ActivationSpec, include_service: bool) -> Result<(), String> {
        let description = spec.service_description();
        let directory = spec
            .watched_directory
            .to_str()
            .ok_or_else(|| "X11 machine registration path is not valid UTF-8".to_owned())?;
        // ExecStart is a(sasb); the command is an argv, not a unit-file line.
        let argv = vec![
            spec.executable.replace('$', "$$"),
            "--internal-x11-reconcile".to_owned(),
        ];
        let service = vec![
            ("Description", Value::from(description.as_str())),
            ("Type", Value::from("oneshot")),
            ("UMask", Value::from(0o077u32)),
            ("CollectMode", Value::from("inactive-or-failed")),
            (
                "ExecStart",
                Value::from(vec![(spec.executable.as_str(), argv, false)]),
            ),
        ];
        let path = vec![
            ("Description", Value::from(PATH_DESCRIPTION)),
            ("CollectMode", Value::from("inactive-or-failed")),
            ("Paths", Value::from(vec![("PathChanged", directory)])),
        ];
        let auxiliary = if include_service {
            vec![(SERVICE_UNIT, service)]
        } else {
            Vec::new()
        };
        self.job("StartTransientUnit", &(PATH_UNIT, "fail", path, auxiliary))
            .await
    }

    async fn start(&self, unit: &str) -> Result<(), String> {
        self.job("StartUnit", &(unit, "replace")).await
    }

    async fn stop(&self, unit: &str) -> Result<(), String> {
        self.job("StopUnit", &(unit, "replace")).await
    }

    async fn reset_failed(&self, unit: &str) -> Result<(), String> {
        self.call("ResetFailedUnit", Some(unit)).await
    }

    async fn reload(&self) -> Result<(), String> {
        self.call("Reload", None).await
    }
}

fn take_property<T>(properties: &mut HashMap<String, OwnedValue>, name: &str) -> zbus::Result<T>
where
    T: TryFrom<OwnedValue, Error = zbus::zvariant::Error>,
{
    let value = properties
        .remove(name)
        .ok_or_else(|| zbus::Error::Failure(format!("unit property {name} is missing")))?;
    T::try_from(value).map_err(Into::into)
}

#[cfg(test)]
#[path = "tests/dbus.rs"]
mod tests;

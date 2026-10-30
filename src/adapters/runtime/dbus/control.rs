//! Machine/unit mutations and image removal over the system bus.

use crate::adapters::error::{NspawnError, Result};
use crate::adapters::lifecycle::error::map_image_control_error;
use crate::application::image_lifecycle::ImageControlOutcome;
use crate::domain::machine::AllowedSignal;
use crate::domain::runtime::{ImageEntry, ImageName};
use zbus::proxy::MethodFlags;
use zbus::zvariant::{self, OwnedObjectPath};

use super::{parse_machine_name, DbusBackend};

type EnableUnitFilesBody<'a> = (Vec<&'a str>, bool, bool);
type DisableUnitFilesBody<'a> = (Vec<&'a str>, bool);

fn enable_unit_files_body(unit: &str) -> EnableUnitFilesBody<'_> {
    (vec!["machines.target", unit], false, false)
}

fn disable_unit_files_body(unit: &str) -> DisableUnitFilesBody<'_> {
    (vec![unit], false)
}

impl DbusBackend {
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
        .await?;
        // machinectl reloads systemd after changing the unit-file graph. Keep
        // the D-Bus route behaviorally aligned with that native command.
        self.reload_daemon().await
    }

    pub(crate) async fn disable(&self, name: &str) -> Result<()> {
        let name = parse_machine_name(name)?;
        let unit = name.systemd_nspawn_unit();
        self.call_systemd1::<_, Vec<(String, String, String)>>(
            "DisableUnitFiles",
            &disable_unit_files_body(&unit),
        )
        .await?;
        self.reload_daemon().await
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

fn parse_image_name(name: &str) -> Result<ImageName> {
    ImageName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_unit_files_uses_systemd_manager_signature() {
        let body = enable_unit_files_body("systemd-nspawn@test.service");
        let message = zbus::Message::method_call("/org/freedesktop/systemd1", "EnableUnitFiles")
            .unwrap()
            .build(&body)
            .unwrap();

        assert_eq!(message.body().signature().to_string_no_parens(), "asbb");
        assert_eq!(body.0, ["machines.target", "systemd-nspawn@test.service"]);
        assert!(!body.1, "runtime must remain disabled");
        assert!(!body.2, "force must remain disabled");
    }

    #[test]
    fn disable_unit_files_uses_systemd_manager_signature() {
        let body = disable_unit_files_body("systemd-nspawn@test.service");
        let message = zbus::Message::method_call("/org/freedesktop/systemd1", "DisableUnitFiles")
            .unwrap()
            .build(&body)
            .unwrap();

        assert_eq!(message.body().signature().to_string_no_parens(), "asb");
        assert_eq!(body.0, ["systemd-nspawn@test.service"]);
        assert!(!body.1, "runtime must remain disabled");
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

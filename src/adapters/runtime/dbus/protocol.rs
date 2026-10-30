//! Native machine1 method, property, and signal signatures.

#![allow(clippy::type_complexity)]

use zbus::proxy;
use zbus::zvariant::{self, OwnedObjectPath};

#[proxy(
    interface = "org.freedesktop.machine1.Manager",
    default_service = "org.freedesktop.machine1",
    default_path = "/org/freedesktop/machine1"
)]
pub(super) trait Manager {
    fn list_machines(&self) -> zbus::Result<Vec<(String, String, String, OwnedObjectPath)>>;
    fn list_images(
        &self,
    ) -> zbus::Result<Vec<(String, String, bool, u64, u64, u64, OwnedObjectPath)>>;
    fn get_machine(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    fn get_image(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    #[zbus(allow_interactive_auth)]
    fn open_machine_shell(
        &self,
        name: &str,
        user: &str,
        path: &str,
        args: Vec<String>,
        environment: Vec<String>,
    ) -> zbus::Result<(zvariant::OwnedFd, String)>;
    #[zbus(allow_interactive_auth)]
    fn open_machine_login(&self, name: &str) -> zbus::Result<(zvariant::OwnedFd, String)>;
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
pub(super) trait Machine {
    #[zbus(property)]
    fn leader(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    fn get_addresses(&self) -> zbus::Result<Vec<(i32, Vec<u8>)>>;
}

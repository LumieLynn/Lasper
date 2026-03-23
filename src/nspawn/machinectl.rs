//! High-level wrapper around `machinectl` and `journalctl` commands.

use super::errors::{NspawnError, Result};
use super::manager::NspawnManager;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::process::Command;
use zbus::{proxy, Connection};
use zbus::zvariant::OwnedObjectPath;

// ── DBus Proxies ─────────────────────────────────────────────────────────────

#[proxy(
    interface = "org.freedesktop.machine1.Manager",
    default_service = "org.freedesktop.machine1",
    default_path = "/org/freedesktop/machine1"
)]
trait Manager {
    /// Returns a list of machines. Signature: a(ssssso)
    /// (name, class, service, root-directory, object-path)
    fn list_machines(&self) -> zbus::Result<Vec<(String, String, String, String, OwnedObjectPath)>>;

    /// Returns a list of images. Signature: a(ssbttto)
    /// (name, type, read-only, creation-time, modification-time, usage, object-path)
    fn list_images(&self) -> zbus::Result<Vec<(String, String, bool, u64, u64, u64, OwnedObjectPath)>>;

    fn get_machine(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    fn get_image(&self, name: &str) -> zbus::Result<OwnedObjectPath>;

    fn start_machine(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    fn stop_machine(&self, name: &str, mode: &str) -> zbus::Result<()>;
    fn terminate_machine(&self, name: &str) -> zbus::Result<()>;
    fn reboot_machine(&self, name: &str) -> zbus::Result<()>;
    fn power_off_machine(&self, name: &str) -> zbus::Result<()>;
    fn kill_machine(&self, name: &str, who: &str, signal: i32) -> zbus::Result<()>;

    /// Returns IP addresses for a machine. Signature: a(iay)
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
    #[zbus(property)]
    fn addresses(&self) -> zbus::Result<Vec<(i32, Vec<u8>)>>;
}

// ── Unified data model ────────────────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum ContainerState {
    Running,
    Off,
    Starting,
    Exiting,
}

impl ContainerState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Off => "poweroff",
            Self::Starting => "starting",
            Self::Exiting => "exiting",
        }
    }
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running | Self::Starting | Self::Exiting)
    }
}

/// A container known to machinectl — either running, poweroff, or both.
#[derive(Debug, Clone)]
pub struct ContainerEntry {
    /// The name used by machinectl
    pub name: String,
    /// Current lifecycle state
    pub state: ContainerState,
    /// Image type ("directory", "raw", "tar", …) — from list-images, None if only seen running
    pub image_type: Option<String>,
    /// Whether the image is read-only (from list-images)
    pub readonly: bool,
    /// Disk usage string (from list-images)
    pub usage: Option<String>,
    /// Network address (from list, only when running)
    pub address: Option<String>,
    /// All network addresses
    pub all_addresses: Vec<String>,
}

// ── SystemdManager ────────────────────────────────────────────────────────────

pub struct SystemdManager {
    is_root: bool,
    conn: std::sync::Arc<tokio::sync::OnceCell<Option<Connection>>>,
}

impl SystemdManager {
    pub fn new(is_root: bool) -> Self {
        Self {
            is_root,
            conn: std::sync::Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    async fn connection(&self) -> Option<&Connection> {
        let conn_opt = self.conn
            .get_or_init(|| async {
                Connection::system().await.ok()
            })
            .await;
        conn_opt.as_ref()
    }

    async fn manager_proxy(&self) -> Option<ManagerProxy<'static>> {
        let conn = self.connection().await?;
        ManagerProxy::new(conn).await.ok()
    }

    fn require_root(&self) -> Result<()> {
        if !self.is_root {
            Err(NspawnError::PermissionDenied)
        } else {
            Ok(())
        }
    }

    async fn run_machinectl(&self, args: &[&str]) -> Result<()> {
        let out = Command::new("machinectl")
            .args(args)
            .output()
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        if !out.status.success() {
            return Err(NspawnError::CommandFailed(
                format!("machinectl {:?}", args),
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(())
    }

    async fn list_all_dbus(&self, proxy: &ManagerProxy<'static>) -> Result<Vec<ContainerEntry>> {
        let machines = proxy.list_machines().await.map_err(NspawnError::Dbus)?;
        let images = proxy.list_images().await.map_err(NspawnError::Dbus)?;

        let mut entries = Vec::new();
        let mut running_names = HashMap::new();

        for (name, _class, _service, _root, _path) in machines {
            let addrs = proxy.get_machine_addresses(&name).await.unwrap_or_default();
            let formatted: Vec<String> = addrs
                .into_iter()
                .map(|(family, data)| format_address(family, &data))
                .collect();
            running_names.insert(name, formatted);
        }

        for (name, img_type, readonly, _cr, _mod, usage, _path) in images {
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

        // Handle machines without images (e.g. transient)
        for (name, addrs) in running_names {
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

    async fn list_all_cmd(&self) -> Result<Vec<ContainerEntry>> {
        let running = self.running_map().await?;

        if !self.is_root {
            return Ok(running
                .into_iter()
                .map(|(name, addrs)| ContainerEntry {
                    state: ContainerState::Running,
                    name,
                    image_type: None,
                    readonly: false,
                    usage: None,
                    address: addrs.first().cloned().filter(|s| !s.is_empty()),
                    all_addresses: addrs,
                })
                .collect());
        }

        let out = Command::new("machinectl")
            .args(["list-images", "-l", "--no-legend", "--no-pager"])
            .output()
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        if !out.status.success() {
            return Err(NspawnError::CommandFailed(
                "machinectl list-images".into(),
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }

        let mut entries: Vec<ContainerEntry> = Vec::new();

        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                continue;
            }
            let name = parts[0].to_string();
            let addrs = running.get(&name).cloned().unwrap_or_default();
            let addr = addrs.first().cloned();
            let state = if running.contains_key(&name) {
                ContainerState::Running
            } else {
                ContainerState::Off
            };

            entries.push(ContainerEntry {
                state,
                name,
                image_type: Some(parts[1].to_string()),
                readonly: parts[2] == "yes",
                usage: parts.get(3).map(|s| s.to_string()),
                address: addr.filter(|s| !s.is_empty()),
                all_addresses: addrs,
            });
        }

        for (name, addrs) in &running {
            if !entries.iter().any(|e| &e.name == name) {
                entries.push(ContainerEntry {
                    name: name.clone(),
                    state: ContainerState::Running,
                    image_type: None,
                    readonly: false,
                    usage: None,
                    address: addrs.first().cloned().filter(|s| !s.is_empty()),
                    all_addresses: addrs.clone(),
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

    async fn running_map(&self) -> Result<HashMap<String, Vec<String>>> {
        let out = Command::new("machinectl")
            .args(["list", "-l", "--no-legend", "--no-pager"])
            .output()
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.is_empty() && !stderr.contains("No machines") {
                return Err(NspawnError::CommandFailed(
                    "machinectl list".into(),
                    stderr.trim().to_string(),
                ));
            }
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        let mut current_machine = String::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with(|c: char| c.is_whitespace()) {
                let ip = line.trim();
                if !current_machine.is_empty() && !ip.is_empty() {
                    if let Some(ips) = map.get_mut(&current_machine) {
                        ips.push(ip.to_string());
                    }
                }
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                continue;
            }
            current_machine = parts[0].to_string();
            let mut ips = Vec::new();
            if let Some(addr) = parts.get(5).copied() {
                if !addr.is_empty() && addr != "-" {
                    ips.push(addr.to_string());
                }
            }
            map.insert(current_machine.clone(), ips);
        }
        Ok(map)
    }
}

#[async_trait]
impl NspawnManager for SystemdManager {
    async fn list_all(&self) -> Result<Vec<ContainerEntry>> {
        if !self.is_root {
            return self.list_all_cmd().await;
        }

        // Try DBus first
        if let Some(proxy) = self.manager_proxy().await {
            match self.list_all_dbus(&proxy).await {
                Ok(entries) => return Ok(entries),
                Err(e) => log::warn!("DBus list_all failed, falling back to command: {}", e),
            }
        }

        self.list_all_cmd().await
    }

    async fn start(&self, name: &str) -> Result<()> {
        self.require_root()?;
        if let Some(proxy) = self.manager_proxy().await {
            if let Ok(_) = proxy.start_machine(name).await {
                return Ok(());
            }
        }
        self.run_machinectl(&["start", name]).await
    }


    async fn terminate(&self, name: &str) -> Result<()> {
        self.require_root()?;
        if let Some(proxy) = self.manager_proxy().await {
            if let Ok(_) = proxy.terminate_machine(name).await {
                return Ok(());
            }
        }
        self.run_machinectl(&["terminate", name]).await
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        self.require_root()?;
        if let Some(proxy) = self.manager_proxy().await {
            if let Ok(_) = proxy.power_off_machine(name).await {
                return Ok(());
            }
        }
        self.run_machinectl(&["poweroff", name]).await
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.require_root()?;
        if let Some(proxy) = self.manager_proxy().await {
            if let Ok(_) = proxy.reboot_machine(name).await {
                return Ok(());
            }
        }
        self.run_machinectl(&["reboot", name]).await
    }

    async fn enable(&self, name: &str) -> Result<()> {
        self.require_root()?;
        self.run_machinectl(&["enable", name]).await
    }

    async fn disable(&self, name: &str) -> Result<()> {
        self.require_root()?;
        self.run_machinectl(&["disable", name]).await
    }

    async fn kill(&self, name: &str, signal: &str) -> Result<()> {
        self.require_root()?;
        if let Some(proxy) = self.manager_proxy().await {
            let sig = signal.parse::<i32>().unwrap_or(15); // Default to SIGTERM
            if let Ok(_) = proxy.kill_machine(name, "all", sig).await {
                return Ok(());
            }
        }
        self.run_machinectl(&["kill", "-s", signal, name]).await
    }

    async fn get_logs(&self, name: &str, lines: usize) -> Result<Vec<String>> {
        // journalctl -M doesn't have a simple DBus equivalent for remote machine journals easily accessible via machine1
        // We'll keep the command for now as it's specializing in machine journals.
        let out = Command::new("journalctl")
            .args([
                "-M",
                name,
                "-n",
                &lines.to_string(),
                "--no-pager",
                "--output=short",
            ])
            .output()
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("journalctl"), e))?;

        if !out.status.success() {
            log::warn!(
                "journalctl -M {} failed: {}",
                name,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect())
    }

    async fn get_properties(&self, name: &str) -> Result<HashMap<String, String>> {
        let mut map = HashMap::new();

        // 1. Try DBus first
        if let Some(proxy) = self.manager_proxy().await {
            if let Ok(path) = proxy.get_machine(name).await {
                if let Some(conn) = self.connection().await {
                    let builder = zbus::fdo::PropertiesProxy::builder(conn)
                        .destination("org.freedesktop.machine1")
                        .and_then(|b| b.path(path));

                    if let Ok(builder) = builder {
                        if let Ok(props_proxy) = builder.build().await {
                            let interface: zbus::names::InterfaceName = "org.freedesktop.machine1.Machine".try_into().unwrap_or_else(|_| "org.freedesktop.machine1.Machine".try_into().unwrap());
                            if let Ok(all_props) = props_proxy.get_all(Some(interface).into()).await {
                                for (k, v) in all_props {
                                    map.insert(k, value_to_string(v.into()));
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. Fallback or Supplement with command
        let machine_out = Command::new("machinectl")
            .args(["show", name])
            .output()
            .await;

        if let Ok(out) = machine_out {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        map.entry(k.trim().to_string()).or_insert_with(|| v.trim().to_string());
                    }
                }
            }
        }

        // 3. Always supplement with systemctl show
        let system_out = Command::new("systemctl")
            .args(["show", &format!("systemd-nspawn@{}.service", name)])
            .output()
            .await;

        if let Ok(out) = system_out {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let key = k.trim();
                        if key == "UnitFileState" || !map.contains_key(key) {
                            map.insert(key.to_string(), v.trim().to_string());
                        }
                    }
                }
            }
        }

        if map.is_empty() {
            return Err(NspawnError::CommandFailed(
                format!("machinectl/systemctl show {}", name),
                "No properties found".to_string(),
            ));
        }

        Ok(map)
    }

    async fn create(
        &self,
        _cfg: &super::models::ContainerConfig,
        _storage: &dyn super::storage::StorageBackend,
    ) -> Result<()> {
        // This will be implemented when refactoring the creation logic.
        todo!("Implement create in SystemdManager")
    }

    async fn is_dbus_available(&self) -> bool {
        self.connection().await.is_some()
    }
}

/// Normalizes an OCI image reference for use with skopeo.
/// If it already contains a transport (e.g. docker://), it is returned as is.
/// Otherwise, docker:// is prepended.
fn normalize_oci_image_ref(image_ref: &str) -> String {
    let transports = [
        "docker://",
        "oci:",
        "dir:",
        "docker-archive:",
        "docker-daemon:",
        "ostree:",
        "containers-storage:",
    ];
    if transports.iter().any(|t| image_ref.starts_with(t)) || image_ref.contains("://") {
        image_ref.to_string()
    } else {
        format!("docker://{}", image_ref)
    }
}

/// Import an OCI registry image as a nspawn rootfs directory.
pub async fn import_oci_image(
    image_ref: &str,
    local_name: &str,
    dest: &std::path::Path,
) -> Result<()> {
    check_tool("skopeo")?;
    check_tool("umoci")?;

    let normalized_ref = normalize_oci_image_ref(image_ref);
    let tmp_oci = format!("/tmp/lasper-oci-{}-{}", local_name, std::process::id());
    let bundle_dir = format!("/tmp/lasper-bundle-{}-{}", local_name, std::process::id());

    // Closure for cleanup
    let cleanup = || {
        let t = tmp_oci.clone();
        let b = bundle_dir.clone();
        async move {
            let _ = tokio::fs::remove_dir_all(&t).await;
            let _ = tokio::fs::remove_dir_all(&b).await;
        }
    };

    log::info!("skopeo copy {} oci:{}:latest", normalized_ref, tmp_oci);
    let skopeo = Command::new("skopeo")
        .args(["copy", &normalized_ref, &format!("oci:{}:latest", tmp_oci)])
        .output()
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from("skopeo"), e))?;

    if !skopeo.status.success() {
        cleanup().await;
        return Err(NspawnError::CommandFailed(
            "skopeo copy".into(),
            String::from_utf8_lossy(&skopeo.stderr).trim().to_string(),
        ));
    }

    log::info!("umoci unpack --image {}:latest {}", tmp_oci, bundle_dir);
    let umoci = Command::new("umoci")
        .args([
            "unpack",
            "--image",
            &format!("{}:latest", tmp_oci),
            &bundle_dir,
        ])
        .output()
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from("umoci"), e))?;

    if !umoci.status.success() {
        cleanup().await;
        return Err(NspawnError::CommandFailed(
            "umoci unpack".into(),
            String::from_utf8_lossy(&umoci.stderr).trim().to_string(),
        ));
    }

    // Move rootfs content to dest
    let rootfs_source = std::path::Path::new(&bundle_dir).join("rootfs");
    if !rootfs_source.exists() {
        cleanup().await;
        return Err(NspawnError::DeployError(
            "umoci unpack did not create rootfs directory".into(),
        ));
    }

    log::info!(
        "Moving rootfs from {} to {}",
        rootfs_source.display(),
        dest.display()
    );

    // Ensure dest directory exists (or at least its parent)
    if let Some(parent) = dest.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    // Use 'cp -a' to copy contents including dotfiles, then cleanup
    let copy_out = Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", rootfs_source.to_string_lossy()),
            &dest.to_string_lossy(),
        ])
        .output()
        .await
        .map_err(|e| NspawnError::Io(dest.to_path_buf(), e))?;

    if !copy_out.status.success() {
        cleanup().await;
        return Err(NspawnError::CommandFailed(
            "cp rootfs".into(),
            String::from_utf8_lossy(&copy_out.stderr).trim().to_string(),
        ));
    }

    cleanup().await;
    log::info!("OCI image imported to {}", dest.display());
    Ok(())
}

/// Import a local disk image (.raw/.tar/.tar.gz/.qcow2) via `importctl`.
pub async fn import_disk_image(path: &str, local_name: &str, dest: &std::path::Path) -> Result<()> {
    check_tool("importctl")?;

    let subcommand = if path.ends_with(".tar")
        || path.ends_with(".tar.gz")
        || path.ends_with(".tar.xz")
        || path.ends_with(".tar.zst")
    {
        "import-tar"
    } else {
        "import-raw"
    };

    log::info!("importctl {} {} {}", subcommand, path, local_name);
    let out = Command::new("importctl")
        .args([subcommand, path, local_name])
        .output()
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from("importctl"), e))?;

    if !out.status.success() {
        return Err(NspawnError::CommandFailed(
            "importctl".into(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }

    let default_dest = std::path::PathBuf::from(format!("/var/lib/machines/{}", local_name));
    if dest != default_dest {
        log::info!("Moving imported image to {}", dest.display());
        tokio::fs::rename(&default_dest, dest)
            .await
            .map_err(|e| NspawnError::Io(dest.to_path_buf(), e))?;
    }

    Ok(())
}

pub fn check_tool(name: &str) -> Result<()> {
    let found = std::env::var_os("PATH")
        .unwrap_or_default()
        .to_string_lossy()
        .split(':')
        .map(|d| std::path::PathBuf::from(d).join(name))
        .any(|p| p.is_file());
    if found {
        Ok(())
    } else {
        Err(NspawnError::ToolNotFound(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_oci_image_ref() {
        assert_eq!(normalize_oci_image_ref("ubuntu"), "docker://ubuntu");
        assert_eq!(
            normalize_oci_image_ref("docker://ubuntu"),
            "docker://ubuntu"
        );
        assert_eq!(
            normalize_oci_image_ref("nvcr.io/nvidia/cuda:12.0"),
            "docker://nvcr.io/nvidia/cuda:12.0"
        );
        assert_eq!(
            normalize_oci_image_ref("oci:/tmp/myimage:latest"),
            "oci:/tmp/myimage:latest"
        );
    }
}

fn format_address(family: i32, data: &[u8]) -> String {
    match family {
        2 => {
            // AF_INET
            if data.len() == 4 {
                format!("{}.{}.{}.{}", data[0], data[1], data[2], data[3])
            } else {
                String::new()
            }
        }
        10 => {
            // AF_INET6
            if data.len() == 16 {
                let mut s = String::new();
                for i in 0..8 {
                    if i > 0 {
                        s.push(':');
                    }
                    s.push_str(&format!(
                        "{:x}",
                        u16::from_be_bytes([data[i * 2], data[i * 2 + 1]])
                    ));
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

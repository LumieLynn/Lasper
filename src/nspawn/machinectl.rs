//! High-level wrapper around `machinectl` and `journalctl` commands.

use super::errors::{NspawnError, Result};
use super::manager::NspawnManager;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::process::Command;

// ── Unified data model ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum ContainerState {
    Running,
    Stopped,
    Starting,
    Exiting,
}

impl ContainerState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Exiting => "exiting",
        }
    }
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running | Self::Starting | Self::Exiting)
    }
}

/// A container known to machinectl — either running, stopped, or both.
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
}

impl SystemdManager {
    pub fn new(is_root: bool) -> Self {
        Self { is_root }
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
                ContainerState::Stopped
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

    async fn start(&self, name: &str) -> Result<()> {
        self.require_root()?;
        self.run_machinectl(&["start", name]).await
    }

    async fn stop(&self, name: &str) -> Result<()> {
        self.require_root()?;
        self.run_machinectl(&["stop", name]).await
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        self.require_root()?;
        self.run_machinectl(&["terminate", name]).await
    }

    async fn get_logs(&self, name: &str, lines: usize) -> Result<Vec<String>> {
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
        let out = Command::new("machinectl")
            .args(["show", name])
            .output()
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        if !out.status.success() {
            return Err(NspawnError::CommandFailed(
                format!("machinectl show {}", name),
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }

        let mut map = HashMap::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        Ok(map)
    }

    #[allow(dead_code)]
    async fn create(
        &self,
        _cfg: &super::models::ContainerConfig,
        _storage: &dyn super::storage::StorageBackend,
    ) -> Result<()> {
        // This will be implemented when refactoring the creation logic.
        todo!("Implement create in SystemdManager")
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

//! OCI and Disk Image deployment implementations.

use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use tokio::process::Command;

use crate::nspawn::models::ContainerConfig;
use crate::nspawn::deploy::Deployer;
use crate::nspawn::errors::{NspawnError, Result};

pub struct OciDeployer {
    pub url: String,
}

#[async_trait]
impl Deployer for OciDeployer {
    async fn deploy(
        &self,
        name: &str,
        _cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        _logs: Arc<Mutex<Vec<String>>>,
    ) -> Result<()> {
        import_oci_image(&self.url, name, rootfs).await
    }
}

pub struct DiskImageDeployer {
    pub path: String,
}

#[async_trait]
impl Deployer for DiskImageDeployer {
    async fn deploy(
        &self,
        name: &str,
        _cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        _logs: Arc<Mutex<Vec<String>>>,
    ) -> Result<()> {
        import_disk_image(&self.path, name, rootfs).await
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

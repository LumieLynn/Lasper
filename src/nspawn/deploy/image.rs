//! OCI and Disk Image deployment implementations.

use crate::nspawn::utils::{new_command, CommandLogged};
use async_trait::async_trait;
use std::os::unix::fs::PermissionsExt;
#[allow(unused_imports)]
use std::sync::{Arc, Mutex};

use crate::nspawn::deploy::Deployer;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ContainerConfig;

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
        _logs: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        import_oci_image(&self.url, name, rootfs).await
    }
}

pub struct DiskImageDeployer {
    pub path: String,
}

impl DiskImageDeployer {
    fn is_tarball(&self) -> bool {
        let p = self.path.to_lowercase();
        p.ends_with(".tar")
            || p.ends_with(".tar.gz")
            || p.ends_with(".tar.xz")
            || p.ends_with(".tar.zst")
            || p.ends_with(".tgz")
    }
}

#[async_trait]
impl Deployer for DiskImageDeployer {
    fn is_external_storage_managed(&self) -> bool {
        !self.is_tarball()
    }

    async fn deploy(
        &self,
        name: &str,
        _cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        _logs: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        import_disk_image(&self.path, name, rootfs).await
    }
}

pub struct NetworkImageDeployer {
    pub url: String,
    pub is_raw: bool,
}

#[async_trait]
impl Deployer for NetworkImageDeployer {
    fn is_external_storage_managed(&self) -> bool {
        self.is_raw
    }

    async fn deploy(
        &self,
        name: &str,
        _cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        let clean_url = self.url.trim();
        // Use /var/cache/lasper for isolated downloads to bypass systemd-machined interference (Error 23)
        let cache_dir = "/var/cache/lasper/downloads";
        let _ = tokio::fs::create_dir_all(cache_dir).await;
        let _ = tokio::fs::create_dir_all("/var/lib/machines").await;

        let _ = logs
            .send(format!("Downloading container from {}...", clean_url))
            .await;
        check_tool("curl")?;

        if self.is_raw {
            check_tool("bash")?;
            let _ = logs
                .send("Streaming and provisioning RAW disk image to cache...".into())
                .await;

            let dest = format!("/var/lib/machines/{}.raw", name);
            let cache_dest = format!("{}/{}.raw.part", cache_dir, name);

            // Phase 1: Download and decompress into isolated cache
            let script = format!(
                "set -o pipefail; case '{url}' in \
                 *.xz)  curl -# -L -f -A 'Lasper/1.0' '{url}' | xz -d > '{cache_dest}' ;; \
                 *.gz)  curl -# -L -f -A 'Lasper/1.0' '{url}' | gzip -d > '{cache_dest}' ;; \
                 *.zst) curl -# -L -f -A 'Lasper/1.0' '{url}' | zstd -d > '{cache_dest}' ;; \
                 *.bz2) curl -# -L -f -A 'Lasper/1.0' '{url}' | bzip2 -d > '{cache_dest}' ;; \
                 *)     curl -# -L -f -A 'Lasper/1.0' '{url}' -o '{cache_dest}' ;; \
                 esac",
                url = clean_url,
                cache_dest = cache_dest
            );

            let mut child = new_command("bash")
                .args(["-c", &script])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| NspawnError::Io(std::path::PathBuf::from("bash"), e))?;

            stream_curl_logs(&mut child, logs.clone());

            let status = child
                .wait()
                .await
                .map_err(|e| NspawnError::Io(std::path::PathBuf::from("bash"), e))?;
            if !status.success() {
                let _ = tokio::fs::remove_file(&cache_dest).await;
                return Err(NspawnError::DeployError(format!(
                    "Raw image download/extraction failed: {}",
                    status
                )));
            }

            // Phase 2: Post-download validation in the cache zone
            let _ = logs.send("Validating disk image integrity...".into()).await;
            let validate = new_command("systemd-dissect")
                .args(["--validate", &cache_dest])
                .logged_output("systemd-dissect")
                .await
                .map_err(|e| NspawnError::Io(std::path::PathBuf::from("systemd-dissect"), e))?;

            if !validate.status.success() {
                let _ = tokio::fs::remove_file(&cache_dest).await;
                return Err(NspawnError::DeployError(
                    "Downloaded file is not a valid disk image.".into(),
                ));
            }

            // Phase 3: Finalize — move to hot zone protected by systemd-machined
            let _ = tokio::fs::rename(&cache_dest, &dest).await;
        } else {
            check_tool("tar")?;
            check_tool("bash")?;

            let cache_tar = format!("{}/{}.tar.part", cache_dir, name);
            let _ = logs
                .send("Downloading compressed tarball to cache...".into())
                .await;

            // Phase 1: Download to isolated cache file
            let download_script = format!(
                "set -o pipefail; curl -# -L -f -A 'Lasper/1.0' '{}' -o '{}'",
                clean_url, cache_tar
            );

            let mut child = new_command("bash")
                .args(["-c", &download_script])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| NspawnError::Io(std::path::PathBuf::from("bash"), e))?;

            stream_curl_logs(&mut child, logs.clone());

            let status = child
                .wait()
                .await
                .map_err(|e| NspawnError::Io(std::path::PathBuf::from("bash"), e))?;
            if !status.success() {
                let _ = tokio::fs::remove_file(&cache_tar).await;
                return Err(NspawnError::DeployError(format!(
                    "Network download failed: {}",
                    status
                )));
            }

            // Phase 2: Extract from local cache file directly into user-selected rootfs
            let _ = logs
                .send("Extracting tarball to storage backend...".into())
                .await;
            let extract_out = new_command("tar")
                .args([
                    "--numeric-owner",
                    "-pxf",
                    &cache_tar,
                    "-C",
                    &rootfs.to_string_lossy(),
                ])
                .logged_output("tar")
                .await
                .map_err(|e| NspawnError::Io(rootfs.to_path_buf(), e))?;

            let _ = tokio::fs::remove_file(&cache_tar).await;
            if !extract_out.status.success() {
                return Err(NspawnError::cmd_failed(
                    "tar -xf",
                    format!("tar -xf {} -C {}", cache_tar, rootfs.display()),
                    &extract_out,
                ));
            }
        }

        Ok(())
    }
}

/// Helper function to stream curl progress logs (split by \r) to the TUI.
fn stream_curl_logs(child: &mut tokio::process::Child, logs: tokio::sync::mpsc::Sender<String>) {
    use tokio::io::AsyncBufReadExt;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut buf = Vec::new();
            // Split by \r (carriage return) instead of \n to capture curl's progress bar updates correctly
            while let Ok(bytes) = reader.read_until(b'\r', &mut buf).await {
                if bytes == 0 {
                    break;
                }
                let line = String::from_utf8_lossy(&buf).trim().to_string();
                if !line.is_empty() {
                    let _ = logs.send(line).await;
                }
                buf.clear();
            }
        });
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

async fn ensure_container_policy() -> Result<()> {
    let policy_path = std::path::Path::new("/etc/containers/policy.json");
    if !policy_path.exists() {
        let default_policy = r#"{"default":[{"type":"insecureAcceptAnything"}]}"#;
        if let Some(parent) = policy_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        tokio::fs::write(policy_path, default_policy)
            .await
            .map_err(|e| NspawnError::Io(policy_path.to_path_buf(), e))?;
    }
    Ok(())
}

/// Import an OCI registry image as a nspawn rootfs directory.
pub async fn import_oci_image(
    image_ref: &str,
    local_name: &str,
    dest: &std::path::Path,
) -> Result<()> {
    check_tool("skopeo")?;
    check_tool("umoci")?;
    ensure_container_policy().await?;

    let normalized_ref = normalize_oci_image_ref(image_ref);
    let tmp_parent = "/var/cache/lasper/oci-staging";
    let _ = tokio::fs::create_dir_all(tmp_parent).await;
    let _ = std::fs::set_permissions(tmp_parent, std::fs::Permissions::from_mode(0o700));

    let tmp_oci = format!("{}/oci-{}-{}", tmp_parent, local_name, std::process::id());
    let bundle_dir = format!(
        "{}/bundle-{}-{}",
        tmp_parent,
        local_name,
        std::process::id()
    );

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
    let skopeo = new_command("skopeo")
        .args(["copy", &normalized_ref, &format!("oci:{}:latest", tmp_oci)])
        .logged_output("skopeo")
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from("skopeo"), e))?;

    if !skopeo.status.success() {
        cleanup().await;
        return Err(NspawnError::cmd_failed(
            "skopeo copy",
            format!("skopeo copy {} oci:{}:latest", normalized_ref, tmp_oci),
            &skopeo,
        ));
    }

    // Ensure dest directory exists (or at least its parent)
    if let Some(parent) = dest.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    log::info!(
        "umoci raw-unpack --image {}:latest {}",
        tmp_oci,
        dest.display()
    );
    let umoci_raw = new_command("umoci")
        .args([
            "raw-unpack",
            "--image",
            &format!("{}:latest", tmp_oci),
            &dest.to_string_lossy(),
        ])
        .logged_output("umoci")
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from("umoci"), e))?;

    if umoci_raw.status.success() {
        cleanup().await;
        log::info!("OCI image imported to {} via raw-unpack", dest.display());
        return Ok(());
    }

    // Fallback to older `umoci unpack` if raw-unpack fails
    log::warn!(
        "umoci raw-unpack failed or missing, falling back to unpack: {:?}",
        String::from_utf8_lossy(&umoci_raw.stderr)
    );
    let umoci = new_command("umoci")
        .args([
            "unpack",
            "--image",
            &format!("{}:latest", tmp_oci),
            &bundle_dir,
        ])
        .logged_output("umoci")
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from("umoci"), e))?;

    if !umoci.status.success() {
        cleanup().await;
        return Err(NspawnError::cmd_failed(
            "umoci unpack",
            format!("umoci unpack --image {}:latest {}", tmp_oci, bundle_dir),
            &umoci,
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

    // Use 'cp -a' to copy contents including dotfiles, then cleanup
    let copy_out = new_command("cp")
        .args([
            "-a",
            &format!("{}/.", rootfs_source.to_string_lossy()),
            &dest.to_string_lossy(),
        ])
        .logged_output("cp")
        .await
        .map_err(|e| NspawnError::Io(dest.to_path_buf(), e))?;

    if !copy_out.status.success() {
        cleanup().await;
        return Err(NspawnError::cmd_failed(
            "cp rootfs content",
            format!("cp -a {}/. {}", rootfs_source.display(), dest.display()),
            &copy_out,
        ));
    }

    cleanup().await;
    log::info!("OCI image imported to {}", dest.display());
    Ok(())
}

/// Import a local disk image (.raw/.tar/.tar.gz).
pub async fn import_disk_image(path: &str, local_name: &str, dest: &std::path::Path) -> Result<()> {
    let p = path.to_lowercase();
    if p.ends_with(".tar")
        || p.ends_with(".tar.gz")
        || p.ends_with(".tar.xz")
        || p.ends_with(".tar.zst")
        || p.ends_with(".tgz")
    {
        return import_disk_image_tar(path, dest).await;
    }

    check_tool("importctl")?;
    log::info!("importctl import-raw {} {}", path, local_name);
    let out = new_command("importctl")
        .args(["import-raw", path, local_name])
        .logged_output("importctl")
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from("importctl"), e))?;

    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "importctl import-raw",
            format!("importctl import-raw {} {}", path, local_name),
            &out,
        ));
    }

    // For raw imports, importctl already placed it in /var/lib/machines/NAME.
    // mod.rs handles the path correctly when is_external_storage_managed is true.
    Ok(())
}

async fn import_disk_image_tar(path: &str, dest: &std::path::Path) -> Result<()> {
    check_tool("tar")?;
    log::info!("Extracting tar {} to {}", path, dest.display());

    let out = new_command("tar")
        .args([
            "--numeric-owner",
            "-pxf",
            path,
            "-C",
            &dest.to_string_lossy(),
        ])
        .logged_output("tar")
        .await
        .map_err(|e| NspawnError::Io(dest.to_path_buf(), e))?;

    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "tar -xf",
            format!("tar -xf {} -C {}", path, dest.display()),
            &out,
        ));
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

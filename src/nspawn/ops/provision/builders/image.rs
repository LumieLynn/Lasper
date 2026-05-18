//! OCI and Disk Image deployment implementations.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ContainerConfig;
use crate::nspawn::ops::provision::Deployer;
use crate::nspawn::sys::{log_output, CommandRunner};

pub struct OciDeployer {
    pub url: String,
    pub cmd_runner: Arc<dyn CommandRunner>,
    pub io: crate::nspawn::sys::ElevatedIo,
}

#[async_trait]
impl Deployer for OciDeployer {
    async fn deploy(
        &self,
        name: &str,
        _cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        import_oci_image(self.cmd_runner.as_ref(), &self.io, &self.url, name, rootfs, &logs).await
    }
}

pub struct DiskImageDeployer {
    pub path: String,
    pub cmd_runner: Arc<dyn CommandRunner>,
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
        import_disk_image(self.cmd_runner.as_ref(), &self.path, name, rootfs).await
    }
}

pub struct NetworkImageDeployer {
    pub url: String,
    pub is_raw: bool,
    pub cmd_runner: Arc<dyn CommandRunner>,
    pub io: crate::nspawn::sys::ElevatedIo,
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
        let cache_dir = "/var/cache/lasper/downloads";
        let _ = self.io.create_dir_all(std::path::Path::new(cache_dir)).await;
        let _ = self
            .io
            .create_dir_all(&crate::paths::machines_dir())
            .await;

        let _ = logs
            .send(format!("Downloading container from {}...", clean_url))
            .await;
        check_tool("curl")?;

        if self.is_raw {
            check_tool("bash")?;
            let _ = logs
                .send("Streaming and provisioning RAW disk image to cache...".into())
                .await;

            let dest = crate::paths::machine_raw_image(name);
            let cache_dest = format!("{}/{}.raw.part", cache_dir, name);

            let script = "set -o pipefail; case \"$1\" in \
                 *.xz)  curl -# -L -f -A 'Lasper/1.0' \"$1\" | xz -d > \"$2\" ;; \
                 *.gz)  curl -# -L -f -A 'Lasper/1.0' \"$1\" | gzip -d > \"$2\" ;; \
                 *.zst) curl -# -L -f -A 'Lasper/1.0' \"$1\" | zstd -d > \"$2\" ;; \
                 *.bz2) curl -# -L -f -A 'Lasper/1.0' \"$1\" | bzip2 -d > \"$2\" ;; \
                 *)     curl -# -L -f -A 'Lasper/1.0' \"$1\" -o \"$2\" ;; \
                 esac";

            {
                let spawned = self.cmd_runner.spawn(
                    "bash",
                    vec![
                        "-c".into(), script.into(),
                        "--".into(), clean_url.into(), cache_dest.clone(),
                    ],
                ).await.map_err(|e| NspawnError::Io(std::path::PathBuf::from("bash"), e))?;
                stream_spawned(spawned, logs.clone()).await?;
            }

            let _ = logs.send("Validating disk image integrity...".into()).await;
            let validate = self.cmd_runner.run(
                "systemd-dissect",
                vec!["--validate".into(), cache_dest.clone()],
            ).await.map_err(|e| NspawnError::Io(std::path::PathBuf::from("systemd-dissect"), e))?;
            log_output("systemd-dissect", &validate);

            if !validate.status.success() {
                let _ = self.io.remove_file(std::path::Path::new(&cache_dest)).await;
                return Err(NspawnError::DeployError(
                    "Downloaded file is not a valid disk image.".into(),
                ));
            }

            // Move from cache to /var/lib/machines/
            let _ = self.cmd_runner.run(
                "mv",
                vec![cache_dest.clone(), dest.to_string_lossy().to_string()],
            ).await;
        } else {
            check_tool("tar")?;
            check_tool("bash")?;

            let cache_tar = format!("{}/{}.tar.part", cache_dir, name);
            let _ = logs
                .send("Downloading compressed tarball to cache...".into())
                .await;

            let download_script =
                "set -o pipefail; curl -# -L -f -A 'Lasper/1.0' \"$1\" -o \"$2\"";
            {
                let spawned = self.cmd_runner.spawn(
                    "bash",
                    vec![
                        "-c".into(), download_script.into(),
                        "--".into(), clean_url.into(), cache_tar.clone(),
                    ],
                ).await.map_err(|e| NspawnError::Io(std::path::PathBuf::from("bash"), e))?;
                stream_spawned(spawned, logs.clone()).await?;
            }

            let _ = logs
                .send("Extracting tarball to storage backend...".into())
                .await;
            let extract_out = self.cmd_runner.run(
                "tar",
                vec![
                    "--numeric-owner".into(), "-pxf".into(),
                    cache_tar.clone(), "-C".into(),
                    rootfs.to_string_lossy().to_string(),
                ],
            ).await.map_err(|e| NspawnError::Io(rootfs.to_path_buf(), e))?;
            log_output("tar", &extract_out);

            let _ = self.io.remove_file(std::path::Path::new(&cache_tar)).await;
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

/// Read stdout lines from a [`SpawnedProcess`], forward to logs, then wait
/// for exit status.  Returns an error if the process failed.
async fn stream_spawned(
    mut spawned: crate::nspawn::sys::SpawnedProcess,
    logs: tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    {
        let mut lines = tokio::io::BufReader::new(&mut spawned.stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim().to_string();
            if !trimmed.is_empty() {
                let _ = logs.send(trimmed).await;
            }
        }
    }
    let status = spawned
        .wait()
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from("bash"), e))?;
    if !status.success() {
        return Err(NspawnError::DeployError(format!(
            "Network download failed: {}",
            status
        )));
    }
    Ok(())
}

/// Normalize an OCI image reference for use with skopeo.
fn normalize_oci_image_ref(image_ref: &str) -> String {
    let transports = [
        "docker://", "oci:", "dir:", "docker-archive:",
        "docker-daemon:", "ostree:", "containers-storage:",
    ];
    if transports.iter().any(|t| image_ref.starts_with(t)) || image_ref.contains("://") {
        image_ref.to_string()
    } else {
        format!("docker://{}", image_ref)
    }
}

async fn ensure_container_policy(io: &crate::nspawn::sys::ElevatedIo) -> Result<()> {
    let policy_path = std::path::Path::new("/etc/containers/policy.json");
    if io.read_to_string(policy_path).await?.is_none() {
        log::info!("Initializing default OCI policy in /etc/containers/policy.json");
        let default_policy = r#"{ "default": [ { "type": "insecureAcceptAnything" } ] }"#;
        io.write(policy_path, default_policy).await?;
    }
    Ok(())
}

/// Import an OCI registry image as a nspawn rootfs directory.
pub async fn import_oci_image(
    cmd_runner: &dyn CommandRunner,
    io: &crate::nspawn::sys::ElevatedIo,
    image_ref: &str,
    local_name: &str,
    dest: &std::path::Path,
    logs: &tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    check_tool("skopeo")?;
    check_tool("umoci")?;
    ensure_container_policy(io).await?;

    let normalized_ref = normalize_oci_image_ref(image_ref);
    let staging_id = uuid::Uuid::new_v4();
    let staging_root = std::path::PathBuf::from(format!(
        "/var/cache/lasper/oci-staging/oci-deploy-{}-{}",
        local_name, staging_id
    ));
    io.create_dir_all(&staging_root).await?;
    let tmp_oci = staging_root.join("oci-repo");
    let bundle_dir = staging_root.join("bundle");

    let _ = logs
        .send(format!(
            "Pulling OCI image '{}' via skopeo...",
            normalized_ref
        ))
        .await;

    // skopeo copy — streamed
    {
        let mut spawned = cmd_runner.spawn(
            "skopeo",
            vec![
                "copy".into(),
                normalized_ref.clone(),
                format!("oci:{}:latest", tmp_oci.display()),
            ],
        ).await.map_err(|e| NspawnError::Io(std::path::PathBuf::from("skopeo"), e))?;
        {
            let mut lines = tokio::io::BufReader::new(&mut spawned.stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    let _ = logs.send(trimmed).await;
                }
            }
        }
        let status = spawned.wait().await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("skopeo"), e))?;
        if !status.success() {
            return Err(NspawnError::DeployError(format!(
                "skopeo copy failed (exit code: {})",
                status
            )));
        }
    }

    // Ensure dest parent exists
    if let Some(parent) = dest.parent() {
        let _ = io.create_dir_all(parent).await;
    }

    let _ = logs.send("Extracting rootfs with umoci...".into()).await;

    let umoci_raw = cmd_runner.run(
        "umoci",
        vec![
            "raw-unpack".into(),
            "--image".into(),
            format!("{}:latest", tmp_oci.display()),
            dest.to_string_lossy().to_string(),
        ],
    ).await.map_err(|e| NspawnError::Io(std::path::PathBuf::from("umoci"), e))?;
    log_output("umoci", &umoci_raw);

    if umoci_raw.status.success() {
        let _ = logs.send("OCI image imported via raw-unpack.".into()).await;
        return Ok(());
    }

    // Fallback: umoci unpack
    let _ = logs.send("umoci raw-unpack failed, falling back to unpack...".into()).await;
    let umoci = cmd_runner.run(
        "umoci",
        vec![
            "unpack".into(),
            "--image".into(),
            format!("{}:latest", tmp_oci.display()),
            bundle_dir.to_string_lossy().to_string(),
        ],
    ).await.map_err(|e| NspawnError::Io(std::path::PathBuf::from("umoci"), e))?;
    log_output("umoci", &umoci);

    if !umoci.status.success() {
        return Err(NspawnError::cmd_failed(
            "umoci unpack",
            format!(
                "umoci unpack --image {}:latest {}",
                tmp_oci.display(),
                bundle_dir.display()
            ),
            &umoci,
        ));
    }

    let rootfs_source = bundle_dir.join("rootfs");
    if io.read_to_string(&rootfs_source).await.is_err() {
        return Err(NspawnError::DeployError(
            "umoci unpack did not create rootfs directory".into(),
        ));
    }

    let _ = logs.send("Copying rootfs to destination...".into()).await;
    let copy_out = cmd_runner.run(
        "cp",
        vec![
            "-a".into(),
            format!("{}/.", rootfs_source.to_string_lossy()),
            dest.to_string_lossy().to_string(),
        ],
    ).await.map_err(|e| NspawnError::Io(dest.to_path_buf(), e))?;
    log_output("cp", &copy_out);

    if !copy_out.status.success() {
        return Err(NspawnError::cmd_failed(
            "cp rootfs content",
            format!("cp -a {}/. {}", rootfs_source.display(), dest.display()),
            &copy_out,
        ));
    }

    let _ = logs.send("OCI image imported successfully.".into()).await;
    // Cleanup staging directory (best-effort)
    let _ = io.remove_dir_all(&staging_root).await;
    Ok(())
}

/// Import a local disk image (.raw/.tar/.tar.gz).
pub async fn import_disk_image(
    cmd_runner: &dyn CommandRunner,
    path: &str,
    local_name: &str,
    dest: &std::path::Path,
) -> Result<()> {
    let p = path.to_lowercase();
    if p.ends_with(".tar")
        || p.ends_with(".tar.gz")
        || p.ends_with(".tar.xz")
        || p.ends_with(".tar.zst")
        || p.ends_with(".tgz")
    {
        return import_disk_image_tar(cmd_runner, path, dest).await;
    }

    check_tool("importctl")?;
    let out = cmd_runner.run(
        "importctl",
        vec![
            "import-raw".into(),
            path.into(),
            local_name.into(),
        ],
    ).await.map_err(|e| NspawnError::Io(std::path::PathBuf::from("importctl"), e))?;
    log_output("importctl", &out);

    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "importctl import-raw",
            format!("importctl import-raw {} {}", path, local_name),
            &out,
        ));
    }

    Ok(())
}

async fn import_disk_image_tar(
    cmd_runner: &dyn CommandRunner,
    path: &str,
    dest: &std::path::Path,
) -> Result<()> {
    check_tool("tar")?;
    let out = cmd_runner.run(
        "tar",
        vec![
            "--numeric-owner".into(),
            "-pxf".into(),
            path.into(),
            "-C".into(),
            dest.to_string_lossy().to_string(),
        ],
    ).await.map_err(|e| NspawnError::Io(dest.to_path_buf(), e))?;
    log_output("tar", &out);

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

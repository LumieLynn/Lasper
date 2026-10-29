use crate::adapters::process::{new_command, run_bounded_child_command};
use crate::domain::secret::SecretBytes;
use std::path::{Path, PathBuf};
use std::process::Output;

pub(crate) const ROOTFS_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const MAX_ROOTFS_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;

/// Execute an offline guest command through systemd-nspawn with bounded output and lifetime.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub(crate) trait RootfsProcessRunner: Send + Sync {
    async fn run(
        &self,
        rootfs: &Path,
        command: Vec<String>,
        stdin: Option<SecretBytes>,
    ) -> std::io::Result<Output>;
}

pub(crate) struct DefaultRootfsProcessRunner;

#[async_trait::async_trait]
impl RootfsProcessRunner for DefaultRootfsProcessRunner {
    async fn run(
        &self,
        rootfs: &Path,
        command: Vec<String>,
        stdin: Option<SecretBytes>,
    ) -> std::io::Result<Output> {
        let mut process = new_command("systemd-nspawn");
        process
            .arg("-D")
            .arg(rootfs)
            .arg("--quiet")
            .arg("--settings=no");
        if stdin.is_some() {
            process.arg("--pipe");
        }
        process.args(command);

        run_bounded_child_command(
            process,
            stdin,
            ROOTFS_COMMAND_TIMEOUT,
            &format!("systemd-nspawn for {}", rootfs.display()),
            MAX_ROOTFS_COMMAND_OUTPUT_BYTES,
        )
        .await
    }
}

pub(crate) fn nspawn_io_path() -> PathBuf {
    PathBuf::from("systemd-nspawn")
}

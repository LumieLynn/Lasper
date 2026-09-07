use crate::adapters::process::{new_command, run_bounded_child_command};
use crate::domain::secret::SecretBytes;
use std::path::{Path, PathBuf};
use std::process::Output;

pub(crate) const ROOTFS_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const MAX_ROOTFS_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;

/// Runs a fixed `systemd-nspawn` executable for typed rootfs operations.
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
            "systemd-nspawn rootfs helper",
            MAX_ROOTFS_COMMAND_OUTPUT_BYTES,
        )
        .await
        .map_err(|error| contextual_io_error(rootfs, error))
    }
}

fn contextual_io_error(rootfs: &Path, error: std::io::Error) -> std::io::Error {
    std::io::Error::new(
        error.kind(),
        format!("systemd-nspawn for {}: {error}", rootfs.display()),
    )
}

pub(crate) fn nspawn_io_path() -> PathBuf {
    PathBuf::from("systemd-nspawn")
}

use crate::domain::secret::SecretBytes;
use crate::nspawn::sys::new_command;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use tokio::io::AsyncWriteExt;

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
            process.arg("--pipe").stdin(Stdio::piped());
        } else {
            process.stdin(Stdio::null());
        }
        process.args(command).kill_on_drop(true);

        let mut child = process
            .spawn()
            .map_err(|error| contextual_io_error(rootfs, error))?;
        if let Some(input) = stdin {
            let mut child_stdin = child.stdin.take().ok_or_else(|| {
                std::io::Error::other(format!(
                    "systemd-nspawn stdin was not available for {}",
                    rootfs.display()
                ))
            })?;
            child_stdin
                .write_all(input.as_slice())
                .await
                .map_err(|error| contextual_io_error(rootfs, error))?;
            child_stdin
                .shutdown()
                .await
                .map_err(|error| contextual_io_error(rootfs, error))?;
        }

        child
            .wait_with_output()
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

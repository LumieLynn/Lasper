//! Debootstrap and Pacstrap deployment implementations.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ContainerConfig;
use crate::nspawn::ops::provision::Deployer;
use crate::nspawn::sys::CommandRunner;

pub struct DebootstrapDeployer {
    pub mirror: String,
    pub suite: String,
    pub packages: String,
    pub cmd_runner: Arc<dyn CommandRunner>,
}

#[async_trait]
impl Deployer for DebootstrapDeployer {
    async fn deploy(
        &self,
        _name: &str,
        cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        let mut args = vec![];
        if cfg.users.iter().any(|u| u.sudoer) {
            args.push("--include=sudo".to_string());
        }
        if !self.packages.is_empty() {
            let pkgs = self
                .packages
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(",");
            args.push(format!("--include={}", pkgs));
        }
        args.push(self.suite.clone());
        args.push(rootfs.to_string_lossy().to_string());
        if !self.mirror.is_empty() {
            args.push(self.mirror.clone());
        }

        run_bootstrap(self.cmd_runner.as_ref(), "debootstrap", args, logs).await
    }
}

pub struct PacstrapDeployer {
    pub packages: String,
    pub cmd_runner: Arc<dyn CommandRunner>,
}

#[async_trait]
impl Deployer for PacstrapDeployer {
    async fn deploy(
        &self,
        _name: &str,
        cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<String>,
    ) -> Result<()> {
        let mut args = vec![
            "-c".into(),
            rootfs.to_string_lossy().to_string(),
            "base".into(),
        ];
        if cfg.users.iter().any(|u| u.sudoer) {
            args.push("sudo".into());
        }
        args.extend(self.packages.split_whitespace().map(|s| s.to_string()));

        run_bootstrap(self.cmd_runner.as_ref(), "pacstrap", args, logs).await
    }
}

async fn run_bootstrap(
    cmd_runner: &dyn CommandRunner,
    prog: &str,
    args: Vec<String>,
    logs: tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    let mut spawned = cmd_runner
        .spawn(prog, args.clone())
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from(prog), e))?;

    // Take stdout, read all lines, then drop the reader before wait()
    // (wait drains remaining stdout internally to avoid pipe deadlocks).
    {
        let reader = &mut spawned.stdout;
        let mut lines = tokio::io::BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = logs.send(line).await;
        }
    }

    let status = spawned
        .wait()
        .await
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from(prog), e))?;
    if !status.success() {
        return Err(NspawnError::CommandFailed(
            format!("Bootstrap tool ({})", prog),
            format!("{} {}", prog, args.join(" ")),
            "Command failed. Check deployment logs for detailed output.".to_string(),
        ));
    }
    Ok(())
}

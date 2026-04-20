//! Debootstrap and Pacstrap deployment implementations.

use crate::nspawn::sys::new_command;
use async_trait::async_trait;
use std::process::Stdio;
#[allow(unused_imports)]
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::ContainerConfig;
use crate::nspawn::ops::provision::Deployer;

pub struct DebootstrapDeployer {
    pub mirror: String,
    pub suite: String,
    pub packages: String,
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

        run_command("debootstrap", args, logs).await
    }
}

pub struct PacstrapDeployer {
    pub packages: String,
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

        run_command("pacstrap", args, logs).await
    }
}

async fn run_command(
    prog: &str,
    args: Vec<String>,
    logs: tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    let mut cmd = new_command(prog);
    cmd.args(&args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| NspawnError::Io(std::path::PathBuf::from(prog), e))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| NspawnError::StorageError("Failed to capture stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| NspawnError::StorageError("Failed to capture stderr".into()))?;

    let l1 = logs.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = r.next_line().await {
            let _ = l1.send(line).await;
        }
    });
    let l2 = logs.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = r.next_line().await {
            let _ = l2.send(line).await;
        }
    });

    let status = child
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

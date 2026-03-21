//! Debootstrap and Pacstrap deployment implementations.

use std::sync::{Arc, Mutex};
use tokio::process::Command;
use tokio::io::{AsyncBufReadExt, BufReader};
use std::process::Stdio;
use async_trait::async_trait;

use crate::nspawn::models::ContainerConfig;
use crate::nspawn::deploy::Deployer;
use crate::nspawn::errors::{NspawnError, Result};

pub struct DebootstrapDeployer {
    pub mirror: String,
    pub suite: String,
}

#[async_trait]
impl Deployer for DebootstrapDeployer {
    async fn deploy(
        &self,
        _name: &str,
        cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: Arc<Mutex<Vec<String>>>,
    ) -> Result<()> {
        let mut args = vec![];
        if cfg.users.iter().any(|u| u.sudoer) {
            args.push("--include=sudo".to_string());
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
        logs: Arc<Mutex<Vec<String>>>,
    ) -> Result<()> {
        let mut args = vec!["-c".into(), rootfs.to_string_lossy().to_string(), "base".into()];
        if cfg.users.iter().any(|u| u.sudoer) {
            args.push("sudo".into());
        }
        args.extend(self.packages.split_whitespace().map(|s| s.to_string()));

        run_command("pacstrap", args, logs).await
    }
}

async fn run_command(prog: &str, args: Vec<String>, logs: Arc<Mutex<Vec<String>>>) -> Result<()> {
    let mut cmd = Command::new(prog);
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| NspawnError::Io(std::path::PathBuf::from(prog), e))?;
    
    let stdout = child.stdout.take().ok_or_else(|| NspawnError::StorageError("Failed to capture stdout".into()))?;
    let stderr = child.stderr.take().ok_or_else(|| NspawnError::StorageError("Failed to capture stderr".into()))?;
    
    let l1 = logs.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = r.next_line().await { l1.lock().unwrap().push(line); }
    });
    let l2 = logs.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = r.next_line().await { l2.lock().unwrap().push(line); }
    });
    
    let status = child.wait().await.map_err(|e| NspawnError::Io(std::path::PathBuf::from(prog), e))?;
    if !status.success() {
        return Err(NspawnError::CommandFailed(prog.into(), format!("exited with status {}", status)));
    }
    Ok(())
}

//! OCI and Disk Image deployment implementations.

use std::sync::{Arc, Mutex};
use async_trait::async_trait;

use crate::nspawn::models::ContainerConfig;
use crate::nspawn::deploy::Deployer;
use crate::nspawn::machinectl;
use crate::nspawn::errors::Result;

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
        machinectl::import_oci_image(&self.url, name, rootfs).await
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
        machinectl::import_disk_image(&self.path, name, rootfs).await
    }
}

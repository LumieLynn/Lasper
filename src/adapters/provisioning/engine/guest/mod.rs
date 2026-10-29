//! Offline guest initialization performed by a deployment transaction.

mod hostname;
mod network;
mod users;

use crate::adapters::process::{CommandRunner, DefaultRootfsProcessRunner, RootfsProcessRunner};
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::adapters::provisioning::engine::host::HostResultExt;
use crate::adapters::storage::rootfs::{access::RootfsAccess, RootfsTarget};
use crate::domain::machine::{validate_login_username, CreateUser, GuestHostname};
use crate::domain::secret::{validate_chpasswd_secret, SecretString};
use crate::domain::wayland::ContainerUserIdentity;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct GuestInitializer {
    commands: Arc<dyn CommandRunner>,
    processes: Arc<dyn RootfsProcessRunner>,
}

impl GuestInitializer {
    pub(super) fn new(commands: Arc<dyn CommandRunner>) -> Self {
        Self {
            commands,
            processes: Arc::new(DefaultRootfsProcessRunner),
        }
    }

    pub(super) async fn has_os_release(&self, target: &RootfsTarget) -> OperationResult<bool> {
        let Some(path) = target
            .existing_directory()
            .await
            .host_context("open guest rootfs")?
        else {
            return Ok(false);
        };
        Ok(path_exists_in_root(&path, "etc/os-release")?
            || path_exists_in_root(&path, "usr/lib/os-release")?)
    }

    pub(super) async fn supports_nspawn_commands(
        &self,
        target: &RootfsTarget,
    ) -> OperationResult<bool> {
        let Some(path) = target
            .existing_directory()
            .await
            .host_context("open guest rootfs")?
        else {
            return Ok(false);
        };
        path_is_directory_in_root(&path, "usr")
    }

    pub(super) async fn configure_hostname(
        &self,
        target: &RootfsTarget,
        hostname: &GuestHostname,
    ) -> OperationResult<()> {
        let path = target
            .required_directory()
            .await
            .host_context("open guest rootfs")?;
        hostname::configure_hostname_at(&path, hostname, self.commands.as_ref()).await
    }

    pub(super) async fn configure_network(
        &self,
        target: &RootfsTarget,
    ) -> OperationResult<Vec<String>> {
        let path = target
            .required_directory()
            .await
            .host_context("open guest rootfs")?;
        network::configure_network_at(&path, self.processes.as_ref()).await
    }

    pub(super) async fn set_root_password(
        &self,
        target: &RootfsTarget,
        password: SecretString,
    ) -> OperationResult<()> {
        validate_chpasswd_secret(password.expose_secret())
            .map_err(|error| OperationFailure::InvalidInput(error.message("root password")))?;
        let path = target
            .required_directory()
            .await
            .host_context("open guest rootfs")?;
        users::set_root_password(&path, password.expose_secret(), self.processes.as_ref()).await
    }

    pub(super) async fn create_user(
        &self,
        target: &RootfsTarget,
        user: &CreateUser,
        password: Option<SecretString>,
    ) -> OperationResult<()> {
        user.validate()
            .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?;
        if let Some(password) = &password {
            validate_chpasswd_secret(password.expose_secret())
                .map_err(|error| OperationFailure::InvalidInput(error.message("user password")))?;
        }
        let path = target
            .required_directory()
            .await
            .host_context("open guest rootfs")?;
        users::create_user_in_container(
            &path,
            user,
            password.as_ref().map(SecretString::expose_secret),
            self.processes.as_ref(),
        )
        .await
    }

    pub(super) async fn resolve_user_identity(
        &self,
        target: &RootfsTarget,
        username: &str,
    ) -> OperationResult<ContainerUserIdentity> {
        validate_login_username(username)
            .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?;
        let path = target
            .required_directory()
            .await
            .host_context("open guest rootfs")?;
        users::resolve_user_identity(&path, username, self.processes.as_ref()).await
    }
}

fn path_exists_in_root(rootfs: &Path, relative_path: &str) -> OperationResult<bool> {
    Ok(open_path_in_root(rootfs, relative_path)?.is_some())
}

fn path_is_directory_in_root(rootfs: &Path, relative_path: &str) -> OperationResult<bool> {
    let Some(file) = open_path_in_root(rootfs, relative_path)? else {
        return Ok(false);
    };
    file.metadata()
        .map(|metadata| metadata.is_dir())
        .map_err(|error| OperationFailure::Io(rootfs.to_path_buf(), error))
}

fn open_path_in_root(rootfs: &Path, relative_path: &str) -> OperationResult<Option<std::fs::File>> {
    RootfsAccess::open(rootfs)
        .and_then(|access| access.entry(relative_path))
        .map_err(|error| OperationFailure::Io(rootfs.to_path_buf(), error))
}

#[cfg(test)]
mod tests;

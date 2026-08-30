use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::log_output;
use crate::adapters::process::{CommandRunner, DefaultCommandRunner};
use crate::adapters::rootfs::hostname::configure_hostname_at;
use crate::adapters::rootfs::network::configure_network_at;
use crate::adapters::rootfs::nvidia::{
    cleanup_nvidia_files, configure_nvidia_rootfs, validate_cleanup_paths, validate_nvidia_config,
};
use crate::adapters::rootfs::process::{DefaultRootfsProcessRunner, RootfsProcessRunner};
use crate::adapters::rootfs::{users, wayland};
use crate::domain::machine::GuestHostname;
use crate::domain::machine::MachineName;
use crate::domain::provisioning::CreateUser;
use crate::domain::secret::{validate_chpasswd_secret, SecretString};
use crate::domain::wayland::ContainerUserIdentity;
use crate::ipc::protocol::rootfs as rootfs_wire;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A rootfs location whose host path is derived from validated identifiers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RootfsTarget {
    Machine {
        machine: MachineName,
    },
    ImageMount {
        machine: MachineName,
    },
    RawMount {
        machine: MachineName,
        mount_id: String,
    },
}

impl RootfsTarget {
    pub(crate) fn from_provisioned_path(name: &str, path: &Path) -> Result<Self> {
        let machine = parse_machine_name(name)?;
        if path == crate::paths::machine_root(machine.as_str()) {
            return Ok(Self::Machine { machine });
        }
        if path == crate::paths::machine_image_mount(machine.as_str()) {
            return Ok(Self::ImageMount { machine });
        }
        Err(NspawnError::Validation(format!(
            "Refusing unmanaged rootfs path: {}",
            path.display()
        )))
    }

    pub(crate) fn path(&self) -> Result<PathBuf> {
        match self {
            Self::Machine { machine } => Ok(crate::paths::machine_root(machine.as_str())),
            Self::ImageMount { machine } => Ok(crate::paths::machine_image_mount(machine.as_str())),
            Self::RawMount { machine, mount_id } => {
                let mount_id = parse_mount_id(mount_id)?;
                Ok(raw_mount_path(machine, &mount_id))
            }
        }
    }

    pub(crate) fn supports_raw_fallback(&self) -> bool {
        matches!(self, Self::Machine { .. })
    }
}

/// Typed access to mutations inside Lasper-managed container root filesystems.
#[derive(Clone)]
pub struct RootfsStore {
    executor: Arc<dyn RootfsExecutor>,
}

impl RootfsStore {
    pub(crate) fn direct() -> Self {
        Self {
            executor: Arc::new(DirectRootfsExecutor),
        }
    }

    pub(crate) fn elevated(daemon: Arc<ElevatedDaemon>) -> Self {
        Self {
            executor: Arc::new(ElevatedRootfsExecutor { daemon }),
        }
    }

    pub(crate) async fn has_os_release(&self, target: &RootfsTarget) -> Result<bool> {
        let result = self
            .execute(RootfsOperation::ProbeOsRelease(TargetRequest {
                target: target.clone(),
            }))
            .await?;
        result
            .present
            .ok_or_else(|| NspawnError::Runtime("rootfs probe operation returned no result".into()))
    }

    pub(crate) async fn supports_nspawn_commands(&self, target: &RootfsTarget) -> Result<bool> {
        let result = self
            .execute(RootfsOperation::ProbeNspawnCommandSupport(TargetRequest {
                target: target.clone(),
            }))
            .await?;
        result.present.ok_or_else(|| {
            NspawnError::Runtime("rootfs command-support probe returned no result".into())
        })
    }

    pub(crate) async fn mount_managed_raw(&self, name: &str) -> Result<Option<RootfsTarget>> {
        let target = RootfsTarget::RawMount {
            machine: parse_machine_name(name)?,
            mount_id: uuid::Uuid::new_v4().to_string(),
        };
        let result = self
            .execute(RootfsOperation::MountManagedRaw(TargetRequest {
                target: target.clone(),
            }))
            .await?;
        match result.present {
            Some(true) => Ok(Some(target)),
            Some(false) => Ok(None),
            None => Err(NspawnError::Runtime(
                "rootfs mount operation returned no result".into(),
            )),
        }
    }

    pub(crate) async fn unmount_managed_raw(&self, target: &RootfsTarget) -> Result<()> {
        self.execute(RootfsOperation::UnmountManagedRaw(TargetRequest {
            target: target.clone(),
        }))
        .await?;
        Ok(())
    }

    pub(crate) async fn configure_network(&self, target: &RootfsTarget) -> Result<Vec<String>> {
        let result = self
            .execute(RootfsOperation::ConfigureNetwork(TargetRequest {
                target: target.clone(),
            }))
            .await?;
        Ok(result.warnings)
    }

    pub(crate) async fn configure_hostname(
        &self,
        target: &RootfsTarget,
        hostname: &GuestHostname,
    ) -> Result<()> {
        self.execute(RootfsOperation::ConfigureHostname(
            ConfigureHostnameRequest {
                target: target.clone(),
                hostname: hostname.clone(),
            },
        ))
        .await?;
        Ok(())
    }

    pub(crate) async fn set_root_password(
        &self,
        target: &RootfsTarget,
        password: SecretString,
    ) -> Result<Vec<String>> {
        let result = self
            .execute(RootfsOperation::SetRootPassword(SetRootPasswordRequest {
                target: target.clone(),
                password,
            }))
            .await?;
        Ok(result.warnings)
    }

    pub(crate) async fn create_user(
        &self,
        target: &RootfsTarget,
        user: &CreateUser,
        password: Option<SecretString>,
    ) -> Result<Vec<String>> {
        let result = self
            .execute(RootfsOperation::CreateUser(CreateUserRequest {
                target: target.clone(),
                username: user.username.clone(),
                uid: user.uid,
                password,
                sudoer: user.sudoer,
                shell: user.shell.clone(),
            }))
            .await?;
        Ok(result.warnings)
    }

    pub(crate) async fn configure_wayland(
        &self,
        target: &RootfsTarget,
        identity: &ContainerUserIdentity,
        shell: &str,
        default_display: &crate::domain::wayland::WaylandDisplay,
    ) -> Result<()> {
        self.execute(RootfsOperation::ConfigureWayland(ConfigureWaylandRequest {
            target: target.clone(),
            identity: identity.clone(),
            shell: shell.to_string(),
            default_display: default_display.clone(),
        }))
        .await?;
        Ok(())
    }

    pub(crate) async fn resolve_user_identity(
        &self,
        target: &RootfsTarget,
        username: &str,
    ) -> Result<ContainerUserIdentity> {
        let result = self
            .execute(RootfsOperation::ResolveUserIdentity(
                ResolveUserIdentityRequest {
                    target: target.clone(),
                    username: username.to_string(),
                },
            ))
            .await?;
        result.identity.ok_or_else(|| {
            NspawnError::Runtime("rootfs identity lookup returned no identity".into())
        })
    }

    pub(crate) async fn configure_nvidia(
        &self,
        target: &RootfsTarget,
        ld_cache_folders: Vec<String>,
        environment: Vec<(String, String)>,
        write_environment: bool,
    ) -> Result<Vec<String>> {
        let result = self
            .execute(RootfsOperation::ConfigureNvidia(ConfigureNvidiaRequest {
                target: target.clone(),
                ld_cache_folders,
                environment,
                write_environment,
            }))
            .await?;
        Ok(result.warnings)
    }

    pub(crate) async fn cleanup_nvidia(&self, name: &str, paths: &[String]) -> Result<Vec<String>> {
        let result = self
            .execute(RootfsOperation::CleanupNvidia(CleanupNvidiaRequest {
                target: RootfsTarget::Machine {
                    machine: parse_machine_name(name)?,
                },
                paths: paths.to_vec(),
            }))
            .await?;
        Ok(result.warnings)
    }

    async fn execute(&self, operation: RootfsOperation) -> Result<RootfsResult> {
        self.executor.execute(operation).await
    }
}

impl std::fmt::Debug for RootfsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootfsStore")
            .field("route", &self.executor.route())
            .finish()
    }
}

impl Default for RootfsStore {
    fn default() -> Self {
        Self::direct()
    }
}

#[async_trait::async_trait]
trait RootfsExecutor: Send + Sync + 'static {
    fn route(&self) -> &'static str;

    async fn execute(&self, operation: RootfsOperation) -> Result<RootfsResult>;
}

struct DirectRootfsExecutor;

#[async_trait::async_trait]
impl RootfsExecutor for DirectRootfsExecutor {
    fn route(&self) -> &'static str {
        "direct"
    }

    async fn execute(&self, operation: RootfsOperation) -> Result<RootfsResult> {
        execute_rootfs_operation_with_runners(
            operation,
            &DefaultCommandRunner,
            &DefaultRootfsProcessRunner,
        )
        .await
    }
}

struct ElevatedRootfsExecutor {
    daemon: Arc<ElevatedDaemon>,
}

#[async_trait::async_trait]
impl RootfsExecutor for ElevatedRootfsExecutor {
    fn route(&self) -> &'static str {
        "elevated_rpc"
    }

    async fn execute(&self, operation: RootfsOperation) -> Result<RootfsResult> {
        self.daemon
            .rootfs(operation)
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))
    }
}

#[derive(Debug)]
pub(crate) enum RootfsOperation {
    ProbeOsRelease(TargetRequest),
    ProbeNspawnCommandSupport(TargetRequest),
    MountManagedRaw(TargetRequest),
    UnmountManagedRaw(TargetRequest),
    ConfigureHostname(ConfigureHostnameRequest),
    ConfigureNetwork(TargetRequest),
    SetRootPassword(SetRootPasswordRequest),
    CreateUser(CreateUserRequest),
    ResolveUserIdentity(ResolveUserIdentityRequest),
    ConfigureWayland(ConfigureWaylandRequest),
    ConfigureNvidia(ConfigureNvidiaRequest),
    CleanupNvidia(CleanupNvidiaRequest),
}

#[derive(Clone, Debug)]
pub(crate) struct TargetRequest {
    target: RootfsTarget,
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigureHostnameRequest {
    target: RootfsTarget,
    hostname: GuestHostname,
}

#[derive(Debug)]
pub(crate) struct SetRootPasswordRequest {
    target: RootfsTarget,
    password: SecretString,
}

#[derive(Debug)]
pub(crate) struct CreateUserRequest {
    target: RootfsTarget,
    username: String,
    uid: Option<u32>,
    password: Option<SecretString>,
    sudoer: bool,
    shell: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigureWaylandRequest {
    target: RootfsTarget,
    identity: ContainerUserIdentity,
    shell: String,
    default_display: crate::domain::wayland::WaylandDisplay,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolveUserIdentityRequest {
    target: RootfsTarget,
    username: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigureNvidiaRequest {
    target: RootfsTarget,
    ld_cache_folders: Vec<String>,
    environment: Vec<(String, String)>,
    write_environment: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CleanupNvidiaRequest {
    target: RootfsTarget,
    paths: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RootfsResult {
    present: Option<bool>,
    warnings: Vec<String>,
    identity: Option<ContainerUserIdentity>,
}

impl TryFrom<rootfs_wire::RootfsOperation> for RootfsOperation {
    type Error = NspawnError;

    fn try_from(operation: rootfs_wire::RootfsOperation) -> Result<Self> {
        use rootfs_wire::RootfsOperation as Wire;

        Ok(match operation {
            Wire::ProbeOsRelease(request) => Self::ProbeOsRelease(TargetRequest {
                target: target_from_wire(request.target)?,
            }),
            Wire::ProbeNspawnCommandSupport(request) => {
                Self::ProbeNspawnCommandSupport(TargetRequest {
                    target: target_from_wire(request.target)?,
                })
            }
            Wire::MountManagedRaw(request) => Self::MountManagedRaw(TargetRequest {
                target: target_from_wire(request.target)?,
            }),
            Wire::UnmountManagedRaw(request) => Self::UnmountManagedRaw(TargetRequest {
                target: target_from_wire(request.target)?,
            }),
            Wire::ConfigureHostname(request) => Self::ConfigureHostname(ConfigureHostnameRequest {
                target: target_from_wire(request.target)?,
                hostname: GuestHostname::try_from(request.hostname)
                    .map_err(|error| NspawnError::Validation(error.to_string()))?,
            }),
            Wire::ConfigureNetwork(request) => Self::ConfigureNetwork(TargetRequest {
                target: target_from_wire(request.target)?,
            }),
            Wire::SetRootPassword(request) => Self::SetRootPassword(SetRootPasswordRequest {
                target: target_from_wire(request.target)?,
                password: request.password,
            }),
            Wire::CreateUser(request) => Self::CreateUser(CreateUserRequest {
                target: target_from_wire(request.target)?,
                username: request.username,
                uid: request.uid,
                password: request.password,
                sudoer: request.sudoer,
                shell: request.shell,
            }),
            Wire::ResolveUserIdentity(request) => {
                Self::ResolveUserIdentity(ResolveUserIdentityRequest {
                    target: target_from_wire(request.target)?,
                    username: request.username,
                })
            }
            Wire::ConfigureWayland(request) => Self::ConfigureWayland(ConfigureWaylandRequest {
                target: target_from_wire(request.target)?,
                identity: request.identity,
                shell: request.shell,
                default_display: request.default_display,
            }),
            Wire::ConfigureNvidia(request) => Self::ConfigureNvidia(ConfigureNvidiaRequest {
                target: target_from_wire(request.target)?,
                ld_cache_folders: request.ld_cache_folders,
                environment: request.environment,
                write_environment: request.write_environment,
            }),
            Wire::CleanupNvidia(request) => Self::CleanupNvidia(CleanupNvidiaRequest {
                target: target_from_wire(request.target)?,
                paths: request.paths,
            }),
        })
    }
}

impl From<RootfsOperation> for rootfs_wire::RootfsOperation {
    fn from(operation: RootfsOperation) -> Self {
        use rootfs_wire::RootfsOperation as Wire;

        match operation {
            RootfsOperation::ProbeOsRelease(request) => Wire::ProbeOsRelease(request.into()),
            RootfsOperation::ProbeNspawnCommandSupport(request) => {
                Wire::ProbeNspawnCommandSupport(request.into())
            }
            RootfsOperation::MountManagedRaw(request) => Wire::MountManagedRaw(request.into()),
            RootfsOperation::UnmountManagedRaw(request) => Wire::UnmountManagedRaw(request.into()),
            RootfsOperation::ConfigureHostname(request) => Wire::ConfigureHostname(request.into()),
            RootfsOperation::ConfigureNetwork(request) => Wire::ConfigureNetwork(request.into()),
            RootfsOperation::SetRootPassword(request) => Wire::SetRootPassword(request.into()),
            RootfsOperation::CreateUser(request) => Wire::CreateUser(request.into()),
            RootfsOperation::ResolveUserIdentity(request) => {
                Wire::ResolveUserIdentity(request.into())
            }
            RootfsOperation::ConfigureWayland(request) => Wire::ConfigureWayland(request.into()),
            RootfsOperation::ConfigureNvidia(request) => Wire::ConfigureNvidia(request.into()),
            RootfsOperation::CleanupNvidia(request) => Wire::CleanupNvidia(request.into()),
        }
    }
}

impl From<RootfsResult> for rootfs_wire::RootfsResult {
    fn from(result: RootfsResult) -> Self {
        Self {
            present: result.present,
            warnings: result.warnings,
            identity: result.identity,
        }
    }
}

impl From<rootfs_wire::RootfsResult> for RootfsResult {
    fn from(result: rootfs_wire::RootfsResult) -> Self {
        Self {
            present: result.present,
            warnings: result.warnings,
            identity: result.identity,
        }
    }
}

fn target_from_wire(target: rootfs_wire::RootfsTarget) -> Result<RootfsTarget> {
    Ok(match target {
        rootfs_wire::RootfsTarget::Machine { machine } => RootfsTarget::Machine {
            machine: parse_machine_name(&machine)?,
        },
        rootfs_wire::RootfsTarget::ImageMount { machine } => RootfsTarget::ImageMount {
            machine: parse_machine_name(&machine)?,
        },
        rootfs_wire::RootfsTarget::RawMount { machine, mount_id } => RootfsTarget::RawMount {
            machine: parse_machine_name(&machine)?,
            mount_id,
        },
    })
}

impl From<TargetRequest> for rootfs_wire::TargetRequest {
    fn from(request: TargetRequest) -> Self {
        Self {
            target: request.target.into(),
        }
    }
}

impl From<RootfsTarget> for rootfs_wire::RootfsTarget {
    fn from(target: RootfsTarget) -> Self {
        match target {
            RootfsTarget::Machine { machine } => Self::Machine {
                machine: machine.into_string(),
            },
            RootfsTarget::ImageMount { machine } => Self::ImageMount {
                machine: machine.into_string(),
            },
            RootfsTarget::RawMount { machine, mount_id } => Self::RawMount {
                machine: machine.into_string(),
                mount_id,
            },
        }
    }
}

impl From<ConfigureHostnameRequest> for rootfs_wire::ConfigureHostnameRequest {
    fn from(request: ConfigureHostnameRequest) -> Self {
        Self {
            target: request.target.into(),
            hostname: request.hostname.into_string(),
        }
    }
}

impl From<SetRootPasswordRequest> for rootfs_wire::SetRootPasswordRequest {
    fn from(request: SetRootPasswordRequest) -> Self {
        Self {
            target: request.target.into(),
            password: request.password,
        }
    }
}

impl From<CreateUserRequest> for rootfs_wire::CreateUserRequest {
    fn from(request: CreateUserRequest) -> Self {
        Self {
            target: request.target.into(),
            username: request.username,
            uid: request.uid,
            password: request.password,
            sudoer: request.sudoer,
            shell: request.shell,
        }
    }
}

impl From<ConfigureWaylandRequest> for rootfs_wire::ConfigureWaylandRequest {
    fn from(request: ConfigureWaylandRequest) -> Self {
        Self {
            target: request.target.into(),
            identity: request.identity,
            shell: request.shell,
            default_display: request.default_display,
        }
    }
}

impl From<ResolveUserIdentityRequest> for rootfs_wire::ResolveUserIdentityRequest {
    fn from(request: ResolveUserIdentityRequest) -> Self {
        Self {
            target: request.target.into(),
            username: request.username,
        }
    }
}

impl From<ConfigureNvidiaRequest> for rootfs_wire::ConfigureNvidiaRequest {
    fn from(request: ConfigureNvidiaRequest) -> Self {
        Self {
            target: request.target.into(),
            ld_cache_folders: request.ld_cache_folders,
            environment: request.environment,
            write_environment: request.write_environment,
        }
    }
}

impl From<CleanupNvidiaRequest> for rootfs_wire::CleanupNvidiaRequest {
    fn from(request: CleanupNvidiaRequest) -> Self {
        Self {
            target: request.target.into(),
            paths: request.paths,
        }
    }
}

pub(crate) async fn execute_rootfs_operation(operation: RootfsOperation) -> Result<RootfsResult> {
    execute_rootfs_operation_with_runners(
        operation,
        &DefaultCommandRunner,
        &DefaultRootfsProcessRunner,
    )
    .await
}

async fn execute_rootfs_operation_with_runners(
    operation: RootfsOperation,
    runner: &dyn CommandRunner,
    rootfs_runner: &dyn RootfsProcessRunner,
) -> Result<RootfsResult> {
    match operation {
        RootfsOperation::ProbeOsRelease(request) => {
            let path = request.target.path()?;
            if !validate_optional_rootfs_directory(&path).await? {
                return Ok(RootfsResult {
                    present: Some(false),
                    ..Default::default()
                });
            }
            let present = path_exists_in_root(&path, "etc/os-release")?
                || path_exists_in_root(&path, "usr/lib/os-release")?;
            Ok(RootfsResult {
                present: Some(present),
                ..Default::default()
            })
        }
        RootfsOperation::ProbeNspawnCommandSupport(request) => {
            let path = request.target.path()?;
            if !validate_optional_rootfs_directory(&path).await? {
                return Ok(RootfsResult {
                    present: Some(false),
                    ..Default::default()
                });
            }
            Ok(RootfsResult {
                present: Some(path_is_directory_in_root(&path, "usr")?),
                ..Default::default()
            })
        }
        RootfsOperation::MountManagedRaw(request) => {
            let (machine, mount_id) = raw_mount_parts(&request.target)?;
            let mounted = mount_managed_raw_at(&machine, &mount_id, runner).await?;
            Ok(RootfsResult {
                present: Some(mounted),
                ..Default::default()
            })
        }
        RootfsOperation::UnmountManagedRaw(request) => {
            let (machine, mount_id) = raw_mount_parts(&request.target)?;
            unmount_managed_raw_at(&machine, &mount_id, runner).await?;
            Ok(RootfsResult::default())
        }
        RootfsOperation::ConfigureHostname(request) => {
            let path = request.target.path()?;
            validate_required_rootfs_directory(&path).await?;
            configure_hostname_at(&path, &request.hostname, runner).await?;
            Ok(RootfsResult::default())
        }
        RootfsOperation::ConfigureNetwork(request) => {
            let path = request.target.path()?;
            validate_required_rootfs_directory(&path).await?;
            let warnings = configure_network_at(&path, runner).await?;
            Ok(RootfsResult {
                warnings,
                ..Default::default()
            })
        }
        RootfsOperation::SetRootPassword(request) => {
            validate_chpasswd_secret(request.password.expose_secret())
                .map_err(|error| NspawnError::Validation(error.message("root password")))?;
            let path = request.target.path()?;
            validate_required_rootfs_directory(&path).await?;
            let warnings =
                users::set_root_password(&path, request.password.expose_secret(), rootfs_runner)
                    .await?;
            Ok(RootfsResult {
                warnings,
                ..Default::default()
            })
        }
        RootfsOperation::CreateUser(request) => {
            let user = CreateUser {
                username: request.username,
                uid: request.uid,
                sudoer: request.sudoer,
                shell: request.shell,
            };
            user.validate()
                .map_err(|error| NspawnError::Validation(error.to_string()))?;
            if let Some(password) = &request.password {
                validate_chpasswd_secret(password.expose_secret())
                    .map_err(|error| NspawnError::Validation(error.message("user password")))?;
            }
            let path = request.target.path()?;
            validate_required_rootfs_directory(&path).await?;
            let warnings = users::create_user_in_container(
                &path,
                &user,
                request.password.as_ref().map(SecretString::expose_secret),
                rootfs_runner,
            )
            .await?;
            Ok(RootfsResult {
                warnings,
                ..Default::default()
            })
        }
        RootfsOperation::ResolveUserIdentity(request) => {
            crate::domain::provisioning::validate_login_username(&request.username)
                .map_err(|error| NspawnError::Validation(error.to_string()))?;
            let path = request.target.path()?;
            validate_required_rootfs_directory(&path).await?;
            let identity =
                users::resolve_user_identity(&path, &request.username, rootfs_runner).await?;
            Ok(RootfsResult {
                identity: Some(identity),
                ..Default::default()
            })
        }
        RootfsOperation::ConfigureWayland(request) => {
            wayland::validate_wayland_config(&request.identity.username, &request.shell)?;
            let path = request.target.path()?;
            validate_required_rootfs_directory(&path).await?;
            let observed =
                users::resolve_user_identity(&path, &request.identity.username, rootfs_runner)
                    .await?;
            if observed != request.identity {
                return Err(NspawnError::Validation(format!(
                    "Wayland target identity changed: expected {}:{} for {}, observed {}:{}",
                    request.identity.uid,
                    request.identity.gid,
                    request.identity.username,
                    observed.uid,
                    observed.gid,
                )));
            }
            wayland::setup_wayland_shell_env(
                &path,
                &request.identity.username,
                &request.shell,
                &crate::adapters::wayland::container_socket_path(
                    request.identity.uid,
                    &request.default_display,
                ),
                rootfs_runner,
            )
            .await?;
            Ok(RootfsResult::default())
        }
        RootfsOperation::ConfigureNvidia(request) => {
            validate_nvidia_config(
                &request.ld_cache_folders,
                &request.environment,
                request.write_environment,
            )?;
            let path = request.target.path()?;
            validate_required_rootfs_directory(&path).await?;
            let warnings = configure_nvidia_rootfs(
                &path,
                &request.ld_cache_folders,
                &request.environment,
                request.write_environment,
                rootfs_runner,
            )
            .await?;
            Ok(RootfsResult {
                warnings,
                ..Default::default()
            })
        }
        RootfsOperation::CleanupNvidia(request) => {
            validate_cleanup_paths(&request.paths)?;
            let path = request.target.path()?;
            if !validate_optional_rootfs_directory(&path).await? {
                return Ok(RootfsResult::default());
            }
            let warnings = cleanup_nvidia_files(&path, &request.paths, rootfs_runner).await?;
            Ok(RootfsResult {
                warnings,
                ..Default::default()
            })
        }
    }
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

fn parse_mount_id(mount_id: &str) -> Result<uuid::Uuid> {
    let parsed = uuid::Uuid::parse_str(mount_id).map_err(|_| {
        NspawnError::Validation(format!("invalid managed rootfs mount id: {mount_id:?}"))
    })?;
    if parsed.to_string() != mount_id {
        return Err(NspawnError::Validation(format!(
            "non-canonical managed rootfs mount id: {mount_id:?}"
        )));
    }
    Ok(parsed)
}

fn raw_mount_parts(target: &RootfsTarget) -> Result<(MachineName, uuid::Uuid)> {
    match target {
        RootfsTarget::RawMount { machine, mount_id } => {
            Ok((machine.clone(), parse_mount_id(mount_id)?))
        }
        _ => Err(NspawnError::Validation(
            "raw mount operation requires a managed raw mount target".into(),
        )),
    }
}

fn raw_mount_path(machine: &MachineName, mount_id: &uuid::Uuid) -> PathBuf {
    crate::paths::rootfs_mounts_dir().join(format!(
        "lasper-dissect-{}-{}",
        machine.as_str(),
        mount_id
    ))
}

async fn validate_optional_rootfs_directory(path: &Path) -> Result<bool> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(NspawnError::Io(path.to_path_buf(), error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(NspawnError::Validation(format!(
            "Refusing non-directory or symlink rootfs target: {}",
            path.display()
        )));
    }
    Ok(true)
}

async fn validate_required_rootfs_directory(path: &Path) -> Result<()> {
    if validate_optional_rootfs_directory(path).await? {
        Ok(())
    } else {
        Err(NspawnError::Validation(format!(
            "Managed rootfs target does not exist: {}",
            path.display()
        )))
    }
}

async fn mount_managed_raw_at(
    machine: &MachineName,
    mount_id: &uuid::Uuid,
    runner: &dyn CommandRunner,
) -> Result<bool> {
    let image = crate::paths::machine_raw_image(machine.as_str());
    let metadata = match tokio::fs::symlink_metadata(&image).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(NspawnError::Io(image, error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(NspawnError::Validation(format!(
            "Refusing non-file or symlink managed raw image: {}",
            image.display()
        )));
    }

    let parent = crate::paths::rootfs_mounts_dir();
    let application_cache = parent.parent().ok_or_else(|| {
        NspawnError::Validation("managed rootfs mount parent has no parent directory".into())
    })?;
    ensure_private_directory(application_cache).await?;
    ensure_private_directory(&parent).await?;

    let mount_point = raw_mount_path(machine, mount_id);
    tokio::fs::create_dir(&mount_point)
        .await
        .map_err(|error| NspawnError::Io(mount_point.clone(), error))?;
    tokio::fs::set_permissions(&mount_point, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| NspawnError::Io(mount_point.clone(), error))?;

    let output = match runner
        .run(
            "systemd-dissect",
            vec![
                "--mount".into(),
                image.to_string_lossy().to_string(),
                mount_point.to_string_lossy().to_string(),
            ],
        )
        .await
    {
        Ok(output) => output,
        Err(error) => {
            let _ = tokio::fs::remove_dir(&mount_point).await;
            return Err(NspawnError::Io(PathBuf::from("systemd-dissect"), error));
        }
    };
    log_output("systemd-dissect", &output);
    if output.status.success() {
        return Ok(true);
    }

    let _ = tokio::fs::remove_dir(&mount_point).await;
    Err(NspawnError::cmd_failed(
        "mount managed raw image for configuration",
        format!(
            "systemd-dissect --mount {} {}",
            image.display(),
            mount_point.display()
        ),
        &output,
    ))
}

async fn ensure_private_directory(path: &Path) -> Result<()> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir(path)
                .await
                .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
            tokio::fs::symlink_metadata(path)
                .await
                .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?
        }
        Err(error) => return Err(NspawnError::Io(path.to_path_buf(), error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(NspawnError::Validation(format!(
            "Refusing unsafe managed rootfs mount directory: {}",
            path.display()
        )));
    }
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))
}

async fn unmount_managed_raw_at(
    machine: &MachineName,
    mount_id: &uuid::Uuid,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let mount_point = raw_mount_path(machine, mount_id);
    validate_required_rootfs_directory(&mount_point).await?;
    let mount_point_string = mount_point.to_string_lossy().to_string();
    let output = runner
        .run(
            "systemd-dissect",
            vec!["--umount".into(), mount_point_string.clone()],
        )
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("systemd-dissect"), error))?;
    log_output("systemd-dissect --umount", &output);

    if !output.status.success() {
        let fallback = runner
            .run("umount", vec![mount_point_string])
            .await
            .map_err(|error| NspawnError::Io(PathBuf::from("umount"), error))?;
        log_output("umount", &fallback);
        if !fallback.status.success() {
            return Err(NspawnError::cmd_failed(
                "unmount managed raw image configuration mount",
                format!("umount {}", mount_point.display()),
                &fallback,
            ));
        }
    }

    tokio::fs::remove_dir(&mount_point)
        .await
        .map_err(|error| NspawnError::Io(mount_point, error))
}

fn path_exists_in_root(rootfs: &Path, relative_path: &str) -> Result<bool> {
    Ok(open_path_in_root(rootfs, relative_path)?.is_some())
}

fn path_is_directory_in_root(rootfs: &Path, relative_path: &str) -> Result<bool> {
    let Some(file) = open_path_in_root(rootfs, relative_path)? else {
        return Ok(false);
    };
    file.metadata()
        .map(|metadata| metadata.is_dir())
        .map_err(|error| NspawnError::Io(rootfs.to_path_buf(), error))
}

fn open_path_in_root(rootfs: &Path, relative_path: &str) -> Result<Option<std::fs::File>> {
    let root = std::fs::File::open(rootfs)
        .map_err(|error| NspawnError::Io(rootfs.to_path_buf(), error))?;
    let relative_path = CString::new(relative_path).map_err(|_| {
        NspawnError::Validation("rootfs probe path contains an interior NUL byte".into())
    })?;
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (libc::O_PATH | libc::O_CLOEXEC) as u64;
    how.resolve = libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_MAGICLINKS;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            relative_path.as_ptr(),
            &how,
            std::mem::size_of::<libc::open_how>(),
        )
    };
    if fd >= 0 {
        return Ok(Some(unsafe {
            std::fs::File::from_raw_fd(fd as libc::c_int)
        }));
    }

    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOENT) | Some(libc::ENOTDIR) => Ok(None),
        Some(libc::ENOSYS) => Err(NspawnError::Runtime(
            "rootfs probing requires Linux openat2 support".into(),
        )),
        _ => Err(NspawnError::Io(rootfs.to_path_buf(), error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubRootfsExecutor;

    #[async_trait::async_trait]
    impl RootfsExecutor for StubRootfsExecutor {
        fn route(&self) -> &'static str {
            "test"
        }

        async fn execute(&self, operation: RootfsOperation) -> Result<RootfsResult> {
            assert!(matches!(operation, RootfsOperation::ProbeOsRelease(_)));
            Ok(RootfsResult {
                present: Some(true),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn store_delegates_once_to_its_fixed_executor() {
        let store = RootfsStore {
            executor: Arc::new(StubRootfsExecutor),
        };
        let target = RootfsTarget::Machine {
            machine: MachineName::new("test").unwrap(),
        };

        assert!(store.has_os_release(&target).await.unwrap());
        assert!(format!("{store:?}").contains("test"));
    }

    #[test]
    fn target_deserialization_rejects_invalid_machine_and_mount_ids() {
        let invalid_machine = r#"{
            "operation":"probe_os_release",
            "params":{"target":{"kind":"machine","machine":"../escape"}}
        }"#;
        assert!(decode_wire_operation(invalid_machine).is_err());

        let invalid_mount = RootfsTarget::RawMount {
            machine: MachineName::new("test").unwrap(),
            mount_id: "../escape".into(),
        };
        assert!(invalid_mount.path().is_err());

        let unknown_field = r#"{
            "operation":"probe_os_release",
            "params":{"target":{"kind":"machine","machine":"test","path":"/tmp"}}
        }"#;
        assert!(decode_wire_operation(unknown_field).is_err());
    }

    #[test]
    fn mutation_deserialization_rejects_unknown_authority_fields() {
        let arbitrary_program = r#"{
            "operation":"create_user",
            "params":{
                "target":{"kind":"machine","machine":"test"},
                "username":"alice",
                "password":"secret",
                "sudoer":false,
                "shell":"/bin/bash",
                "program":"sh"
            }
        }"#;
        assert!(decode_wire_operation(arbitrary_program).is_err());

        let arbitrary_path = r#"{
            "operation":"configure_wayland",
            "params":{
                "target":{"kind":"machine","machine":"test"},
                "username":"alice",
                "shell":"/bin/bash",
                "path":"/etc/shadow"
            }
        }"#;
        assert!(decode_wire_operation(arbitrary_path).is_err());

        let x11_display = r#"{
            "operation":"configure_wayland",
            "params":{
                "target":{"kind":"machine","machine":"test"},
                "username":"alice",
                "shell":"/bin/bash",
                "display":":0"
            }
        }"#;
        assert!(decode_wire_operation(x11_display).is_err());

        let invalid_hostname = r#"{
            "operation":"configure_hostname",
            "params":{
                "target":{"kind":"machine","machine":"test"},
                "hostname":"guest_name"
            }
        }"#;
        assert!(decode_wire_operation(invalid_hostname).is_err());
    }

    fn decode_wire_operation(value: &str) -> std::result::Result<RootfsOperation, String> {
        let operation: rootfs_wire::RootfsOperation =
            serde_json::from_str(value).map_err(|error| error.to_string())?;
        RootfsOperation::try_from(operation).map_err(|error| error.to_string())
    }

    #[tokio::test]
    async fn mutation_values_are_validated_before_rootfs_state() {
        let operation = RootfsOperation::SetRootPassword(SetRootPasswordRequest {
            target: RootfsTarget::Machine {
                machine: MachineName::new("missing-test-machine").unwrap(),
            },
            password: SecretString::new("bad\npassword".into()),
        });
        let command_runner = crate::adapters::process::MockCommandRunner::new();
        let mut rootfs_runner = crate::adapters::rootfs::process::MockRootfsProcessRunner::new();
        rootfs_runner.expect_run().never();

        let result =
            execute_rootfs_operation_with_runners(operation, &command_runner, &rootfs_runner).await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[test]
    fn provisioned_target_accepts_only_exact_managed_paths() {
        let machine =
            RootfsTarget::from_provisioned_path("test", &crate::paths::machine_root("test"))
                .unwrap();
        assert!(matches!(machine, RootfsTarget::Machine { .. }));

        let image =
            RootfsTarget::from_provisioned_path("test", &crate::paths::machine_image_mount("test"))
                .unwrap();
        assert!(matches!(image, RootfsTarget::ImageMount { .. }));

        assert!(RootfsTarget::from_provisioned_path("test", Path::new("/tmp/test")).is_err());
    }

    #[tokio::test]
    async fn rootfs_directory_validation_rejects_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let link = directory.path().join("link");
        tokio::fs::create_dir(&real).await.unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(validate_optional_rootfs_directory(&link).await.is_err());
    }

    #[tokio::test]
    async fn raw_mount_operations_reject_non_raw_targets_before_running_commands() {
        let operation = RootfsOperation::UnmountManagedRaw(TargetRequest {
            target: RootfsTarget::Machine {
                machine: MachineName::new("test").unwrap(),
            },
        });
        let runner = crate::adapters::process::MockCommandRunner::new();
        let rootfs_runner = crate::adapters::rootfs::process::MockRootfsProcessRunner::new();

        let result =
            execute_rootfs_operation_with_runners(operation, &runner, &rootfs_runner).await;

        assert!(result.is_err());
    }

    #[test]
    fn rootfs_probe_resolves_absolute_symlinks_inside_the_rootfs() {
        let rootfs = tempfile::tempdir().unwrap();
        let etc = rootfs.path().join("etc");
        let static_etc = rootfs.path().join("etc/static");
        std::fs::create_dir_all(&static_etc).unwrap();
        std::fs::write(static_etc.join("os-release"), "NAME=NixOS\n").unwrap();
        std::os::unix::fs::symlink("/etc/static/os-release", etc.join("os-release")).unwrap();

        assert!(path_exists_in_root(rootfs.path(), "etc/os-release").unwrap());
    }

    #[test]
    fn rootfs_probe_does_not_follow_absolute_symlinks_onto_the_host() {
        let rootfs = tempfile::tempdir().unwrap();
        let etc = rootfs.path().join("etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", etc.join("os-release")).unwrap();

        assert!(!path_exists_in_root(rootfs.path(), "etc/os-release").unwrap());
    }

    #[test]
    fn nspawn_command_probe_requires_usr_inside_rootfs() {
        let rootfs = tempfile::tempdir().unwrap();
        assert!(!path_is_directory_in_root(rootfs.path(), "usr").unwrap());

        std::fs::write(rootfs.path().join("usr"), "not a directory").unwrap();
        assert!(!path_is_directory_in_root(rootfs.path(), "usr").unwrap());
        std::fs::remove_file(rootfs.path().join("usr")).unwrap();

        std::fs::create_dir(rootfs.path().join("usr")).unwrap();
        assert!(path_is_directory_in_root(rootfs.path(), "usr").unwrap());
    }
}

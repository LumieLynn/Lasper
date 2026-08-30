//! Durable deployment crash manifests under the composed trusted state root.

use crate::adapters::trusted_state::{StateDirectory, TrustedDirectory, TrustedStateRoot};
use crate::application::provisioning::{
    DeploymentCrashManifest, DeploymentId, DeploymentStateError, DeploymentStatePort,
};
use crate::nspawn::errors::NspawnError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const MAX_DEPLOYMENT_MANIFEST_BYTES: usize = 512 * 1024;

pub(crate) struct FilesystemDeploymentState {
    root: TrustedStateRoot,
}

pub(crate) struct ElevatedDeploymentState {
    daemon: Arc<crate::adapters::elevated::ElevatedDaemon>,
}

impl ElevatedDeploymentState {
    pub(crate) fn new(daemon: Arc<crate::adapters::elevated::ElevatedDaemon>) -> Self {
        Self { daemon }
    }
}

#[async_trait]
impl DeploymentStatePort for ElevatedDeploymentState {
    async fn create(&self, manifest: DeploymentCrashManifest) -> Result<(), DeploymentStateError> {
        self.execute(DeploymentStateOperation::Create(Box::new(manifest)))
            .await
            .map(|_| ())
    }

    async fn update(
        &self,
        expected_revision: u64,
        manifest: DeploymentCrashManifest,
    ) -> Result<(), DeploymentStateError> {
        self.execute(DeploymentStateOperation::Update(Box::new(
            UpdateDeploymentState {
                expected_revision,
                manifest,
            },
        )))
        .await
        .map(|_| ())
    }

    async fn remove(
        &self,
        deployment_id: DeploymentId,
        expected_revision: u64,
    ) -> Result<(), DeploymentStateError> {
        self.execute(DeploymentStateOperation::Remove(RemoveDeploymentState {
            deployment_id,
            expected_revision,
        }))
        .await
        .map(|_| ())
    }

    async fn unfinished(&self) -> Result<Vec<DeploymentCrashManifest>, DeploymentStateError> {
        self.execute(DeploymentStateOperation::List)
            .await
            .map(|result| result.manifests)
    }
}

impl ElevatedDeploymentState {
    async fn execute(
        &self,
        operation: DeploymentStateOperation,
    ) -> Result<DeploymentStateResult, DeploymentStateError> {
        self.daemon
            .deployment_state(operation)
            .await
            .map_err(|error| DeploymentStateError::Unavailable(error.to_string()))?
            .into_result()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "operation", content = "params", rename_all = "snake_case")]
pub(crate) enum DeploymentStateOperation {
    Create(Box<DeploymentCrashManifest>),
    Update(Box<UpdateDeploymentState>),
    Remove(RemoveDeploymentState),
    List,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateDeploymentState {
    expected_revision: u64,
    manifest: DeploymentCrashManifest,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveDeploymentState {
    deployment_id: DeploymentId,
    expected_revision: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentStateResult {
    manifests: Vec<DeploymentCrashManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<DeploymentStateError>,
}

impl DeploymentStateResult {
    pub(crate) fn failure(error: DeploymentStateError) -> Self {
        Self {
            manifests: Vec::new(),
            error: Some(error),
        }
    }

    fn into_result(self) -> Result<Self, DeploymentStateError> {
        match self.error.clone() {
            Some(error) => Err(error),
            None => Ok(self),
        }
    }
}

pub(crate) async fn execute_deployment_state_operation(
    operation: DeploymentStateOperation,
    root: TrustedStateRoot,
) -> Result<DeploymentStateResult, DeploymentStateError> {
    let state = FilesystemDeploymentState::new(root);
    match operation {
        DeploymentStateOperation::Create(manifest) => {
            state.create(*manifest).await?;
            Ok(DeploymentStateResult::default())
        }
        DeploymentStateOperation::Update(request) => {
            state
                .update(request.expected_revision, request.manifest)
                .await?;
            Ok(DeploymentStateResult::default())
        }
        DeploymentStateOperation::Remove(request) => {
            state
                .remove(request.deployment_id, request.expected_revision)
                .await?;
            Ok(DeploymentStateResult::default())
        }
        DeploymentStateOperation::List => Ok(DeploymentStateResult {
            manifests: state.unfinished().await?,
            error: None,
        }),
    }
}

impl FilesystemDeploymentState {
    pub(crate) fn new(root: TrustedStateRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl DeploymentStatePort for FilesystemDeploymentState {
    async fn create(&self, manifest: DeploymentCrashManifest) -> Result<(), DeploymentStateError> {
        let root = self.root.clone();
        run_blocking(move || {
            manifest.validate()?;
            let directory = deployment_directory(&root)?;
            let name = manifest_name(manifest.deployment_id);
            let bytes = serialize_manifest(&manifest)?;
            directory
                .with_exclusive_lock(&name, || {
                    if read_manifest(&directory, &name)
                        .map_err(state_error_to_nspawn)?
                        .is_some()
                    {
                        return Err(state_conflict(format!(
                            "deployment {} already has a crash manifest",
                            manifest.deployment_id
                        )));
                    }
                    directory.write_atomic(&name, &bytes, 0o600)
                })
                .map_err(map_state_error)
        })
        .await
    }

    async fn update(
        &self,
        expected_revision: u64,
        manifest: DeploymentCrashManifest,
    ) -> Result<(), DeploymentStateError> {
        let root = self.root.clone();
        run_blocking(move || {
            manifest.validate()?;
            if manifest.revision != expected_revision.saturating_add(1) {
                return Err(DeploymentStateError::Invalid(format!(
                    "manifest revision {} does not follow expected revision {expected_revision}",
                    manifest.revision
                )));
            }
            let directory = deployment_directory(&root)?;
            let name = manifest_name(manifest.deployment_id);
            let bytes = serialize_manifest(&manifest)?;
            directory
                .with_exclusive_lock(&name, || {
                    let current = read_manifest(&directory, &name)
                        .map_err(state_error_to_nspawn)?
                        .ok_or_else(|| {
                            state_conflict(format!(
                                "deployment {} crash manifest is missing",
                                manifest.deployment_id
                            ))
                        })?;
                    if current.deployment_id != manifest.deployment_id
                        || current.revision != expected_revision
                    {
                        return Err(state_conflict(format!(
                        "deployment {} crash manifest changed from revision {expected_revision}",
                        manifest.deployment_id
                    )));
                    }
                    directory.write_atomic(&name, &bytes, 0o600)
                })
                .map_err(map_state_error)
        })
        .await
    }

    async fn remove(
        &self,
        deployment_id: DeploymentId,
        expected_revision: u64,
    ) -> Result<(), DeploymentStateError> {
        let root = self.root.clone();
        run_blocking(move || {
            let directory = deployment_directory(&root)?;
            let name = manifest_name(deployment_id);
            directory
                .with_exclusive_lock_and_cleanup(&name, || {
                    let current = read_manifest(&directory, &name)
                        .map_err(state_error_to_nspawn)?
                        .ok_or_else(|| {
                        state_conflict(format!(
                            "deployment {deployment_id} crash manifest is missing"
                        ))
                        })?;
                    if current.deployment_id != deployment_id
                        || current.revision != expected_revision
                    {
                        return Err(state_conflict(format!(
                            "deployment {deployment_id} crash manifest changed from revision {expected_revision}"
                        )));
                    }
                    directory.remove_unlocked(&name)
                })
                .map_err(map_state_error)
        })
        .await
    }

    async fn unfinished(&self) -> Result<Vec<DeploymentCrashManifest>, DeploymentStateError> {
        let root = self.root.clone();
        run_blocking(move || {
            let directory = deployment_directory(&root)?;
            let mut manifests = Vec::new();
            for name in directory.entry_names().map_err(map_state_error)? {
                if !name.starts_with("deployment-") || !name.ends_with(".json") {
                    continue;
                }
                let manifest = read_manifest(&directory, &name)?.ok_or_else(|| {
                    DeploymentStateError::Unavailable(format!(
                        "deployment manifest disappeared while listing: {name}"
                    ))
                })?;
                if name != manifest_name(manifest.deployment_id) {
                    return Err(DeploymentStateError::Invalid(format!(
                        "deployment manifest filename does not match its typed id: {name}"
                    )));
                }
                manifests.push(manifest);
            }
            manifests.sort_by_key(|manifest| manifest.deployment_id.as_uuid());
            Ok(manifests)
        })
        .await
    }
}

async fn run_blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, DeploymentStateError> + Send + 'static,
) -> Result<T, DeploymentStateError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            DeploymentStateError::Unavailable(format!(
                "deployment state worker did not complete: {error}"
            ))
        })?
}

fn deployment_directory(root: &TrustedStateRoot) -> Result<TrustedDirectory, DeploymentStateError> {
    root.directory(StateDirectory::Deployments)
        .map_err(map_state_error)
}

fn manifest_name(id: DeploymentId) -> String {
    format!("deployment-{id}.json")
}

fn serialize_manifest(manifest: &DeploymentCrashManifest) -> Result<Vec<u8>, DeploymentStateError> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        DeploymentStateError::Invalid(format!("could not serialize deployment manifest: {error}"))
    })?;
    if bytes.len() > MAX_DEPLOYMENT_MANIFEST_BYTES {
        return Err(DeploymentStateError::Invalid(format!(
            "deployment manifest exceeds {MAX_DEPLOYMENT_MANIFEST_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

fn read_manifest(
    directory: &TrustedDirectory,
    name: &str,
) -> Result<Option<DeploymentCrashManifest>, DeploymentStateError> {
    let Some(file) = directory
        .read_bounded(name, MAX_DEPLOYMENT_MANIFEST_BYTES)
        .map_err(map_state_error)?
    else {
        return Ok(None);
    };
    if file.uid != directory.expected_uid() || file.mode != 0o600 {
        return Err(DeploymentStateError::Invalid(format!(
            "deployment manifest has unsafe ownership or mode: {name}"
        )));
    }
    let manifest: DeploymentCrashManifest =
        serde_json::from_slice(&file.bytes).map_err(|error| {
            DeploymentStateError::Invalid(format!(
                "deployment manifest is not valid typed JSON ({name}): {error}"
            ))
        })?;
    manifest.validate()?;
    Ok(Some(manifest))
}

fn state_conflict(message: String) -> NspawnError {
    NspawnError::Runtime(format!("deployment state conflict: {message}"))
}

fn state_error_to_nspawn(error: DeploymentStateError) -> NspawnError {
    match error {
        DeploymentStateError::Invalid(message) => NspawnError::Validation(message),
        DeploymentStateError::Conflict(message) => state_conflict(message),
        DeploymentStateError::Unavailable(message) => NspawnError::Runtime(message),
    }
}

fn map_state_error(error: NspawnError) -> DeploymentStateError {
    match error {
        NspawnError::Validation(message) => DeploymentStateError::Invalid(message),
        NspawnError::Runtime(message) if message.starts_with("deployment state conflict: ") => {
            DeploymentStateError::Conflict(
                message
                    .trim_start_matches("deployment state conflict: ")
                    .to_owned(),
            )
        }
        error => DeploymentStateError::Unavailable(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::provisioning::MachineProvisioningConfig;
    use crate::application::provisioning::{
        DeploymentManifestState, DeploymentPlan, DeploymentRequest, DeploymentSource,
        DeploymentStage, DeploymentStorage, ResourceLedger,
    };

    fn plan() -> DeploymentPlan {
        DeploymentPlan::build(DeploymentRequest {
            config: MachineProvisioningConfig {
                name: "test".into(),
                ..Default::default()
            },
            source: DeploymentSource::Copy {
                source_name: "base".into(),
            },
            storage: DeploymentStorage::Directory,
            nvidia_profile: None,
            wayland: Vec::new(),
            allow_unsafe_remote_tar: false,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn manifest_create_update_list_and_remove_are_revision_checked() {
        let temporary = tempfile::tempdir().unwrap();
        let state = FilesystemDeploymentState::new(TrustedStateRoot::for_test(
            temporary.path().join("lasper"),
        ));
        let plan = plan();
        let id = DeploymentId::from_u128(7);
        let manifest = DeploymentCrashManifest::prepared(id, &plan);
        state.create(manifest.clone()).await.unwrap();
        assert!(matches!(
            state.create(manifest.clone()).await,
            Err(DeploymentStateError::Conflict(_))
        ));

        let mut next = manifest.clone();
        next.revision = 2;
        next.state = DeploymentManifestState::Committed {
            stage: DeploymentStage::StoragePreparation,
        };
        next.committed_ledger = ResourceLedger::default().snapshot();
        state.update(1, next.clone()).await.unwrap();
        assert!(matches!(
            state.update(1, next.clone()).await,
            Err(DeploymentStateError::Invalid(_) | DeploymentStateError::Conflict(_))
        ));

        assert_eq!(state.unfinished().await.unwrap(), vec![next.clone()]);
        state.remove(id, 2).await.unwrap();
        assert!(state.unfinished().await.unwrap().is_empty());
        assert!(!temporary
            .path()
            .join(format!("lasper/deployments/.{}.lock", manifest_name(id)))
            .exists());
    }

    #[tokio::test]
    async fn manifest_symlink_is_never_followed_or_replaced() {
        let temporary = tempfile::tempdir().unwrap();
        let root = TrustedStateRoot::for_test(temporary.path().join("lasper"));
        root.directory(StateDirectory::Deployments).unwrap();
        let state = FilesystemDeploymentState::new(root);
        let id = DeploymentId::from_u128(8);
        let name = manifest_name(id);
        let outside = temporary.path().join("outside");
        std::fs::write(&outside, "unchanged").unwrap();
        std::os::unix::fs::symlink(
            &outside,
            temporary.path().join("lasper/deployments").join(&name),
        )
        .unwrap();

        assert!(state
            .create(DeploymentCrashManifest::prepared(id, &plan()))
            .await
            .is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "unchanged");
    }

    #[test]
    fn rpc_result_preserves_typed_state_failures() {
        let result =
            DeploymentStateResult::failure(DeploymentStateError::Conflict("stale revision".into()));
        let encoded = serde_json::to_vec(&result).unwrap();
        let decoded: DeploymentStateResult = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            decoded.into_result().unwrap_err(),
            DeploymentStateError::Conflict("stale revision".into())
        );
    }

    #[test]
    fn deployment_state_wire_never_accepts_a_caller_selected_root() {
        let request = serde_json::json!({
            "operation": "list",
            "params": {"root": "/tmp/attacker"}
        });
        assert!(serde_json::from_value::<DeploymentStateOperation>(request).is_err());
    }
}

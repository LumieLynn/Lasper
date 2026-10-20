//! Deployment state, job coordination, and submission-stream RPC proxies.

use crate::adapters::elevated::fd_channel::receive_fd_response;
use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::provisioning::engine::image_operation::TarRuntimeAssessment;
use crate::adapters::provisioning::state::{DeploymentStateOperation, DeploymentStateResult};
use crate::ipc::protocol::deployment::{
    DeploymentJobRequest, DeploymentJobSnapshot, DeploymentSubmissionRequest,
    DeploymentSubmissionSnapshot, ProbeDeploymentRecoveryRequest, ProbeDeploymentRecoveryResult,
    ReleaseUnresolvedDeploymentRequest, SubmitDeploymentParams,
};
use crate::ipc::protocol::FdOperation;
use sendfd::SendWithFd;
use std::os::fd::{AsRawFd, OwnedFd};

impl ElevatedDaemon {
    pub(crate) async fn deployment_state(
        &self,
        operation: DeploymentStateOperation,
    ) -> std::io::Result<DeploymentStateResult> {
        let params = serde_json::to_value(operation)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc.call("deployment_state", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn submit_deployment(
        &self,
        params: SubmitDeploymentParams,
        artifact_source: Option<std::fs::File>,
    ) -> std::io::Result<OwnedFd> {
        let socket = self
            .fd_channel
            .open(FdOperation::SubmitDeployment(Box::new(params)))
            .await?;
        tokio::task::spawn_blocking(move || {
            receive_deployment_stream(&socket, artifact_source.as_ref())
        })
        .await?
    }

    pub(crate) async fn deployment_status(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
    ) -> std::io::Result<Option<DeploymentJobSnapshot>> {
        let params = serde_json::to_value(DeploymentJobRequest { deployment_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc.call("deployment_status", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn resolve_deployment_submission(
        &self,
        request_id: crate::application::provisioning::DeploymentRequestId,
    ) -> std::io::Result<Option<DeploymentSubmissionSnapshot>> {
        let params = serde_json::to_value(DeploymentSubmissionRequest { request_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self
            .rpc
            .call("resolve_deployment_submission", params)
            .await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn acknowledge_deployment_submission(
        &self,
        request_id: crate::application::provisioning::DeploymentRequestId,
    ) -> std::io::Result<()> {
        let params = serde_json::to_value(DeploymentSubmissionRequest { request_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        self.rpc
            .call("acknowledge_deployment_submission", params)
            .await?;
        Ok(())
    }

    pub(crate) async fn cancel_deployment(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
    ) -> std::io::Result<DeploymentJobSnapshot> {
        let params = serde_json::to_value(DeploymentJobRequest { deployment_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc.call("cancel_deployment", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn acknowledge_deployment(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
    ) -> std::io::Result<()> {
        let params = serde_json::to_value(DeploymentJobRequest { deployment_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        self.rpc.call("acknowledge_deployment", params).await?;
        Ok(())
    }

    pub(crate) async fn probe_deployment_recovery(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
        expected_revision: u64,
    ) -> std::io::Result<Vec<crate::application::provisioning::DeploymentRecoveryObservation>> {
        let params = serde_json::to_value(ProbeDeploymentRecoveryRequest {
            deployment_id,
            expected_revision,
        })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result: ProbeDeploymentRecoveryResult =
            serde_json::from_value(self.rpc.call("probe_deployment_recovery", params).await?)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if result.deployment_id != deployment_id || result.manifest_revision != expected_revision {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "daemon recovery probe returned a mismatched deployment revision",
            ));
        }
        Ok(result.observations)
    }

    pub(crate) async fn reconcile_deployment(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
    ) -> std::io::Result<DeploymentJobSnapshot> {
        let params = serde_json::to_value(DeploymentJobRequest { deployment_id })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self.rpc.call("reconcile_deployment", params).await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn release_unresolved_deployment(
        &self,
        deployment_id: crate::application::provisioning::DeploymentId,
        confirmed: bool,
    ) -> std::io::Result<DeploymentJobSnapshot> {
        let params = serde_json::to_value(ReleaseUnresolvedDeploymentRequest {
            deployment_id,
            confirmed,
        })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let result = self
            .rpc
            .call("release_unresolved_deployment", params)
            .await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) async fn assess_tar_runtime(&self) -> std::io::Result<TarRuntimeAssessment> {
        let result = self
            .rpc
            .call("assess_tar_runtime", serde_json::json!({}))
            .await?;
        serde_json::from_value(result)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }
}

fn receive_deployment_stream(
    socket: &std::os::unix::net::UnixStream,
    artifact_source: Option<&std::fs::File>,
) -> std::io::Result<OwnedFd> {
    let handoff_timeout = std::time::Duration::from_secs(10);
    socket.set_read_timeout(Some(handoff_timeout))?;
    socket.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    if let Some(source) = artifact_source {
        use std::io::BufRead;

        let mut reader = std::io::BufReader::new(socket.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim() != "artifact-ready" {
            return Err(std::io::Error::other(format!(
                "daemon refused deployment artifact fd: {}",
                line.trim()
            )));
        }
        socket.send_with_fd(b"artifact", &[source.as_raw_fd()])?;
    }

    let (message, fds) = receive_fd_response(socket, "deployment stream", handoff_timeout)?;
    if fds.len() != 1 {
        return Err(std::io::Error::other(format!(
            "daemon deployment submission returned {} fds: {}",
            fds.len(),
            message.trim()
        )));
    }
    if message.trim() != "ok" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "daemon returned an invalid deployment stream response: {}",
                message.trim()
            ),
        ));
    }
    Ok(fds.into_iter().next().expect("FD count checked"))
}

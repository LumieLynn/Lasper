//! Daemon-owned provisioning job registry and dedicated event stream.

use super::deployment_protocol::{
    DeploymentClaimState, DeploymentJobSnapshot, DeploymentStreamFrame,
    DeploymentSubmissionSnapshot, DeploymentSubmissionStatus, SubmitDeploymentParams,
    MAX_DEPLOYMENT_STREAM_FRAME_BYTES,
};
use super::session_server::DaemonServerState;
use crate::adapters::provisioning::direct::DirectProvisioningExecutor;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::operations::ResourceReservation;
use crate::application::provisioning::{
    deployment_job_channel, run_deployment_executor, DeploymentCancellation, DeploymentExecutor,
    DeploymentId, DeploymentPlan, DeploymentRequestId, DeploymentStatus, DeploymentSubmission,
    PlanFingerprint,
};
use crate::application::{OperationRegistry, ResourceClaim};
use sendfd::{RecvWithFd, SendWithFd};
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

const MAX_DEPLOYMENT_RECORDS: usize = 64;

#[derive(Default)]
pub(super) struct DeploymentRegistry {
    inner: parking_lot::Mutex<DeploymentRegistryInner>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct DeploymentRegistryInner {
    jobs: HashMap<DeploymentId, DeploymentRecord>,
    submissions: HashMap<DeploymentRequestId, DeploymentSubmissionRecord>,
    next_sequence: u64,
}

struct DeploymentRecord {
    request_id: DeploymentRequestId,
    revision: u64,
    status: DeploymentStatus,
    claim: DeploymentClaimState,
    cancellation: DeploymentCancellation,
    cancellation_requested: bool,
    reservation: Option<ResourceReservation>,
    acknowledged: bool,
    sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeploymentSubmissionIdentity {
    deployment_id: DeploymentId,
    plan: PlanFingerprint,
}

struct DeploymentSubmissionRecord {
    identity: DeploymentSubmissionIdentity,
    status: DeploymentSubmissionStatus,
    acknowledged: bool,
    sequence: u64,
}

impl DeploymentRegistry {
    fn reserve_submission(
        &self,
        request_id: DeploymentRequestId,
        deployment_id: DeploymentId,
        plan: PlanFingerprint,
    ) -> std::io::Result<()> {
        let mut inner = self.inner.lock();
        let identity = DeploymentSubmissionIdentity {
            deployment_id,
            plan,
        };
        if let Some(existing) = inner.submissions.get(&request_id) {
            let message = if existing.identity == identity {
                format!(
                    "deployment submission {request_id} already exists; resolve it instead of resending secrets"
                )
            } else {
                format!("deployment submission {request_id} has a different identity")
            };
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                message,
            ));
        }
        while inner.submissions.len() >= MAX_DEPLOYMENT_RECORDS {
            if !evict_one(&mut inner) {
                return Err(std::io::Error::other(
                    "daemon deployment submission registry is at capacity",
                ));
            }
        }
        let sequence = next_sequence(&mut inner);
        inner.submissions.insert(
            request_id,
            DeploymentSubmissionRecord {
                identity,
                status: DeploymentSubmissionStatus::Pending,
                acknowledged: false,
                sequence,
            },
        );
        Ok(())
    }

    fn reject_submission(&self, request_id: DeploymentRequestId, message: String) {
        let mut inner = self.inner.lock();
        let sequence = next_sequence(&mut inner);
        if let Some(record) = inner.submissions.get_mut(&request_id) {
            record.status = DeploymentSubmissionStatus::Rejected { message };
            record.sequence = sequence;
        }
        self.changed.notify_waiters();
    }

    fn register(
        &self,
        request_id: DeploymentRequestId,
        deployment_id: DeploymentId,
        claims: Vec<ResourceClaim>,
        cancellation: DeploymentCancellation,
        operations: &Arc<OperationRegistry>,
    ) -> std::io::Result<()> {
        let mut inner = self.inner.lock();
        let submission = inner.submissions.get(&request_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("deployment submission {request_id} is not registered"),
            )
        })?;
        if submission.status != DeploymentSubmissionStatus::Pending
            || submission.identity.deployment_id != deployment_id
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("deployment submission {request_id} is not pending for {deployment_id}"),
            ));
        }
        if inner.jobs.len() >= MAX_DEPLOYMENT_RECORDS {
            return Err(std::io::Error::other(
                "daemon deployment registry is at capacity",
            ));
        }
        if inner.jobs.contains_key(&deployment_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("deployment {deployment_id} already exists"),
            ));
        }
        let reservation = operations.reserve(claims).map_err(|conflict| {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("deployment resource is busy: {:?}", conflict.key),
            )
        })?;
        let sequence = next_sequence(&mut inner);
        inner.jobs.insert(
            deployment_id,
            DeploymentRecord {
                request_id,
                revision: 1,
                status: DeploymentStatus::Running,
                claim: DeploymentClaimState::Held,
                cancellation,
                cancellation_requested: false,
                reservation: Some(reservation),
                acknowledged: false,
                sequence,
            },
        );
        let submission = inner
            .submissions
            .get_mut(&request_id)
            .expect("pending submission remains registered");
        submission.status = DeploymentSubmissionStatus::Accepted { deployment_id };
        submission.sequence = sequence;
        Ok(())
    }

    fn update_status(
        &self,
        deployment_id: DeploymentId,
        status: DeploymentStatus,
    ) -> Option<DeploymentJobSnapshot> {
        let mut inner = self.inner.lock();
        let sequence = next_sequence(&mut inner);
        let record = inner.jobs.get_mut(&deployment_id)?;
        let claim = if matches!(status, DeploymentStatus::ReconciliationRequired(_)) {
            DeploymentClaimState::ReconciliationRequired
        } else if status.is_finished() {
            DeploymentClaimState::Released
        } else {
            DeploymentClaimState::Held
        };
        if status != record.status || claim != record.claim {
            record.revision = record.revision.saturating_add(1);
            record.status = status;
            record.claim = claim;
            record.sequence = sequence;
            if claim == DeploymentClaimState::Released {
                record.reservation.take();
            }
        }
        let snapshot = snapshot(deployment_id, record);
        drop(inner);
        self.changed.notify_waiters();
        Some(snapshot)
    }

    pub(super) fn snapshot(
        &self,
        deployment_id: DeploymentId,
    ) -> std::io::Result<DeploymentJobSnapshot> {
        let inner = self.inner.lock();
        let record = inner.jobs.get(&deployment_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("deployment {deployment_id} is not registered"),
            )
        })?;
        Ok(snapshot(deployment_id, record))
    }

    pub(super) fn resolve_submission(
        &self,
        request_id: DeploymentRequestId,
    ) -> std::io::Result<DeploymentSubmissionSnapshot> {
        let inner = self.inner.lock();
        let record = inner.submissions.get(&request_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("deployment submission {request_id} is not registered"),
            )
        })?;
        Ok(DeploymentSubmissionSnapshot {
            request_id,
            status: record.status.clone(),
            acknowledged: record.acknowledged,
        })
    }

    pub(super) fn acknowledge_submission(
        &self,
        request_id: DeploymentRequestId,
    ) -> std::io::Result<()> {
        let mut inner = self.inner.lock();
        let sequence = next_sequence(&mut inner);
        let record = inner.submissions.get_mut(&request_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("deployment submission {request_id} is not registered"),
            )
        })?;
        if record.status == DeploymentSubmissionStatus::Pending {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("deployment submission {request_id} is still pending"),
            ));
        }
        record.acknowledged = true;
        record.sequence = sequence;
        Ok(())
    }

    pub(super) fn cancel(
        &self,
        deployment_id: DeploymentId,
    ) -> std::io::Result<DeploymentJobSnapshot> {
        let mut inner = self.inner.lock();
        let sequence = next_sequence(&mut inner);
        let record = inner.jobs.get_mut(&deployment_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("deployment {deployment_id} is not registered"),
            )
        })?;
        if record.status.is_finished() {
            return Ok(snapshot(deployment_id, record));
        }
        if !record.cancellation_requested {
            record.cancellation_requested = true;
            record.revision = record.revision.saturating_add(1);
            record.sequence = sequence;
        }
        let cancellation = record.cancellation.clone();
        let snapshot = snapshot(deployment_id, record);
        drop(inner);
        cancellation.request();
        self.changed.notify_waiters();
        Ok(snapshot)
    }

    pub(super) fn acknowledge(&self, deployment_id: DeploymentId) -> std::io::Result<()> {
        let mut inner = self.inner.lock();
        let request_id = {
            let record = inner.jobs.get(&deployment_id).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("deployment {deployment_id} is not registered"),
                )
            })?;
            if !record.status.is_finished() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!("deployment {deployment_id} is still running"),
                ));
            }
            if record.claim == DeploymentClaimState::ReconciliationRequired {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!("deployment {deployment_id} still requires reconciliation"),
                ));
            }
            record.request_id
        };
        let submission = inner
            .submissions
            .get(&request_id)
            .expect("job retains its submission record");
        if !submission.acknowledged {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("deployment submission {} is not acknowledged", request_id),
            ));
        }
        let sequence = next_sequence(&mut inner);
        let record = inner
            .jobs
            .get_mut(&deployment_id)
            .expect("validated deployment remains registered");
        record.acknowledged = true;
        record.sequence = sequence;
        Ok(())
    }

    pub(super) fn release_unresolved(
        &self,
        deployment_id: DeploymentId,
        confirmed: bool,
    ) -> std::io::Result<DeploymentJobSnapshot> {
        if !confirmed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "releasing an unresolved deployment requires explicit confirmation",
            ));
        }
        let mut inner = self.inner.lock();
        let sequence = next_sequence(&mut inner);
        let record = inner.jobs.get_mut(&deployment_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("deployment {deployment_id} is not registered"),
            )
        })?;
        if !record.status.is_finished()
            || record.claim != DeploymentClaimState::ReconciliationRequired
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("deployment {deployment_id} is not eligible for unresolved release"),
            ));
        }
        record.reservation.take();
        record.claim = DeploymentClaimState::ReleasedUnresolved;
        record.revision = record.revision.saturating_add(1);
        record.sequence = sequence;
        let snapshot = snapshot(deployment_id, record);
        drop(inner);
        self.changed.notify_waiters();
        log::warn!(
            "[AUDIT] Explicitly released unresolved coordination claim for deployment {deployment_id}; historical outcome remains unknown"
        );
        Ok(snapshot)
    }

    pub(super) fn reconcile(
        &self,
        deployment_id: DeploymentId,
        manifest_present: bool,
    ) -> std::io::Result<DeploymentJobSnapshot> {
        let mut inner = self.inner.lock();
        let sequence = next_sequence(&mut inner);
        let record = inner.jobs.get_mut(&deployment_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("deployment {deployment_id} is not registered"),
            )
        })?;
        if !matches!(record.status, DeploymentStatus::ReconciliationRequired(_)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("deployment {deployment_id} does not require reconciliation"),
            ));
        }
        match record.claim {
            DeploymentClaimState::Reconciled => return Ok(snapshot(deployment_id, record)),
            DeploymentClaimState::ReconciliationRequired => {}
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("deployment {deployment_id} cannot be reconciled in its current state"),
                ));
            }
        }
        if !manifest_present {
            record.reservation.take();
            record.claim = DeploymentClaimState::Reconciled;
            record.revision = record.revision.saturating_add(1);
            record.sequence = sequence;
        }
        let snapshot = snapshot(deployment_id, record);
        drop(inner);
        self.changed.notify_waiters();
        Ok(snapshot)
    }

    fn cancel_all(&self) {
        let cancellations = self
            .inner
            .lock()
            .jobs
            .values()
            .map(|record| record.cancellation.clone())
            .collect::<Vec<_>>();
        for cancellation in cancellations {
            cancellation.request();
        }
    }

    pub(super) async fn cancel_all_and_wait(&self, timeout: std::time::Duration) -> bool {
        self.cancel_all();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let changed = self.changed.notified();
            let active = self
                .inner
                .lock()
                .jobs
                .values()
                .any(|record| !record.status.is_finished());
            if !active {
                return true;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return false;
            }
        }
    }
}

fn next_sequence(inner: &mut DeploymentRegistryInner) -> u64 {
    inner.next_sequence = inner.next_sequence.saturating_add(1);
    inner.next_sequence
}

fn snapshot(deployment_id: DeploymentId, record: &DeploymentRecord) -> DeploymentJobSnapshot {
    DeploymentJobSnapshot {
        deployment_id,
        revision: record.revision,
        status: record.status.clone(),
        cancellation_requested: record.cancellation_requested,
        claim: record.claim,
    }
}

fn evict_one(inner: &mut DeploymentRegistryInner) -> bool {
    let request_id = inner
        .submissions
        .iter()
        .filter(|(_, submission)| {
            if !submission.acknowledged {
                return false;
            }
            match submission.status {
                DeploymentSubmissionStatus::Rejected { .. } => true,
                DeploymentSubmissionStatus::Accepted { deployment_id } => {
                    inner.jobs.get(&deployment_id).is_some_and(|job| {
                        job.acknowledged
                            && matches!(
                                job.claim,
                                DeploymentClaimState::Released
                                    | DeploymentClaimState::Reconciled
                                    | DeploymentClaimState::ReleasedUnresolved
                            )
                    })
                }
                DeploymentSubmissionStatus::Pending => false,
            }
        })
        .min_by_key(|(_, submission)| submission.sequence)
        .map(|(request_id, _)| *request_id);
    let Some(request_id) = request_id else {
        return false;
    };
    if let Some(DeploymentSubmissionRecord {
        status: DeploymentSubmissionStatus::Accepted { deployment_id },
        ..
    }) = inner.submissions.remove(&request_id)
    {
        inner.jobs.remove(&deployment_id);
    }
    true
}

pub(super) async fn submit(
    stream: &mut std::os::unix::net::UnixStream,
    params: SubmitDeploymentParams,
    server_state: Arc<DaemonServerState>,
    state_root: TrustedStateRoot,
) {
    let SubmitDeploymentParams {
        request_id,
        deployment_id,
        request,
        secrets,
    } = params;
    let plan = match DeploymentPlan::build(request.clone()) {
        Ok(plan) => plan,
        Err(error) => {
            let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
            return;
        }
    };
    if let Err(error) = server_state.deployments.reserve_submission(
        request_id,
        deployment_id,
        plan.fingerprint().clone(),
    ) {
        let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
        return;
    }
    let expects_artifact = matches!(
        &request.source,
        crate::application::provisioning::DeploymentSource::Artifact(_)
    );
    let artifact_source = if expects_artifact {
        match receive_artifact_source(stream).await {
            Ok(source) => Some(source),
            Err(error) => {
                server_state
                    .deployments
                    .reject_submission(request_id, error.to_string());
                let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
                return;
            }
        }
    } else {
        None
    };

    let submission = DeploymentSubmission::new(request, secrets.into_secrets());
    if let Err(error) = submission.validate_secrets() {
        server_state
            .deployments
            .reject_submission(request_id, error.to_string());
        let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
        return;
    }
    let (_, secrets) = submission.into_parts();

    let (reader, writer) = match stream_pipe() {
        Ok(pipe) => pipe,
        Err(error) => {
            server_state
                .deployments
                .reject_submission(request_id, error.to_string());
            let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
            return;
        }
    };
    let (handle, context) = deployment_job_channel(deployment_id);
    if let Err(error) = server_state.deployments.register(
        request_id,
        deployment_id,
        plan.resource_claims(),
        handle.cancellation(),
        &server_state.operations,
    ) {
        server_state
            .deployments
            .reject_submission(request_id, error.to_string());
        let _ = stream.send_with_fd(error.to_string().as_bytes(), &[]);
        return;
    }

    let executor: Arc<dyn DeploymentExecutor> = Arc::new(DirectProvisioningExecutor::for_daemon(
        state_root,
        artifact_source,
    ));
    let terminal_status = context.status_sender();
    tokio::spawn(async move {
        let terminal = run_deployment_executor(executor, plan, secrets, context).await;
        terminal_status.send_replace(terminal.status);
    });
    tokio::spawn(forward_job_stream(
        deployment_id,
        handle,
        writer,
        Arc::clone(&server_state),
    ));
    if let Err(error) = stream.send_with_fd(b"ok", &[reader.as_raw_fd()]) {
        log::error!(
            "Daemon: deployment {deployment_id} was accepted but its stream fd could not be delivered: {error}"
        );
    }
    drop(reader);
}

async fn receive_artifact_source(
    stream: &std::os::unix::net::UnixStream,
) -> std::io::Result<std::fs::File> {
    let stream = stream.try_clone()?;
    tokio::task::spawn_blocking(move || receive_artifact_source_blocking(stream))
        .await
        .map_err(|error| std::io::Error::other(format!("artifact fd worker failed: {error}")))?
}

fn receive_artifact_source_blocking(
    mut stream: std::os::unix::net::UnixStream,
) -> std::io::Result<std::fs::File> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    let received: std::io::Result<([u8; 16], [RawFd; 1], usize, usize)> = (|| {
        stream.write_all(b"artifact-ready\n")?;
        let mut marker = [0u8; 16];
        let mut fds = [-1 as RawFd; 1];
        let (read, count) = stream.recv_with_fd(&mut marker, &mut fds)?;
        Ok((marker, fds, read, count))
    })();
    let read_reset = stream.set_read_timeout(None);
    let write_reset = stream.set_write_timeout(None);
    let (marker, fds, read, count) = received?;
    if let Some(error) = read_reset.err().or_else(|| write_reset.err()) {
        for fd in fds.into_iter().take(count).filter(|fd| *fd >= 0) {
            unsafe { libc::close(fd) };
        }
        return Err(error);
    }
    if count != 1 || &marker[..read] != b"artifact" {
        for fd in fds.into_iter().take(count).filter(|fd| *fd >= 0) {
            unsafe { libc::close(fd) };
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "deployment artifact transfer requires exactly one source fd",
        ));
    }
    let source = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    crate::adapters::storage::image_ops::validate_import_source(&source)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(source)
}

async fn forward_job_stream(
    deployment_id: DeploymentId,
    handle: crate::application::provisioning::DeploymentJobHandle,
    writer: std::fs::File,
    server_state: Arc<DaemonServerState>,
) {
    use tokio::io::AsyncWriteExt;

    let (mut events, mut status) = handle.into_streams();
    let mut writer = tokio::io::BufWriter::new(tokio::fs::File::from_std(writer));
    let mut stream_open = true;
    loop {
        let current = status.borrow().clone();
        if current.is_finished() {
            while let Ok(event) = events.try_recv() {
                if stream_open {
                    stream_open =
                        write_frame(&mut writer, DeploymentStreamFrame::Event(event)).await;
                }
            }
            let snapshot = server_state
                .deployments
                .update_status(deployment_id, current)
                .expect("registered deployment remains until terminal acknowledgment");
            if stream_open {
                let _ = write_frame(&mut writer, DeploymentStreamFrame::Snapshot(snapshot)).await;
            }
            let _ = writer.flush().await;
            return;
        }

        tokio::select! {
            event = events.recv() => {
                if let Some(event) = event {
                    if stream_open {
                        stream_open = write_frame(
                            &mut writer,
                            DeploymentStreamFrame::Event(event),
                        ).await;
                    }
                }
            }
            changed = status.changed() => {
                if changed.is_err() {
                    let failed = DeploymentStatus::ReconciliationRequired(
                        "daemon deployment status channel closed before a terminal result".into(),
                    );
                    let snapshot = server_state
                        .deployments
                        .update_status(deployment_id, failed)
                        .expect("registered deployment remains while its stream is active");
                    if stream_open {
                        let _ = write_frame(
                            &mut writer,
                            DeploymentStreamFrame::Snapshot(snapshot),
                        ).await;
                    }
                    return;
                }
                let current = status.borrow().clone();
                let snapshot = server_state
                    .deployments
                    .update_status(deployment_id, current.clone())
                    .expect("registered deployment remains while its stream is active");
                if !current.is_finished() && stream_open {
                    stream_open = write_frame(
                        &mut writer,
                        DeploymentStreamFrame::Snapshot(snapshot),
                    ).await;
                }
            }
        }
    }
}

async fn write_frame(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    frame: DeploymentStreamFrame,
) -> bool {
    use tokio::io::AsyncWriteExt;

    let mut bytes = match serde_json::to_vec(&frame) {
        Ok(bytes) => bytes,
        Err(error) => {
            log::error!("Daemon: serialize deployment stream frame failed: {error}");
            return false;
        }
    };
    if bytes.len().saturating_add(1) > MAX_DEPLOYMENT_STREAM_FRAME_BYTES {
        bytes = serde_json::to_vec(&DeploymentStreamFrame::Event(
            crate::application::provisioning::DeploymentEvent::Line(
                "WARNING: An oversized deployment output line was omitted.".into(),
            ),
        ))
        .expect("bounded fallback deployment frame serializes");
    }
    bytes.push(b'\n');
    if writer.write_all(&bytes).await.is_err() {
        return false;
    }
    // The client consumes one frame at a time from the pipe. Flush here so
    // progress output is visible while the deployment is still running,
    // rather than being held by the BufWriter until the terminal snapshot.
    writer.flush().await.is_ok()
}

fn stream_pipe() -> std::io::Result<(OwnedFd, std::fs::File)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Ok((reader, writer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::provisioning::{
        DeploymentEvent, DeploymentRequest, DeploymentSource, DeploymentStorage,
    };
    use crate::domain::machine::MachineName;
    use crate::nspawn::models::ContainerConfig;
    use tokio::io::AsyncBufReadExt;

    fn plan(target: &str) -> DeploymentPlan {
        DeploymentPlan::build(DeploymentRequest {
            config: ContainerConfig {
                name: target.into(),
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

    fn register_job(
        registry: &DeploymentRegistry,
        operations: &Arc<OperationRegistry>,
        request_id: DeploymentRequestId,
        deployment_id: DeploymentId,
        target: &MachineName,
    ) -> std::io::Result<()> {
        let plan = plan(target.as_str());
        registry.reserve_submission(request_id, deployment_id, plan.fingerprint().clone())?;
        registry.register(
            request_id,
            deployment_id,
            vec![ResourceClaim::exclusive(
                crate::application::ResourceKey::for_machine(target),
            )],
            DeploymentCancellation::default(),
            operations,
        )
    }

    #[tokio::test]
    async fn deployment_stream_frames_are_flushed_before_terminal_state() {
        let (reader, writer) = tokio::io::duplex(1024);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut writer = tokio::io::BufWriter::new(writer);

        assert!(
            write_frame(
                &mut writer,
                DeploymentStreamFrame::Event(DeploymentEvent::Line("early log".into())),
            )
            .await
        );

        let mut line = String::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            reader.read_line(&mut line),
        )
        .await
        .expect("deployment event should be visible before terminal state")
        .unwrap();
        assert!(read > 0);
        assert!(matches!(
            serde_json::from_str::<DeploymentStreamFrame>(line.trim_end()).unwrap(),
            DeploymentStreamFrame::Event(DeploymentEvent::Line(message)) if message == "early log"
        ));
    }

    #[test]
    fn registry_reserves_targets_and_retains_reconciliation_required_jobs() {
        let registry = DeploymentRegistry::default();
        let operations = OperationRegistry::new();
        let first = DeploymentId::from_u128(1);
        let second = DeploymentId::from_u128(2);
        let target = MachineName::new("test").unwrap();
        let first_request = DeploymentRequestId::from_u128(11);
        register_job(&registry, &operations, first_request, first, &target).unwrap();
        assert!(register_job(
            &registry,
            &operations,
            DeploymentRequestId::from_u128(12),
            second,
            &target,
        )
        .is_err());

        registry.update_status(
            first,
            DeploymentStatus::ReconciliationRequired("inspect manifest".into()),
        );
        assert!(registry.acknowledge(first).is_err());
        assert!(matches!(
            registry.snapshot(first).unwrap().status,
            DeploymentStatus::ReconciliationRequired(_)
        ));
    }

    #[test]
    fn registry_releases_a_confirmed_terminal_job() {
        let registry = DeploymentRegistry::default();
        let operations = OperationRegistry::new();
        let id = DeploymentId::from_u128(3);
        let target = MachineName::new("test").unwrap();
        let request_id = DeploymentRequestId::from_u128(13);
        register_job(&registry, &operations, request_id, id, &target).unwrap();
        registry.update_status(id, DeploymentStatus::Succeeded);
        registry.acknowledge_submission(request_id).unwrap();
        registry.acknowledge(id).unwrap();
        register_job(
            &registry,
            &operations,
            DeploymentRequestId::from_u128(14),
            DeploymentId::from_u128(4),
            &target,
        )
        .unwrap();
    }

    #[test]
    fn submission_resolution_refuses_secret_resubmission_and_is_idempotently_acknowledged() {
        let registry = DeploymentRegistry::default();
        let request_id = DeploymentRequestId::from_u128(20);
        let deployment_id = DeploymentId::from_u128(21);
        let plan = plan("test");
        registry
            .reserve_submission(request_id, deployment_id, plan.fingerprint().clone())
            .unwrap();

        assert_eq!(
            registry.resolve_submission(request_id).unwrap().status,
            DeploymentSubmissionStatus::Pending
        );
        let duplicate = registry
            .reserve_submission(request_id, deployment_id, plan.fingerprint().clone())
            .unwrap_err();
        assert!(duplicate.to_string().contains("resolve"));
        assert!(registry
            .reserve_submission(
                request_id,
                DeploymentId::from_u128(22),
                plan.fingerprint().clone(),
            )
            .unwrap_err()
            .to_string()
            .contains("different identity"));

        registry.reject_submission(request_id, "invalid secret capsule".into());
        assert!(matches!(
            registry.resolve_submission(request_id).unwrap().status,
            DeploymentSubmissionStatus::Rejected { .. }
        ));
        registry.acknowledge_submission(request_id).unwrap();
        registry.acknowledge_submission(request_id).unwrap();
        assert!(
            registry
                .resolve_submission(request_id)
                .unwrap()
                .acknowledged
        );
    }

    #[test]
    fn snapshots_are_revisioned_and_release_only_proven_terminal_claims() {
        let registry = DeploymentRegistry::default();
        let operations = OperationRegistry::new();
        let request_id = DeploymentRequestId::from_u128(30);
        let deployment_id = DeploymentId::from_u128(31);
        let target = MachineName::new("revision-test").unwrap();
        let key = crate::application::ResourceKey::for_machine(&target);
        register_job(&registry, &operations, request_id, deployment_id, &target).unwrap();

        let initial = registry.snapshot(deployment_id).unwrap();
        assert_eq!(initial.revision, 1);
        assert_eq!(initial.claim, DeploymentClaimState::Held);
        assert!(operations.is_held(&key));

        let rolling = registry
            .update_status(deployment_id, DeploymentStatus::RollingBack)
            .unwrap();
        assert_eq!(rolling.revision, 2);
        let repeated = registry
            .update_status(deployment_id, DeploymentStatus::RollingBack)
            .unwrap();
        assert_eq!(repeated.revision, 2);

        let terminal = registry
            .update_status(
                deployment_id,
                DeploymentStatus::Failed("known failure".into()),
            )
            .unwrap();
        assert_eq!(terminal.revision, 3);
        assert_eq!(terminal.claim, DeploymentClaimState::Released);
        assert!(!operations.is_held(&key));
    }

    #[test]
    fn cancellation_request_is_revisioned_and_idempotent() {
        let registry = DeploymentRegistry::default();
        let operations = OperationRegistry::new();
        let request_id = DeploymentRequestId::from_u128(35);
        let deployment_id = DeploymentId::from_u128(36);
        let target = MachineName::new("cancel-test").unwrap();
        register_job(&registry, &operations, request_id, deployment_id, &target).unwrap();

        let requested = registry.cancel(deployment_id).unwrap();
        assert_eq!(requested.revision, 2);
        assert!(requested.cancellation_requested);
        let repeated = registry.cancel(deployment_id).unwrap();
        assert_eq!(repeated.revision, requested.revision);
        assert!(repeated.cancellation_requested);
    }

    #[test]
    fn reconciliation_releases_only_when_durable_crash_state_is_absent() {
        let registry = DeploymentRegistry::default();
        let operations = OperationRegistry::new();
        let request_id = DeploymentRequestId::from_u128(37);
        let deployment_id = DeploymentId::from_u128(38);
        let target = MachineName::new("reconcile-test").unwrap();
        let key = crate::application::ResourceKey::for_machine(&target);
        register_job(&registry, &operations, request_id, deployment_id, &target).unwrap();
        let unresolved = registry
            .update_status(
                deployment_id,
                DeploymentStatus::ReconciliationRequired("inspect state".into()),
            )
            .unwrap();

        let retained = registry.reconcile(deployment_id, true).unwrap();
        assert_eq!(retained.revision, unresolved.revision);
        assert_eq!(retained.claim, DeploymentClaimState::ReconciliationRequired);
        assert!(operations.is_held(&key));

        let reconciled = registry.reconcile(deployment_id, false).unwrap();
        assert_eq!(reconciled.revision, unresolved.revision + 1);
        assert_eq!(reconciled.claim, DeploymentClaimState::Reconciled);
        assert!(matches!(
            reconciled.status,
            DeploymentStatus::ReconciliationRequired(_)
        ));
        assert!(!operations.is_held(&key));
        registry.acknowledge_submission(request_id).unwrap();
        registry.acknowledge(deployment_id).unwrap();
    }

    #[test]
    fn unresolved_release_requires_confirmation_and_preserves_history() {
        let registry = DeploymentRegistry::default();
        let operations = OperationRegistry::new();
        let request_id = DeploymentRequestId::from_u128(40);
        let deployment_id = DeploymentId::from_u128(41);
        let target = MachineName::new("unresolved-test").unwrap();
        let key = crate::application::ResourceKey::for_machine(&target);
        register_job(&registry, &operations, request_id, deployment_id, &target).unwrap();
        registry.update_status(
            deployment_id,
            DeploymentStatus::ReconciliationRequired("inspect host state".into()),
        );

        assert!(registry.release_unresolved(deployment_id, false).is_err());
        assert!(operations.is_held(&key));
        let released = registry.release_unresolved(deployment_id, true).unwrap();
        assert_eq!(released.claim, DeploymentClaimState::ReleasedUnresolved);
        assert!(matches!(
            released.status,
            DeploymentStatus::ReconciliationRequired(_)
        ));
        assert!(!operations.is_held(&key));

        registry.acknowledge_submission(request_id).unwrap();
        registry.acknowledge(deployment_id).unwrap();
        registry.acknowledge(deployment_id).unwrap();
    }

    #[test]
    fn capacity_never_evicts_pending_submission_evidence() {
        let registry = DeploymentRegistry::default();
        let fingerprint = plan("test").fingerprint().clone();
        for value in 1..=MAX_DEPLOYMENT_RECORDS as u128 {
            registry
                .reserve_submission(
                    DeploymentRequestId::from_u128(value),
                    DeploymentId::from_u128(value + 100),
                    fingerprint.clone(),
                )
                .unwrap();
        }
        assert!(registry
            .reserve_submission(
                DeploymentRequestId::from_u128(1_000),
                DeploymentId::from_u128(1_001),
                fingerprint,
            )
            .unwrap_err()
            .to_string()
            .contains("capacity"));
    }

    #[test]
    fn capacity_evicts_the_oldest_acknowledged_resolved_job() {
        let registry = DeploymentRegistry::default();
        let operations = OperationRegistry::new();
        for value in 1..=MAX_DEPLOYMENT_RECORDS as u128 {
            let request_id = DeploymentRequestId::from_u128(value);
            let deployment_id = DeploymentId::from_u128(value + 100);
            let target = MachineName::new(format!("evict-{value}")).unwrap();
            register_job(&registry, &operations, request_id, deployment_id, &target).unwrap();
            registry.update_status(deployment_id, DeploymentStatus::Succeeded);
            registry.acknowledge_submission(request_id).unwrap();
            registry.acknowledge(deployment_id).unwrap();
        }

        let replacement_plan = plan("replacement");
        registry
            .reserve_submission(
                DeploymentRequestId::from_u128(1_000),
                DeploymentId::from_u128(1_001),
                replacement_plan.fingerprint().clone(),
            )
            .unwrap();
        assert!(registry
            .resolve_submission(DeploymentRequestId::from_u128(1))
            .is_err());
        assert!(registry.snapshot(DeploymentId::from_u128(101)).is_err());
    }

    #[tokio::test]
    async fn shutdown_waits_for_cancelled_job_to_publish_a_terminal_snapshot() {
        let registry = Arc::new(DeploymentRegistry::default());
        let operations = OperationRegistry::new();
        let request_id = DeploymentRequestId::from_u128(50);
        let deployment_id = DeploymentId::from_u128(51);
        let target = MachineName::new("shutdown-test").unwrap();
        let plan = plan(target.as_str());
        registry
            .reserve_submission(request_id, deployment_id, plan.fingerprint().clone())
            .unwrap();
        let cancellation = DeploymentCancellation::default();
        registry
            .register(
                request_id,
                deployment_id,
                vec![ResourceClaim::exclusive(
                    crate::application::ResourceKey::for_machine(&target),
                )],
                cancellation.clone(),
                &operations,
            )
            .unwrap();
        let updater = Arc::clone(&registry);
        tokio::spawn(async move {
            cancellation.cancelled().await;
            updater.update_status(
                deployment_id,
                DeploymentStatus::Cancelled("daemon shutdown".into()),
            );
        });

        assert!(
            registry
                .cancel_all_and_wait(std::time::Duration::from_secs(1))
                .await
        );
    }
}

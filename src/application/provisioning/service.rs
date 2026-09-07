use super::contract::{
    deployment_job_channel, DeploymentClaimStatus, DeploymentError, DeploymentStatus,
};
use super::{
    DeploymentClaimControl, DeploymentExecutor, DeploymentJobHandle, DeploymentPlan,
    DeploymentPreflight, DeploymentRequest, DeploymentSubmission, RemoteTarSafety, SourcePreflight,
};
use super::{
    DeploymentCrashManifest, DeploymentRecoveryProbe, DeploymentRecoveryReport, DeploymentStatePort,
};
use crate::application::operations::ResourceReservation;
use crate::application::OperationRegistry;
use futures_util::FutureExt;
use std::collections::HashMap;
use std::sync::Arc;

pub struct ProvisioningService {
    source_preflight: Arc<dyn SourcePreflight>,
    executor: Arc<dyn DeploymentExecutor>,
    state: Arc<dyn DeploymentStatePort>,
    recovery: Arc<dyn DeploymentRecoveryProbe>,
    claim_control: Arc<dyn DeploymentClaimControl>,
    operations: Arc<OperationRegistry>,
    unresolved_claims:
        Arc<parking_lot::Mutex<HashMap<super::DeploymentId, RetainedDeploymentClaim>>>,
    recovered_claims: tokio::sync::Mutex<HashMap<super::DeploymentId, RecoveredDeploymentClaim>>,
}

struct RetainedDeploymentClaim {
    _reservation: ResourceReservation,
    status: tokio::sync::watch::Sender<DeploymentClaimStatus>,
    release_in_progress: bool,
}

struct RecoveredDeploymentClaim {
    claim: crate::application::ResourceClaim,
    _reservation: ResourceReservation,
}

impl ProvisioningService {
    pub fn new(
        source_preflight: Arc<dyn SourcePreflight>,
        executor: Arc<dyn DeploymentExecutor>,
        state: Arc<dyn DeploymentStatePort>,
        recovery: Arc<dyn DeploymentRecoveryProbe>,
        claim_control: Arc<dyn DeploymentClaimControl>,
        operations: Arc<OperationRegistry>,
    ) -> Self {
        Self {
            source_preflight,
            executor,
            state,
            recovery,
            claim_control,
            operations,
            unresolved_claims: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            recovered_claims: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub async fn preflight(
        &self,
        request: &DeploymentRequest,
    ) -> Result<DeploymentPreflight, DeploymentError> {
        request.validate()?;
        if !request
            .source
            .is_unacknowledged_remote_tar(request.allow_unsafe_remote_tar)
        {
            return Ok(DeploymentPreflight::Ready);
        }

        Ok(match self.source_preflight.inspect_remote_tar().await? {
            RemoteTarSafety::Compatible => DeploymentPreflight::Ready,
            RemoteTarSafety::Risk(reason) => DeploymentPreflight::ConfirmationRequired(reason),
        })
    }

    pub fn start(
        &self,
        submission: DeploymentSubmission,
    ) -> Result<DeploymentJobHandle, DeploymentError> {
        submission.request().validate()?;
        submission.validate_secrets()?;
        let (request, secrets) = submission.into_parts();
        let plan = DeploymentPlan::build(request)?;
        let reservation = self
            .operations
            .reserve(plan.resource_claims())
            .map_err(|conflict| {
                DeploymentError::rejected(format!(
                    "deployment resource is busy: {:?}",
                    conflict.key
                ))
            })?;

        let id = super::DeploymentId::new();
        let (handle, context) = deployment_job_channel(id);
        let terminal_status = context.status_sender();
        let claim_status = context.claim_status_sender();
        let executor = Arc::clone(&self.executor);
        let unresolved_claims = Arc::clone(&self.unresolved_claims);
        tokio::spawn(async move {
            let terminal = run_deployment_executor(executor, plan, secrets, context).await;
            if terminal.claim_status == DeploymentClaimStatus::ReconciliationRequired {
                unresolved_claims.lock().insert(
                    id,
                    RetainedDeploymentClaim {
                        _reservation: reservation,
                        status: claim_status.clone(),
                        release_in_progress: false,
                    },
                );
            } else {
                // A terminal status is the public completion boundary. Release
                // known-outcome claims before observers can act on it.
                drop(reservation);
            }
            claim_status.send_replace(terminal.claim_status);
            terminal_status.send_replace(terminal.status);
        });
        Ok(handle)
    }

    pub async fn release_unresolved(
        &self,
        deployment_id: super::DeploymentId,
        confirmed: bool,
    ) -> Result<(), DeploymentError> {
        if !confirmed {
            return Err(DeploymentError::rejected(
                "releasing an unresolved deployment requires explicit confirmation",
            ));
        }
        {
            let mut claims = self.unresolved_claims.lock();
            let claim = claims.get_mut(&deployment_id).ok_or_else(|| {
                DeploymentError::rejected(format!(
                    "deployment {deployment_id} does not retain an unresolved coordination claim"
                ))
            })?;
            if claim.release_in_progress {
                return Err(DeploymentError::rejected(format!(
                    "deployment {deployment_id} claim release is already in progress"
                )));
            }
            claim.release_in_progress = true;
        }

        if let Err(error) = self
            .claim_control
            .release_unresolved(deployment_id, true)
            .await
        {
            if let Some(claim) = self.unresolved_claims.lock().get_mut(&deployment_id) {
                claim.release_in_progress = false;
            }
            return Err(error);
        }

        let claim = self
            .unresolved_claims
            .lock()
            .remove(&deployment_id)
            .ok_or_else(|| {
                DeploymentError::reconciliation_required(format!(
                    "deployment {deployment_id} claim disappeared while its release was completing"
                ))
            })?;
        claim
            .status
            .send_replace(DeploymentClaimStatus::ReleasedUnresolved);
        log::warn!(
            "[AUDIT] Explicitly released unresolved application claim for deployment {deployment_id}; historical outcome remains unknown"
        );
        Ok(())
    }

    pub(crate) async fn unfinished_deployments(
        &self,
    ) -> Result<Vec<DeploymentRecoveryReport>, DeploymentError> {
        // Serialize the snapshot read and claim reconciliation so an older
        // concurrent scan cannot overwrite a newer manifest view.
        let mut recovered_claims = self.recovered_claims.lock().await;
        let manifests = self
            .state
            .unfinished()
            .await
            .map_err(|error| DeploymentError::failed(error.to_string()))?;

        let mut desired = HashMap::with_capacity(manifests.len());
        for manifest in &manifests {
            if desired
                .insert(manifest.deployment_id, manifest.recovery_claim())
                .is_some()
            {
                return Err(DeploymentError::reconciliation_required(format!(
                    "unfinished deployment {} appears more than once",
                    manifest.deployment_id
                )));
            }
        }

        for (deployment_id, retained) in recovered_claims.iter() {
            if let Some(claim) = desired.get(deployment_id) {
                if claim != &retained.claim {
                    return Err(DeploymentError::reconciliation_required(format!(
                        "unfinished deployment {deployment_id} changed its claimed resource"
                    )));
                }
            }
        }

        recovered_claims.retain(|deployment_id, _| desired.contains_key(deployment_id));
        let mut additions = Vec::new();
        for (deployment_id, claim) in &desired {
            if recovered_claims.contains_key(deployment_id) {
                continue;
            }
            let reservation = self
                .operations
                .reserve([claim.clone()])
                .map_err(|conflict| {
                    DeploymentError::reconciliation_required(format!(
                        "unfinished deployment recovery conflicts with an active resource: {:?}",
                        conflict.key
                    ))
                })?;
            additions.push((
                *deployment_id,
                RecoveredDeploymentClaim {
                    claim: claim.clone(),
                    _reservation: reservation,
                },
            ));
        }
        recovered_claims.extend(additions);
        drop(recovered_claims);

        let mut reports = Vec::with_capacity(manifests.len());
        for manifest in manifests {
            match self.recovery.probe(&manifest).await {
                Ok(observations) => reports.push(DeploymentRecoveryReport {
                    manifest,
                    observations,
                    probe_error: None,
                }),
                Err(error) => reports.push(DeploymentRecoveryReport {
                    manifest,
                    observations: Vec::new(),
                    probe_error: Some(error.to_string()),
                }),
            }
        }
        Ok(reports)
    }
}

pub(crate) struct DeploymentTerminal {
    pub(crate) status: DeploymentStatus,
    pub(crate) claim_status: DeploymentClaimStatus,
}

pub(crate) async fn run_deployment_executor(
    executor: Arc<dyn DeploymentExecutor>,
    plan: DeploymentPlan,
    secrets: super::DeploymentSecrets,
    context: super::DeploymentJobContext,
) -> DeploymentTerminal {
    let execution = async { executor.run(plan, secrets, context).await };
    let result = std::panic::AssertUnwindSafe(execution).catch_unwind().await;
    let terminal = match result {
        Ok(Ok(())) => DeploymentTerminal {
            status: DeploymentStatus::Succeeded,
            claim_status: DeploymentClaimStatus::Released,
        },
        Ok(Err(error)) if error.is_cancelled() => DeploymentTerminal {
            status: DeploymentStatus::Cancelled(error.to_string()),
            claim_status: DeploymentClaimStatus::Released,
        },
        Ok(Err(error)) if error.requires_reconciliation() => DeploymentTerminal {
            status: DeploymentStatus::ReconciliationRequired(error.to_string()),
            claim_status: if error.retains_resource_claim() {
                DeploymentClaimStatus::ReconciliationRequired
            } else {
                DeploymentClaimStatus::Reconciled
            },
        },
        Ok(Err(error)) => DeploymentTerminal {
            status: DeploymentStatus::Failed(error.to_string()),
            claim_status: DeploymentClaimStatus::Released,
        },
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            log::error!("Deployment job panicked: {message}");
            DeploymentTerminal {
                status: DeploymentStatus::ReconciliationRequired(
                    "Deployment pipeline panicked; inspect durable deployment state before retrying"
                        .into(),
                ),
                claim_status: DeploymentClaimStatus::ReconciliationRequired,
            }
        }
    };
    terminal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::provisioning::MachineProvisioningConfig;
    use crate::application::provisioning::{
        DeploymentEvent, DeploymentJobContext, DeploymentSecrets, DeploymentSource,
        DeploymentStorage,
    };
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingExecutor {
        outcome: Mutex<Option<Result<(), DeploymentError>>>,
    }

    struct RecordingPreflight {
        calls: AtomicUsize,
        safety: RemoteTarSafety,
    }

    struct CancellationExecutor;

    struct PanicExecutor;

    struct FailingClaimControl;

    #[async_trait]
    impl SourcePreflight for RecordingPreflight {
        async fn inspect_remote_tar(&self) -> Result<RemoteTarSafety, DeploymentError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.safety.clone())
        }
    }

    #[async_trait]
    impl DeploymentExecutor for RecordingExecutor {
        async fn run(
            &self,
            _plan: DeploymentPlan,
            _secrets: DeploymentSecrets,
            context: DeploymentJobContext,
        ) -> Result<(), DeploymentError> {
            context
                .event_sender()
                .send(DeploymentEvent::Line("started".into()))
                .await
                .unwrap();
            self.outcome.lock().take().unwrap_or(Ok(()))
        }
    }

    #[async_trait]
    impl DeploymentExecutor for CancellationExecutor {
        async fn run(
            &self,
            _plan: DeploymentPlan,
            _secrets: DeploymentSecrets,
            context: DeploymentJobContext,
        ) -> Result<(), DeploymentError> {
            context.cancellation().cancelled().await;
            Err(DeploymentError::cancelled("cancelled"))
        }
    }

    #[async_trait]
    impl DeploymentExecutor for PanicExecutor {
        async fn run(
            &self,
            _plan: DeploymentPlan,
            _secrets: DeploymentSecrets,
            _context: DeploymentJobContext,
        ) -> Result<(), DeploymentError> {
            panic!("executor panic")
        }
    }

    #[async_trait]
    impl super::super::DeploymentClaimControl for FailingClaimControl {
        async fn release_unresolved(
            &self,
            _deployment_id: super::super::DeploymentId,
            _confirmed: bool,
        ) -> Result<(), DeploymentError> {
            Err(DeploymentError::reconciliation_required(
                "claim authority is unavailable",
            ))
        }
    }

    fn submission() -> DeploymentSubmission {
        DeploymentSubmission::new(
            DeploymentRequest {
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
            },
            DeploymentSecrets::new(String::new(), Vec::new()),
        )
    }

    fn executor(outcome: Result<(), DeploymentError>) -> Arc<RecordingExecutor> {
        Arc::new(RecordingExecutor {
            outcome: Mutex::new(Some(outcome)),
        })
    }

    fn recovery() -> Arc<super::super::MemoryDeploymentRecoveryProbe> {
        Arc::new(super::super::MemoryDeploymentRecoveryProbe)
    }

    fn claim_control() -> Arc<super::super::MemoryDeploymentClaimControl> {
        Arc::new(super::super::MemoryDeploymentClaimControl)
    }

    async fn wait_until_finished(handle: &DeploymentJobHandle) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !handle.status().is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn local_source_preflight_does_not_probe_the_host_tar_runtime() {
        let preflight = Arc::new(RecordingPreflight {
            calls: AtomicUsize::new(0),
            safety: RemoteTarSafety::Risk("old tar".into()),
        });
        let service = ProvisioningService::new(
            preflight.clone(),
            executor(Ok(())),
            Arc::new(super::super::MemoryDeploymentStatePort::default()),
            recovery(),
            claim_control(),
            OperationRegistry::new(),
        );
        let (request, _) = submission().into_parts();

        assert_eq!(
            service.preflight(&request).await.unwrap(),
            DeploymentPreflight::Ready
        );
        assert_eq!(preflight.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn remote_tar_risk_is_application_owned_confirmation_policy() {
        let preflight = Arc::new(RecordingPreflight {
            calls: AtomicUsize::new(0),
            safety: RemoteTarSafety::Risk("old tar".into()),
        });
        let service = ProvisioningService::new(
            preflight.clone(),
            executor(Ok(())),
            Arc::new(super::super::MemoryDeploymentStatePort::default()),
            recovery(),
            claim_control(),
            OperationRegistry::new(),
        );
        let (mut request, _) = submission().into_parts();
        request.source = DeploymentSource::Pull {
            url: "https://example.invalid/rootfs.tar".into(),
            is_raw: false,
        };

        assert_eq!(
            service.preflight(&request).await.unwrap(),
            DeploymentPreflight::ConfirmationRequired("old tar".into())
        );
        assert_eq!(preflight.calls.load(Ordering::Relaxed), 1);

        request.allow_unsafe_remote_tar = true;
        assert_eq!(
            service.preflight(&request).await.unwrap(),
            DeploymentPreflight::Ready
        );
        assert_eq!(preflight.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn job_handle_owns_events_and_terminal_status() {
        let preflight = Arc::new(RecordingPreflight {
            calls: AtomicUsize::new(0),
            safety: RemoteTarSafety::Compatible,
        });
        let service = ProvisioningService::new(
            preflight,
            executor(Ok(())),
            Arc::new(super::super::MemoryDeploymentStatePort::default()),
            recovery(),
            claim_control(),
            OperationRegistry::new(),
        );
        let mut handle = service.start(submission()).unwrap();

        wait_until_finished(&handle).await;

        assert_eq!(handle.status(), DeploymentStatus::Succeeded);
        assert_eq!(
            handle.try_recv().unwrap(),
            DeploymentEvent::Line("started".into())
        );
    }

    #[tokio::test]
    async fn cancelled_port_result_has_a_distinct_terminal_state() {
        let preflight = Arc::new(RecordingPreflight {
            calls: AtomicUsize::new(0),
            safety: RemoteTarSafety::Compatible,
        });
        let service = ProvisioningService::new(
            preflight,
            executor(Err(DeploymentError::cancelled("cancelled"))),
            Arc::new(super::super::MemoryDeploymentStatePort::default()),
            recovery(),
            claim_control(),
            OperationRegistry::new(),
        );
        let handle = service.start(submission()).unwrap();

        wait_until_finished(&handle).await;

        assert_eq!(
            handle.status(),
            DeploymentStatus::Cancelled("cancelled".into())
        );
    }

    #[tokio::test]
    async fn deployment_claims_conflict_while_running_and_release_after_known_cancellation() {
        let registry = OperationRegistry::new();
        let service = ProvisioningService::new(
            Arc::new(RecordingPreflight {
                calls: AtomicUsize::new(0),
                safety: RemoteTarSafety::Compatible,
            }),
            Arc::new(CancellationExecutor),
            Arc::new(super::super::MemoryDeploymentStatePort::default()),
            recovery(),
            claim_control(),
            Arc::clone(&registry),
        );
        let handle = service.start(submission()).unwrap();
        let target = crate::domain::machine::MachineName::new("test").unwrap();
        let source = crate::domain::runtime::ImageName::new("base").unwrap();

        assert!(registry
            .reserve([crate::application::ResourceClaim::exclusive(
                crate::application::ResourceKey::for_machine(&target),
            )])
            .is_err());
        assert!(registry
            .reserve([crate::application::ResourceClaim::exclusive(
                crate::application::ResourceKey::for_image(&source),
            )])
            .is_err());

        handle.request_cancel();
        wait_until_finished(&handle).await;
        assert!(registry
            .reserve([crate::application::ResourceClaim::exclusive(
                crate::application::ResourceKey::for_machine(&target),
            )])
            .is_ok());
    }

    #[tokio::test]
    async fn executor_panic_is_reconciliation_required_and_retains_its_claim() {
        let registry = OperationRegistry::new();
        let service = ProvisioningService::new(
            Arc::new(RecordingPreflight {
                calls: AtomicUsize::new(0),
                safety: RemoteTarSafety::Compatible,
            }),
            Arc::new(PanicExecutor),
            Arc::new(super::super::MemoryDeploymentStatePort::default()),
            recovery(),
            claim_control(),
            Arc::clone(&registry),
        );
        let handle = service.start(submission()).unwrap();
        wait_until_finished(&handle).await;
        assert!(matches!(
            handle.status(),
            DeploymentStatus::ReconciliationRequired(_)
        ));
        assert_eq!(
            handle.claim_status(),
            DeploymentClaimStatus::ReconciliationRequired
        );
        let target = crate::domain::machine::MachineName::new("test").unwrap();
        assert!(registry
            .reserve([crate::application::ResourceClaim::exclusive(
                crate::application::ResourceKey::for_machine(&target),
            )])
            .is_err());
        assert_eq!(service.unresolved_claims.lock().len(), 1);

        assert!(service
            .release_unresolved(handle.id(), false)
            .await
            .is_err());
        assert_eq!(
            handle.claim_status(),
            DeploymentClaimStatus::ReconciliationRequired
        );
        service.release_unresolved(handle.id(), true).await.unwrap();
        assert_eq!(
            handle.claim_status(),
            DeploymentClaimStatus::ReleasedUnresolved
        );
        assert!(matches!(
            handle.status(),
            DeploymentStatus::ReconciliationRequired(_)
        ));
        assert!(service.unresolved_claims.lock().is_empty());
        assert!(registry
            .reserve([crate::application::ResourceClaim::exclusive(
                crate::application::ResourceKey::for_machine(&target),
            )])
            .is_ok());
    }

    #[tokio::test]
    async fn failed_authoritative_release_keeps_the_claim_and_allows_retry() {
        let registry = OperationRegistry::new();
        let service = ProvisioningService::new(
            Arc::new(RecordingPreflight {
                calls: AtomicUsize::new(0),
                safety: RemoteTarSafety::Compatible,
            }),
            Arc::new(PanicExecutor),
            Arc::new(super::super::MemoryDeploymentStatePort::default()),
            recovery(),
            Arc::new(FailingClaimControl),
            Arc::clone(&registry),
        );
        let handle = service.start(submission()).unwrap();
        wait_until_finished(&handle).await;

        let first = service
            .release_unresolved(handle.id(), true)
            .await
            .unwrap_err();
        let second = service
            .release_unresolved(handle.id(), true)
            .await
            .unwrap_err();
        assert_eq!(first.to_string(), "claim authority is unavailable");
        assert_eq!(second.to_string(), "claim authority is unavailable");
        assert_eq!(
            handle.claim_status(),
            DeploymentClaimStatus::ReconciliationRequired
        );
        let target = crate::domain::machine::MachineName::new("test").unwrap();
        assert!(registry
            .reserve([crate::application::ResourceClaim::exclusive(
                crate::application::ResourceKey::for_machine(&target),
            )])
            .is_err());
    }

    #[tokio::test]
    async fn reconciled_unknown_outcome_keeps_history_but_releases_its_claim() {
        let registry = OperationRegistry::new();
        let service = ProvisioningService::new(
            Arc::new(RecordingPreflight {
                calls: AtomicUsize::new(0),
                safety: RemoteTarSafety::Compatible,
            }),
            executor(Err(DeploymentError::reconciled_unknown(
                "historical outcome is unknown",
            ))),
            Arc::new(super::super::MemoryDeploymentStatePort::default()),
            recovery(),
            claim_control(),
            Arc::clone(&registry),
        );
        let handle = service.start(submission()).unwrap();
        wait_until_finished(&handle).await;
        assert_eq!(
            handle.status(),
            DeploymentStatus::ReconciliationRequired("historical outcome is unknown".into())
        );
        assert_eq!(handle.claim_status(), DeploymentClaimStatus::Reconciled);

        let target = crate::domain::machine::MachineName::new("test").unwrap();
        assert!(registry
            .reserve([crate::application::ResourceClaim::exclusive(
                crate::application::ResourceKey::for_machine(&target),
            )])
            .is_ok());
        assert!(service.unresolved_claims.lock().is_empty());
    }

    #[tokio::test]
    async fn startup_recovery_tracks_manifest_claims_without_mutating_them() {
        let registry = OperationRegistry::new();
        let state = Arc::new(super::super::MemoryDeploymentStatePort::default());
        let (request, _) = submission().into_parts();
        let plan = DeploymentPlan::build(request).unwrap();
        let manifest = DeploymentCrashManifest::prepared(
            crate::application::provisioning::DeploymentId::from_u128(70),
            &plan,
        );
        state.create(manifest.clone()).await.unwrap();
        let service = ProvisioningService::new(
            Arc::new(RecordingPreflight {
                calls: AtomicUsize::new(0),
                safety: RemoteTarSafety::Compatible,
            }),
            executor(Ok(())),
            state.clone(),
            recovery(),
            claim_control(),
            Arc::clone(&registry),
        );

        let recovered = service.unfinished_deployments().await.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].manifest, manifest);
        assert!(recovered[0].observations.is_empty());
        assert_eq!(recovered[0].probe_error, None);
        assert!(registry
            .reserve([crate::application::ResourceClaim::exclusive(
                crate::application::ResourceKey::for_machine(plan.target()),
            )])
            .is_err());
        assert_eq!(state.unfinished().await.unwrap().len(), 1);
        assert_eq!(service.unfinished_deployments().await.unwrap().len(), 1);

        state
            .remove(manifest.deployment_id, manifest.revision)
            .await
            .unwrap();
        assert!(service.unfinished_deployments().await.unwrap().is_empty());
        assert!(registry
            .reserve([crate::application::ResourceClaim::exclusive(
                crate::application::ResourceKey::for_machine(plan.target()),
            )])
            .is_ok());
    }

    #[test]
    fn invalid_secret_capsule_is_rejected_before_a_job_is_allocated() {
        let preflight = Arc::new(RecordingPreflight {
            calls: AtomicUsize::new(0),
            safety: RemoteTarSafety::Compatible,
        });
        let service = ProvisioningService::new(
            preflight,
            executor(Ok(())),
            Arc::new(super::super::MemoryDeploymentStatePort::default()),
            recovery(),
            claim_control(),
            OperationRegistry::new(),
        );
        let (request, _) = submission().into_parts();
        let invalid = DeploymentSubmission::new(
            request,
            DeploymentSecrets::new(
                String::new(),
                vec![crate::application::provisioning::UserSecret::new(
                    "unexpected".into(),
                    "secret".into(),
                )],
            ),
        );

        let error = match service.start(invalid) {
            Ok(_) => panic!("mismatched secrets must be rejected"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("do not match the requested user accounts"));
    }
}

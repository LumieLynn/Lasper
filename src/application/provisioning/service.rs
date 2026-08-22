use super::contract::{deployment_job_channel, DeploymentError, DeploymentStatus};
use super::{
    DeploymentExecutor, DeploymentJobHandle, DeploymentPreflight, DeploymentRequest,
    DeploymentSubmission, RemoteTarSafety, SourcePreflight,
};
use futures_util::FutureExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct ProvisioningService {
    source_preflight: Arc<dyn SourcePreflight>,
    executor: Arc<dyn DeploymentExecutor>,
    next_id: AtomicU64,
}

impl ProvisioningService {
    pub fn new(
        source_preflight: Arc<dyn SourcePreflight>,
        executor: Arc<dyn DeploymentExecutor>,
    ) -> Self {
        Self {
            source_preflight,
            executor,
            next_id: AtomicU64::new(1),
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

        let id = self.allocate_id();
        let (handle, context) = deployment_job_channel(id);
        let executor = Arc::clone(&self.executor);
        let terminal_status = context.status_sender();
        tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(executor.run(submission, context))
                .catch_unwind()
                .await;
            let status = match result {
                Ok(Ok(())) => DeploymentStatus::Succeeded,
                Ok(Err(error)) if error.is_cancelled() => {
                    DeploymentStatus::Cancelled(error.to_string())
                }
                Ok(Err(error)) => DeploymentStatus::Failed(error.to_string()),
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("unknown panic");
                    log::error!("Deployment job panicked: {message}");
                    DeploymentStatus::Failed("Deployment pipeline panicked".into())
                }
            };
            terminal_status.send_replace(status);
        });
        Ok(handle)
    }

    fn allocate_id(&self) -> super::DeploymentId {
        loop {
            let value = self.next_id.fetch_add(1, Ordering::Relaxed);
            if let Some(id) = super::DeploymentId::new(value) {
                return id;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::provisioning::{
        DeploymentEvent, DeploymentJobContext, DeploymentSecrets, DeploymentSource,
        DeploymentStorage,
    };
    use crate::nspawn::models::ContainerConfig;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicUsize;

    struct RecordingExecutor {
        outcome: Mutex<Option<Result<(), DeploymentError>>>,
    }

    struct RecordingPreflight {
        calls: AtomicUsize,
        safety: RemoteTarSafety,
    }

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
            _submission: DeploymentSubmission,
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

    fn submission() -> DeploymentSubmission {
        DeploymentSubmission::new(
            DeploymentRequest {
                config: ContainerConfig {
                    name: "test".into(),
                    ..Default::default()
                },
                source: DeploymentSource::Copy {
                    source_name: "base".into(),
                },
                storage: DeploymentStorage::Directory,
                nvidia_profile: None,
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

    #[tokio::test]
    async fn local_source_preflight_does_not_probe_the_host_tar_runtime() {
        let preflight = Arc::new(RecordingPreflight {
            calls: AtomicUsize::new(0),
            safety: RemoteTarSafety::Risk("old tar".into()),
        });
        let service = ProvisioningService::new(preflight.clone(), executor(Ok(())));
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
        let service = ProvisioningService::new(preflight.clone(), executor(Ok(())));
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
        let service = ProvisioningService::new(preflight, executor(Ok(())));
        let mut handle = service.start(submission()).unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !handle.status().is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

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
        );
        let handle = service.start(submission()).unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !handle.status().is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(
            handle.status(),
            DeploymentStatus::Cancelled("cancelled".into())
        );
    }

    #[test]
    fn invalid_secret_capsule_is_rejected_before_a_job_is_allocated() {
        let preflight = Arc::new(RecordingPreflight {
            calls: AtomicUsize::new(0),
            safety: RemoteTarSafety::Compatible,
        });
        let service = ProvisioningService::new(preflight, executor(Ok(())));
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

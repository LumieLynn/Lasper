use super::contract::{deployment_job_channel, DeploymentError, DeploymentStatus};
use super::{
    DeploymentJobHandle, DeploymentPreflight, DeploymentRequest, DeploymentSubmission,
    ProvisioningPort,
};
use futures_util::FutureExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct ProvisioningService {
    port: Arc<dyn ProvisioningPort>,
    next_id: AtomicU64,
}

impl ProvisioningService {
    pub fn new(port: Arc<dyn ProvisioningPort>) -> Self {
        Self {
            port,
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn preflight(
        &self,
        request: &DeploymentRequest,
    ) -> Result<DeploymentPreflight, DeploymentError> {
        request.validate()?;
        self.port.preflight(request).await
    }

    pub fn start(
        &self,
        submission: DeploymentSubmission,
    ) -> Result<DeploymentJobHandle, DeploymentError> {
        submission.request().validate()?;
        submission.validate_secrets()?;

        let id = self.allocate_id();
        let (handle, context) = deployment_job_channel(id);
        let port = Arc::clone(&self.port);
        let terminal_status = context.status_sender();
        tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(port.run(submission, context))
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

    struct RecordingPort {
        outcome: Mutex<Option<Result<(), DeploymentError>>>,
    }

    #[async_trait]
    impl ProvisioningPort for RecordingPort {
        async fn preflight(
            &self,
            _request: &DeploymentRequest,
        ) -> Result<DeploymentPreflight, DeploymentError> {
            Ok(DeploymentPreflight::Ready)
        }

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

    #[tokio::test]
    async fn job_handle_owns_events_and_terminal_status() {
        let service = ProvisioningService::new(Arc::new(RecordingPort {
            outcome: Mutex::new(Some(Ok(()))),
        }));
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
        let service = ProvisioningService::new(Arc::new(RecordingPort {
            outcome: Mutex::new(Some(Err(DeploymentError::cancelled("cancelled")))),
        }));
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
        let service = ProvisioningService::new(Arc::new(RecordingPort {
            outcome: Mutex::new(Some(Ok(()))),
        }));
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

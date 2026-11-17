//! Owned requests and the public entry point for guest execution.
//!
//! Prepare hands its connection to a background driver. The returned Session
//! controls that driver; SessionIo owns the data descriptors independently.

mod error;
mod options;
mod request;
mod session;
mod shell;

pub use error::{PrepareCapacityError, PrepareError, RequestError};
pub use options::{ClientOptions, ClientTimeouts, DEFAULT_ENDPOINT};
pub use request::{ProcessSpec, Program, Stdio};
pub use session::Session;
pub use shell::{open_shell, ShellOpenError};

use crate::{
    BrokerConnectError, BrokerConnection, BrokerSession, MachineTarget, NegotiatedHello, SessionIo,
};
use error::invalid;
use lasper_guest_protocol::{ControlLimits, PrepareReady};
use std::sync::Arc;
use tokio::{sync::Mutex, time::Instant};

#[derive(Clone)]
pub struct GuestClient {
    target: Arc<MachineTarget>,
    options: ClientOptions,
}

#[derive(Clone, Debug)]
pub struct RuntimeInfo {
    pub hello: NegotiatedHello,
}

pub use crate::session::StartOptions;

/// Execution is prepared but not submitted. Read or drain `io` independently
/// once Start returns; terminal input must wait for Start acceptance.
pub struct Prepared {
    pub session: Session,
    pub io: SessionIo,
}

impl GuestClient {
    pub fn new(target: Arc<MachineTarget>, options: ClientOptions) -> Result<Self, RequestError> {
        if !options.endpoint.starts_with('/')
            || options.endpoint.chars().any(char::is_control)
            || options.endpoint.split('/').any(|s| matches!(s, "." | ".."))
        {
            return Err(invalid(
                "endpoint",
                "expected an absolute path without control characters or dot components",
            ));
        }
        ControlLimits::new(
            options.control_limits.max_control_frame_bytes(),
            options.control_limits.max_descriptors(),
        )
        .map_err(|e| invalid("control_limits", e))?;
        let supported =
            lasper_guest_protocol::HelloOffer::local(options.control_limits).supported_features;
        if !supported.contains(options.required_features) {
            return Err(invalid("required_features", "unsupported feature bits"));
        }
        for duration in [
            options.timeouts.connect_hello,
            options.timeouts.prepare,
            options.timeouts.operation,
            options.timeouts.cleanup_observation,
        ] {
            if duration.is_zero()
                || duration.as_millis() > u128::from(u64::MAX)
                || Instant::now().checked_add(duration).is_none()
            {
                return Err(invalid(
                    "timeouts",
                    "expected positive representable durations",
                ));
            }
        }
        Ok(Self { target, options })
    }

    pub fn target(&self) -> &Arc<MachineTarget> {
        &self.target
    }

    async fn connect(&self, deadline: Instant) -> Result<BrokerConnection, BrokerConnectError> {
        BrokerConnection::connect_with_features(
            Arc::clone(&self.target),
            &self.options.endpoint,
            self.options.control_limits,
            self.options.required_features,
            deadline.min(Instant::now() + self.options.timeouts.connect_hello),
        )
        .await
    }

    /// Exchanges Hello on a fresh connection without creating a worker.
    pub async fn probe(&self, deadline: Instant) -> Result<RuntimeInfo, BrokerConnectError> {
        let connection = self.connect(deadline).await?;
        Ok(RuntimeInfo {
            hello: connection.hello(),
        })
    }

    /// Validates capacities after Hello and before sending Prepare. Returning
    /// Ready also requires worker identity verification. Cancellation drops the
    /// connection or its driver handle and does not wait for guest cleanup.
    pub async fn prepare(
        &self,
        spec: ProcessSpec,
        deadline: Instant,
    ) -> Result<Prepared, PrepareError> {
        let deadline = deadline.min(Instant::now() + self.options.timeouts.prepare);
        let request = spec.wire().map_err(PrepareError::InvalidRequest)?;
        let connection = self
            .connect(deadline)
            .await
            .map_err(PrepareError::Connect)?;
        let info = RuntimeInfo {
            hello: connection.hello(),
        };
        spec.validate_profile(&request, info.hello.profile().capacities())
            .map_err(PrepareError::InvalidRequest)?;
        // Encode before handing the connection to the driver. No Prepare byte
        // is sent on either capacity error, so no worker has been created.
        let mut bytes = vec![0; info.hello.profile().capacities().max_control_frame_bytes as usize];
        let frame = lasper_guest_protocol::PrepareRequestFrame::new(
            2,
            info.hello.response().broker_id(),
            request.clone(),
        );
        let length = info
            .hello
            .protocol()
            .encode_prepare_frame(&frame, &mut bytes)
            .map_err(|error| match error {
                lasper_guest_protocol::PrepareFrameError::Limit(
                    lasper_guest_protocol::ControlLimitError::FrameTooLarge { declared, maximum },
                ) => PrepareError::Capacity(PrepareCapacityError::FrameBytes {
                    required: declared as usize,
                    available: maximum as usize,
                }),
                error => PrepareError::InvalidRequest(invalid("process", error)),
            })?;
        let descriptors = PrepareReady::descriptor_count_for(request.execution());
        let available = info.hello.limits().get().max_descriptors();
        if descriptors > available {
            return Err(PrepareError::Capacity(
                PrepareCapacityError::ReadyDescriptors {
                    required: descriptors,
                    available,
                },
            ));
        }
        let encoded = crate::session::EncodedPrepare::from_frame(bytes, length);
        let mut inner = BrokerSession::prepare_encoded(
            connection,
            encoded,
            deadline,
            self.options.timeouts.clone(),
        )
        .await
        .map_err(|(error, last_snapshot)| PrepareError::Session {
            error,
            last_snapshot,
        })?;
        let io = SessionIo::from_ready(inner.take_io().expect("Ready delivers data descriptors"))
            .map_err(|e| PrepareError::Session {
            error: e.into(),
            last_snapshot: inner.snapshot(),
        })?;
        Ok(Prepared {
            session: Session {
                inner,
                info,
                close_once: Mutex::new(false),
            },
            io,
        })
    }
}

//! Establish one helper connection against a pinned machine instance.

mod endpoint;
mod peer;
mod socket;

use lasper_guest_protocol::{
    ControlFrameHeader, ControlLimits, HelloResponse, MessageKind, HELLO_PAYLOAD_BUFFER_BYTES,
};
use lasper_rootfs_path::RootfsAccess;
use std::fmt;
use std::fs::File;
use std::io;
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::time::{sleep_until, timeout_at, Duration, Instant};

use crate::process::ProcessHandle;
use crate::{MachineTarget, UnixControlStream, WorkerHello, WorkerReadiness, WorkerReadinessError};

const CONNECT_HELLO_TIMEOUT: Duration = Duration::from_secs(1);
const HELLO_REQUEST_ID: u64 = 1;
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// The control stream, machine instance and broker generation from one Hello.
/// A restarted broker requires a new connection; preparations are not migrated.
#[must_use]
pub struct BrokerConnection {
    target: Arc<MachineTarget>,
    peer: ProcessHandle,
    control: UnixControlStream,
    readiness: WorkerReadiness,
}

impl BrokerConnection {
    /// Wait for an endpoint inside the retained guest root, then connect to its
    /// pinned inode. Missing/refused endpoints, queue backoff and Hello share one
    /// deadline: at most one second, shortened by the caller's readiness deadline.
    /// Once connected, peer or Hello failures are returned without reconnecting.
    ///
    /// Broker identity requires SO_PEERPIDFD. Older kernels return Unsupported
    /// rather than replacing the socket's peer with a later numeric PID lookup.
    pub async fn connect(
        target: Arc<MachineTarget>,
        endpoint: &str,
        limits: ControlLimits,
        deadline: Instant,
    ) -> Result<Self, BrokerConnectError> {
        let deadline = deadline.min(Instant::now() + CONNECT_HELLO_TIMEOUT);
        if Instant::now() >= deadline {
            return Err(BrokerConnectError::Deadline);
        }
        timeout_at(
            deadline,
            Self::establish(target, endpoint, limits, deadline),
        )
        .await
        .map_err(|_| BrokerConnectError::Deadline)?
    }

    async fn establish(
        target: Arc<MachineTarget>,
        endpoint: &str,
        limits: ControlLimits,
        deadline: Instant,
    ) -> Result<Self, BrokerConnectError> {
        target.recheck().map_err(BrokerConnectError::Target)?;
        let hello = WorkerHello::new(limits).map_err(BrokerConnectError::Readiness)?;
        let stream = connect_endpoint(&target, endpoint, deadline).await?;
        let peer = peer::identify(&stream, &target).map_err(BrokerConnectError::InvalidPeer)?;
        let mut control = UnixControlStream::new(stream);
        let readiness = exchange_hello(&mut control, hello).await?;
        target
            .verify_process(&peer)
            .map_err(BrokerConnectError::InvalidPeer)?;
        if Instant::now() >= deadline {
            return Err(BrokerConnectError::Deadline);
        }
        Ok(Self {
            target,
            peer,
            control,
            readiness,
        })
    }

    pub fn target(&self) -> &Arc<MachineTarget> {
        &self.target
    }

    pub fn readiness(&self) -> &WorkerReadiness {
        &self.readiness
    }

    pub fn recheck(&self) -> io::Result<()> {
        if !self.control.can_continue() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "broker control connection is no longer usable",
            ));
        }
        self.target.verify_process(&self.peer)
    }
}

async fn connect_endpoint(
    target: &MachineTarget,
    path: &str,
    deadline: Instant,
) -> Result<UnixStream, BrokerConnectError> {
    let root = target
        .root()
        .try_clone_to_owned()
        .map(File::from)
        .and_then(RootfsAccess::from_directory)
        .map_err(BrokerConnectError::Endpoint)?;
    let mut endpoint = None;
    loop {
        if Instant::now() >= deadline {
            return Err(BrokerConnectError::Deadline);
        }
        target.recheck().map_err(BrokerConnectError::Target)?;
        if endpoint.is_none() {
            endpoint = endpoint::BrokerEndpoint::open(&root, path)
                .map_err(BrokerConnectError::Endpoint)?;
        }
        if let Some(candidate) = &endpoint {
            match socket::connect(&candidate.address()) {
                Ok(stream) => return Ok(stream),
                // bind creates the pathname before listen. A stale socket can
                // also refuse: resolve again so a restarted broker can replace it.
                Err(error) if error.raw_os_error() == Some(libc::ECONNREFUSED) => {
                    endpoint = None;
                }
                // A full AF_UNIX accept queue has no pending connection. Each
                // attempt owns a fresh socket but keeps this candidate inode.
                Err(error) if error.raw_os_error() == Some(libc::EAGAIN) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(BrokerConnectError::Connect(error)),
            }
        }
        sleep_until((Instant::now() + CONNECT_RETRY_INTERVAL).min(deadline)).await;
    }
}

async fn exchange_hello(
    control: &mut UnixControlStream,
    hello: WorkerHello,
) -> Result<WorkerReadiness, BrokerConnectError> {
    let mut payload = [0; HELLO_PAYLOAD_BUFFER_BYTES];
    let length = hello
        .request()
        .encode_payload(&mut payload)
        .map_err(hello_payload)?;
    let header = ControlFrameHeader::new(
        MessageKind::HelloRequest,
        HELLO_REQUEST_ID,
        length as u32,
        0,
    )
    .map_err(hello_payload)?;
    control
        .send_frame(header, &payload[..length], &[])
        .await
        .map_err(BrokerConnectError::Hello)?;
    let frame = control
        .receive_frame()
        .await
        .map_err(BrokerConnectError::Hello)?;
    if frame.header().kind() != MessageKind::HelloResponse
        || frame.header().request_id() != HELLO_REQUEST_ID
    {
        return Err(hello_payload(
            "broker response does not match this Hello request",
        ));
    }
    let response = HelloResponse::decode_payload(frame.payload()).map_err(hello_payload)?;
    let readiness = hello
        .negotiate(frame.header().protocol_version(), response)
        .map_err(BrokerConnectError::Readiness)?;
    control
        .complete_hello(readiness.limits())
        .map_err(BrokerConnectError::Hello)?;
    Ok(readiness)
}

fn hello_payload(error: impl fmt::Debug) -> BrokerConnectError {
    BrokerConnectError::Hello(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid broker Hello: {error:?}"),
    ))
}

#[derive(Debug)]
pub enum BrokerConnectError {
    Target(io::Error),
    Endpoint(io::Error),
    Connect(io::Error),
    InvalidPeer(io::Error),
    Hello(io::Error),
    Readiness(WorkerReadinessError),
    Deadline,
}

impl fmt::Display for BrokerConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Target(error) => {
                write!(formatter, "helper machine instance is unavailable: {error}")
            }
            Self::Endpoint(error) => write!(formatter, "resolve guest broker socket: {error}"),
            Self::Connect(error) => write!(formatter, "connect guest broker socket: {error}"),
            Self::InvalidPeer(error) => write!(formatter, "verify guest broker process: {error}"),
            Self::Hello(error) => write!(formatter, "exchange guest broker Hello: {error}"),
            Self::Readiness(error) => error.fmt(formatter),
            Self::Deadline => {
                formatter.write_str("guest broker connect and Hello deadline expired")
            }
        }
    }
}

impl std::error::Error for BrokerConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Target(error)
            | Self::Endpoint(error)
            | Self::Connect(error)
            | Self::InvalidPeer(error)
            | Self::Hello(error) => Some(error),
            Self::Readiness(error) => Some(error),
            Self::Deadline => None,
        }
    }
}

use super::channel::invalid_data;
use super::process::WorkerProcess;
use super::UnixIdentityChannel;
use lasper_guest_protocol::{
    GuestIdentityReply, PrepareRequest, PrepareRequestFrame, IDENTITY_CHALLENGE_BYTES,
};
use std::io;
use std::sync::Arc;

/// Keeps the identity channel and its request together for Prepare and Recheck.
/// The worker is assigned before the first reply. Reported guest IDs must map
/// to its kernel credentials; machine membership still needs checking before
/// desktop authorization. Account-name resolution belongs to the trusted
/// guest helper, not this credential check.
pub struct IdentityWorker<'payload> {
    channel: UnixIdentityChannel,
    request: Arc<PrepareRequestFrame<'payload>>,
    process: WorkerProcess,
    observed_ids: Option<ObservedIds>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ObservedIds {
    host_uid: u32,
    host_gid: u32,
    guest_uid: u32,
    guest_gid: u32,
}

impl<'payload> IdentityWorker<'payload> {
    pub(crate) fn new(
        channel: UnixIdentityChannel,
        request: Arc<PrepareRequestFrame<'payload>>,
        process: WorkerProcess,
    ) -> Self {
        Self {
            channel,
            request,
            process,
            observed_ids: None,
        }
    }

    pub fn process(&self) -> &WorkerProcess {
        &self.process
    }

    pub fn can_continue(&self) -> bool {
        self.channel.can_continue()
    }

    pub(crate) fn invalidate(&mut self) {
        self.channel.invalidate();
    }

    /// The caller owns the deadline. Cancellation invalidates the channel,
    /// including cancellation between receiving a reply and verifying procfs.
    pub async fn observe(&mut self) -> io::Result<WorkerObservation> {
        if !self.channel.can_continue() {
            return Err(invalid_data("identity worker cannot be reused"));
        }
        let mut check = WorkerCheck {
            channel: &mut self.channel,
            completed: false,
        };
        let result = observe_worker(check.channel, self.request.request(), &self.process)
            .await
            .and_then(|observation| {
                let ids = ObservedIds {
                    host_uid: observation.host_uid,
                    host_gid: observation.host_gid,
                    guest_uid: observation.reply.guest_uid(),
                    guest_gid: observation.reply.guest_gid(),
                };
                if self.observed_ids.is_some_and(|previous| previous != ids) {
                    return Err(invalid_data("worker identity changed during recheck"));
                }
                self.observed_ids = Some(ids);
                Ok(observation)
            });
        if result.is_ok() {
            check.completed = true;
        }
        result
    }
}

/// A guest report whose IDs map to the kernel credentials of its live sender.
/// The process has not yet been matched to a target machine. The account name
/// matches the request, but its relationship to these IDs relies on the guest
/// helper resolving and switching to that account correctly.
#[derive(Debug)]
pub struct WorkerObservation {
    reply: GuestIdentityReply,
    host_uid: u32,
    host_gid: u32,
}

impl WorkerObservation {
    pub fn reply(&self) -> &GuestIdentityReply {
        &self.reply
    }
    pub const fn host_uid(&self) -> u32 {
        self.host_uid
    }
    pub const fn host_gid(&self) -> u32 {
        self.host_gid
    }
}

async fn observe_worker(
    channel: &mut UnixIdentityChannel,
    request: &PrepareRequest<'_>,
    process: &WorkerProcess,
) -> io::Result<WorkerObservation> {
    process.recheck()?;
    let (reply, credentials) = channel.observe(fresh_nonce()?, request).await?;
    process.verify_identity(&credentials, reply.guest_uid(), reply.guest_gid())?;
    Ok(WorkerObservation {
        reply,
        host_uid: credentials.uid(),
        host_gid: credentials.gid(),
    })
}

struct WorkerCheck<'a> {
    channel: &'a mut UnixIdentityChannel,
    completed: bool,
}

impl Drop for WorkerCheck<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.channel.invalidate();
        }
    }
}

fn fresh_nonce() -> io::Result<[u8; IDENTITY_CHALLENGE_BYTES]> {
    let mut nonce = [0; IDENTITY_CHALLENGE_BYTES];
    let mut remaining = nonce.as_mut_slice();
    while !remaining.is_empty() {
        // SAFETY: getrandom writes only into the remaining part of this array.
        let read = unsafe {
            libc::getrandom(
                remaining.as_mut_ptr().cast(),
                remaining.len(),
                libc::GRND_NONBLOCK,
            )
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "getrandom returned no data",
            ));
        }
        remaining = &mut remaining[read as usize..];
    }
    Ok(nonce)
}

#[cfg(test)]
#[path = "../../tests/identity/worker.rs"]
mod tests;

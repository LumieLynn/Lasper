use core::marker::PhantomData;
use lasper_guest_protocol::PrepareReadyFrame;

use super::lifecycle::ReadyTransition;
use super::linux::Descriptor;

/// Keeps Ready resources and its lifecycle transition together. Only a
/// successful delivery enables Start; dropping or failing delivery closes
/// these endpoint copies and leaves the guardian responsible for cleanup.
#[must_use = "deliver Ready or drop it to cancel the preparation"]
pub struct WorkerReady<'a> {
    resources: ReadyResources<'a>,
    transition: ReadyTransition<'a>,
}

impl<'a> WorkerReady<'a> {
    pub(super) fn new(resources: ReadyResources<'a>, transition: ReadyTransition<'a>) -> Self {
        Self {
            resources,
            transition,
        }
    }

    pub const fn frame(&self) -> PrepareReadyFrame {
        self.resources.frame()
    }

    pub fn descriptors(&self) -> WorkerDescriptors<'_> {
        self.resources.descriptors()
    }

    /// The callback must return success only after the complete frame and its
    /// descriptors have been sent. Encoding or transport errors must propagate;
    /// even a partial send cancels this preparation rather than enabling Start.
    pub fn deliver<T, E>(
        self,
        send: impl FnOnce(PrepareReadyFrame, WorkerDescriptors<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let Self {
            resources,
            transition,
        } = self;
        let result = send(resources.frame(), resources.descriptors())?;
        transition.complete();
        Ok(result)
    }
}

// PTY/identity move out of the guardian for delivery. The pidfd stays borrowed
// because the guardian still needs its own copy to observe the worker's exit.
pub(super) struct ReadyResources<'a> {
    frame: PrepareReadyFrame,
    terminal: Option<Descriptor>,
    identity: Descriptor,
    pidfd: &'a Descriptor,
}

impl<'a> ReadyResources<'a> {
    pub(super) fn new(
        frame: PrepareReadyFrame,
        terminal: Option<Descriptor>,
        identity: Descriptor,
        pidfd: &'a Descriptor,
    ) -> Self {
        Self {
            frame,
            terminal,
            identity,
            pidfd,
        }
    }

    const fn frame(&self) -> PrepareReadyFrame {
        self.frame
    }

    fn descriptors(&self) -> WorkerDescriptors<'_> {
        let (fds, length) = match &self.terminal {
            Some(terminal) => ([terminal.raw(), self.identity.raw(), self.pidfd.raw()], 3),
            None => ([self.identity.raw(), self.pidfd.raw(), -1], 2),
        };
        WorkerDescriptors {
            fds,
            length,
            owner: PhantomData,
        }
    }
}

/// A wire-order view that cannot outlive its Ready delivery owner.
pub struct WorkerDescriptors<'a> {
    fds: [i32; 3],
    length: usize,
    owner: PhantomData<&'a Descriptor>,
}

impl WorkerDescriptors<'_> {
    pub fn as_raw_fds(&self) -> &[i32] {
        &self.fds[..self.length]
    }
}

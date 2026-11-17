mod context;
mod hello;
mod supervisor;

use crate::diagnostic::Diagnostic;
use core::fmt::Write;
use lasper_guest_protocol::BrokerId;

use crate::linux::uapi::errno::{EAGAIN, EINTR};
use crate::transport::GuestControlStream;

use super::error::SystemResultExt;
use super::GuardianError;
use crate::linux::signals::{BlockedSignals, Signals};
use crate::linux::{self, Descriptor, PollFd};

use crate::SupervisorSignal;
pub use context::GuardianContext;
use supervisor::Supervisor;
pub use supervisor::SupervisorStatus;

pub const GUARDIAN_START_BYTE: u8 = 1;

/// Initializes a guardian child before any worker is created.
pub struct GuardianBootstrap {
    control: i32,
    broker: i32,
    broker_id: BrokerId,
    accepted_at_ms: u64,
}

impl GuardianBootstrap {
    /// Waits for the broker's Go, completes Hello and calls `entry`.
    ///
    /// The Hello policy sets the deadline relative to `accepted_at_ms`, in
    /// `CLOCK_MONOTONIC` milliseconds. Failure exits without creating a worker.
    ///
    /// # Safety
    ///
    /// Call only in a single-threaded fork child with no children of its own.
    /// Keep it single-threaded with exclusive child creation and reaping while
    /// the context is live. Entry and worker callbacks must not unwind, and their
    /// call chain must reserve `WORKER_STACK_BUDGET_BYTES` for worker setup and exec.
    ///
    /// `control` must be a connected Unix stream. `broker` must be the read end
    /// of this guardian's broker liveness pipe. They must be distinct FDs >= 3.
    /// This call takes exclusive ownership of both. The caller and callbacks
    /// must not use or close them through old raw FDs or owners. Initialization
    /// also closes unrelated inherited FDs, so their owners on the abandoned
    /// caller stack must never be used or dropped.
    pub unsafe fn run(
        control: i32,
        broker: i32,
        broker_id: BrokerId,
        accepted_at_ms: u64,
        entry: fn(GuardianContext) -> i32,
    ) -> ! {
        let initialized = BlockedSignals::block()
            .context("block guardian startup signals")
            .and_then(|signals| {
                Self {
                    control,
                    broker,
                    broker_id,
                    accepted_at_ms,
                }
                .initialize(signals)
            });
        let code = match initialized {
            Ok(context) => entry(context),
            Err(error) => {
                let _ = writeln!(
                    Diagnostic,
                    "lasper guest runtime: guardian bootstrap failed: {error:?}"
                );
                error.class().exit_code()
            }
        };
        linux::exit(code)
    }

    fn initialize(self, signals: BlockedSignals) -> Result<GuardianContext, GuardianError> {
        if self.control < 3 || self.broker < 3 || self.control == self.broker {
            return Err(GuardianError::InvalidBootstrapDescriptors);
        }
        linux::validate_descriptor(self.control).context("validate guardian control socket")?;
        linux::validate_descriptor(self.broker).context("validate guardian broker pipe")?;
        linux::close_unrelated(&[self.control, self.broker])
            .context("close inherited broker FDs")?;

        // Owners are created only after pruning; none can hold a closed FD that
        // the new guardian might subsequently reuse.
        let control =
            Descriptor::adopt(self.control as isize).context("own guardian control socket")?;
        let broker = Descriptor::adopt(self.broker as isize).context("own guardian broker pipe")?;
        control
            .cloexec()
            .context("configure guardian control socket")?;
        broker.cloexec().context("configure guardian broker pipe")?;
        broker
            .nonblocking()
            .context("make guardian broker pipe nonblocking")?;
        let signals = signals.finish().context("initialize guardian signals")?;
        linux::set_subreaper().context("set guardian subreaper")?;
        let hello_deadline_ms = self
            .accepted_at_ms
            .checked_add(crate::policy::hello_timeout_ms())
            .ok_or(GuardianError::StartupExpired)?;
        wait_for_start(&broker, &signals, hello_deadline_ms)?;
        // SAFETY: The connected FD has one fresh owner in this child.
        let mut connection = unsafe { GuestControlStream::from_raw_fd(control.into_raw()) };
        let supervisor = Supervisor::new(broker, signals);
        let hello = hello::exchange(
            &mut connection,
            &supervisor,
            self.broker_id,
            hello_deadline_ms,
        )?;
        Ok(GuardianContext::new(
            connection,
            supervisor,
            self.broker_id,
            hello,
        ))
    }
}

fn wait_for_start(
    broker: &Descriptor,
    signals: &Signals,
    deadline_ms: u64,
) -> Result<(), GuardianError> {
    loop {
        let now = linux::monotonic_ms().context("read guardian startup clock")?;
        if now >= deadline_ms {
            return Err(GuardianError::StartupExpired);
        }
        if let Some(SupervisorSignal::Stop(signal)) =
            signals.receive().context("read guardian startup signal")?
        {
            return Err(GuardianError::SupervisorStopped(signal));
        }
        let mut byte = [0];
        match broker.read(&mut byte) {
            Ok(1) if byte[0] == GUARDIAN_START_BYTE => return Ok(()),
            Ok(0) => return Err(GuardianError::BrokerDisconnected),
            Ok(_) => return Err(GuardianError::StartupNotReleased),
            Err(EAGAIN) => {}
            Err(errno) => {
                return Err(GuardianError::System {
                    operation: "read guardian startup gate",
                    errno,
                })
            }
        }
        let mut events = [PollFd::new(broker.raw()), PollFd::new(signals.raw())];
        match linux::poll(&mut events, Some(deadline_ms - now)) {
            Err(EINTR) => continue,
            result => result.context("wait for guardian startup gate")?,
        }
    }
}

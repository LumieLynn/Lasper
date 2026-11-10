mod context;
mod hello;
mod supervisor;

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
const HELLO_TIMEOUT_MS: u64 = 1000;

/// The only transition from a broker child into a guardian. Inherited FD owners
/// on the broker's stack must never be dropped after the whitelist is applied.
pub struct GuardianBootstrap {
    control: i32,
    broker: i32,
    broker_id: BrokerId,
    accepted_at_ms: u64,
}

impl GuardianBootstrap {
    /// Completes Go and Hello within one second of accept before calling entry.
    /// Failure exits this child without creating a worker.
    ///
    /// # Safety
    ///
    /// Call only in the single-threaded fork child, which owns no children yet.
    /// `control` must be a connected Unix stream; `broker` must be the read end
    /// of this guardian's broker liveness pipe. They must be distinct FDs >= 3.
    /// This call takes exclusive ownership of both: the caller and callbacks
    /// must not use or close them through old raw FDs or Rust owners. Inherited
    /// owners may remain on the abandoned caller stack but must never be dropped.
    /// `accepted_at_ms` uses CLOCK_MONOTONIC milliseconds.
    /// While the context is live, keep this process single-threaded and retain
    /// exclusive child creation/reaping. Entry and worker callbacks must not
    /// unwind or use/drop inherited FD owners. Their call chain must reserve
    /// WORKER_STACK_BUDGET_BYTES for worker setup and exec.
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
                70
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
        let hello_deadline_ms = self.accepted_at_ms.saturating_add(HELLO_TIMEOUT_MS);
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

struct Diagnostic;

impl core::fmt::Write for Diagnostic {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let mut bytes = text.as_bytes();
        while !bytes.is_empty() {
            match linux::write(2, bytes) {
                Ok(0) | Err(_) => return Err(core::fmt::Error),
                Ok(count) => bytes = &bytes[count..],
            }
        }
        Ok(())
    }
}

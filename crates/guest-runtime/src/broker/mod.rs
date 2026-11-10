mod listener;
mod registry;

use core::ffi::CStr;
use core::fmt::Write;
use lasper_guest_protocol::BrokerId;

use crate::linux::children::visit_direct_children;
use crate::linux::signals::{BlockedSignals, Signals};
use crate::linux::uapi::{
    errno::{EAGAIN, ECHILD, EINTR, EIO, ESRCH},
    nr,
    random::GRND_NONBLOCK,
    signal::SIGKILL,
};
use crate::linux::{self, Descriptor, Pipe, PollFd, ProcessExit};
use crate::readiness::GuestWorkerReadiness;
use crate::session::{GuardianBootstrap, GuardianContext};
use crate::syscall::syscall3;
use crate::SupervisorSignal;
use listener::Listener;
use registry::Registry;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerError {
    pub operation: &'static str,
    pub errno: i32,
}

trait BrokerResult<T> {
    fn at(self, operation: &'static str) -> Result<T, BrokerError>;
}

impl<T> BrokerResult<T> for Result<T, i32> {
    fn at(self, operation: &'static str) -> Result<T, BrokerError> {
        self.map_err(|errno| BrokerError { operation, errno })
    }
}

/// Runs the guest-side listener and owns every guardian until wait completes.
/// The socket directory must already exist, be owned by this guest's root, and
/// have mode 0700. A lock excludes another broker before a stale socket is removed.
///
/// # Safety
/// Call from a single-threaded process with no children and exclusive child
/// creation/reaping. The process must exit after this function returns: signal
/// dispositions, the blocked mask and subreaper status are intentionally retained.
/// Reserve WORKER_STACK_BUDGET_BYTES for the forked worker's call chain.
/// Inherited FD owners on the caller stack must not be used or dropped after
/// this call; startup prunes them. stderr is the launcher's diagnostic FD.
pub unsafe fn run(directory: &CStr) -> Result<(), BrokerError> {
    let blocked = BlockedSignals::block().at("block broker signals")?;
    standard_streams().at("initialize broker standard streams")?;
    let signals = blocked.finish().at("initialize broker signals")?;
    let mut existing = false;
    visit_direct_children(linux::pid(), linux::tid(), |_| {
        existing = true;
        Ok(())
    })
    .at("check broker child ownership")?;
    if existing {
        return Err(BrokerError {
            operation: "broker already has children",
            errno: ECHILD,
        });
    }
    linux::set_subreaper().at("set broker subreaper")?;
    GuestWorkerReadiness::check().map_err(|error| BrokerError {
        operation: "check broker pidfd support",
        errno: match error {
            crate::readiness::GuestReadinessError::WorkerPidfdUnavailable { errno } => errno,
        },
    })?;
    let id = broker_id().at("generate broker ID")?;
    let listener = Listener::bind(directory).at("create broker listener")?;
    let [socket, directory, lock] = listener.descriptors();
    linux::close_unrelated(&[socket, directory, lock, signals.raw()])
        .at("close inherited broker descriptors")?;
    Broker {
        listener: Some(listener),
        signals,
        registry: Registry::new(),
        id,
        failure: None,
        has_orphans: false,
        signals_usable: true,
    }
    .drive()
}

fn standard_streams() -> Result<(), i32> {
    let null = linux::null_device()?;
    null.duplicate_to(0)?;
    null.duplicate_to(1)?;
    match linux::validate_descriptor(2) {
        Ok(()) => Ok(()),
        Err(crate::linux::uapi::errno::EBADF) => null.duplicate_to(2),
        Err(errno) => Err(errno),
    }
}

struct Broker {
    listener: Option<Listener>,
    signals: Signals,
    registry: Registry,
    id: BrokerId,
    failure: Option<BrokerError>,
    has_orphans: bool,
    signals_usable: bool,
}

impl Broker {
    fn drive(mut self) -> Result<(), BrokerError> {
        loop {
            if let Err(error) = self.advance() {
                self.failure.get_or_insert(error);
                self.stop();
            }
            if self.listener.is_none() && self.registry.is_empty() {
                match linux::reap_one() {
                    Err(ECHILD) => return self.failure.map_or(Ok(()), Err),
                    Ok(Some(child)) => {
                        self.reaped(child);
                        continue;
                    }
                    Ok(None) => {}
                    Err(errno) => {
                        self.failure.get_or_insert(BrokerError {
                            operation: "wait broker children",
                            errno,
                        });
                    }
                }
            }
            let mut events = [PollFd::new(-1), PollFd::new(-1)];
            if self.signals_usable {
                events[0] = PollFd::new(self.signals.raw());
            }
            if let Some(listener) = &self.listener {
                events[1] = PollFd::new(listener.raw());
            }
            // A killed guardian can leave adopted children after its SIGCHLD
            // has been consumed. Revisit them without waiting for another exit.
            let retry = (self.has_orphans || !self.signals_usable).then_some(50);
            if let Err(errno) = linux::poll(&mut events, retry) {
                if errno != EINTR {
                    self.failure.get_or_insert(BrokerError {
                        operation: "poll broker",
                        errno,
                    });
                    self.stop();
                }
            }
        }
    }

    fn advance(&mut self) -> Result<(), BrokerError> {
        for _ in 0..32 {
            if !self.signals_usable {
                break;
            }
            match self.signals.receive() {
                Ok(Some(SupervisorSignal::Stop(_))) => self.stop(),
                Ok(Some(SupervisorSignal::ChildChanged)) => {}
                Ok(None) => break,
                Err(errno) => {
                    self.failure.get_or_insert(BrokerError {
                        operation: "read broker signal",
                        errno,
                    });
                    self.signals_usable = false;
                    self.stop();
                    break;
                }
            }
        }
        self.reap()?;
        self.has_orphans = self.clean_orphans()?;
        for _ in 0..8 {
            let Some(listener) = &self.listener else {
                break;
            };
            let control = match listener.accept() {
                Ok(control) => control,
                Err(EAGAIN) => break,
                Err(errno) => {
                    return Err(BrokerError {
                        operation: "accept broker connection",
                        errno,
                    })
                }
            };
            let accepted_at = linux::monotonic_ms().at("read connection acceptance clock")?;
            let Some(slot) = self.registry.reserve() else {
                let _ = writeln!(
                    Diagnostic,
                    "lasper guest runtime: broker guardian capacity exhausted"
                );
                continue;
            };
            if let Err(error) = Self::spawn(self.id, slot, control, accepted_at) {
                let _ = writeln!(
                    Diagnostic,
                    "lasper guest runtime: guardian admission failed: {error:?}"
                );
            }
        }
        Ok(())
    }

    fn spawn(
        id: BrokerId,
        slot: registry::Reservation<'_>,
        control: Descriptor,
        accepted_at: u64,
    ) -> Result<(), BrokerError> {
        let pipe = Pipe::new().at("create guardian liveness pipe")?;
        // SAFETY: run requires a single-threaded, exclusive child owner. The
        // child immediately enters the non-returning FD-pruning bootstrap.
        let pid = unsafe { linux::fork() }.at("fork guardian")?;
        if pid == 0 {
            unsafe {
                GuardianBootstrap::run(
                    control.raw(),
                    pipe.reader.raw(),
                    id,
                    accepted_at,
                    session_entry,
                )
            }
        }
        drop(control);
        drop(pipe.reader);
        slot.register(pid, pipe.writer)
            .at("register and release guardian")
    }

    fn reap(&mut self) -> Result<(), BrokerError> {
        for _ in 0..256 {
            match linux::reap_one() {
                Ok(Some(child)) => self.reaped(child),
                Ok(None) | Err(ECHILD) => break,
                Err(errno) => {
                    return Err(BrokerError {
                        operation: "reap broker child",
                        errno,
                    })
                }
            }
        }
        Ok(())
    }

    fn reaped(&mut self, child: linux::ReapedChild) {
        let guardian = self.registry.reaped(child.pid);
        if guardian && child.exit != ProcessExit::Exited(0) {
            let _ = writeln!(Diagnostic, "lasper guest runtime: guardian {} exited {:?}; adopted descendants will be cleaned", child.pid, child.exit);
        }
    }

    fn clean_orphans(&self) -> Result<bool, BrokerError> {
        let mut found = false;
        visit_direct_children(linux::pid(), linux::tid(), |pid| {
            if self.registry.contains(pid) {
                return Ok(());
            }
            found = true;
            // These are our unreaped direct children, so their numeric PIDs
            // cannot be reused between enumeration and signal delivery.
            match linux::signal(pid, SIGKILL) {
                Ok(()) | Err(ESRCH) => Ok(()),
                Err(errno) => Err(errno),
            }
        })
        .at("clean adopted broker children")?;
        Ok(found)
    }

    fn stop(&mut self) {
        self.listener.take();
        if let Err(errno) = self.registry.stop() {
            self.failure.get_or_insert(BrokerError {
                operation: "resume guardians for shutdown",
                errno,
            });
        }
    }
}

fn session_entry(context: GuardianContext) -> i32 {
    match context.run_session() {
        Ok(()) => 0,
        Err(
            crate::session::GuardianError::Control(crate::transport::TransportError::UnexpectedEof)
            | crate::session::GuardianError::BrokerDisconnected
            | crate::session::GuardianError::SupervisorStopped(_),
        ) => 0,
        Err(error) => {
            let _ = writeln!(
                Diagnostic,
                "lasper guest runtime: guardian session failed: {error:?}"
            );
            70
        }
    }
}

fn broker_id() -> Result<BrokerId, i32> {
    let mut bytes = [0; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: getrandom writes only into the remaining nonce storage.
        let result = unsafe {
            syscall3(
                nr::GETRANDOM,
                bytes[filled..].as_mut_ptr() as usize,
                bytes.len() - filled,
                GRND_NONBLOCK,
            )
        };
        if result == -(EINTR as isize) {
            continue;
        }
        let count = linux::result_or_errno(result)?;
        if count == 0 {
            return Err(EIO);
        }
        filled += count;
    }
    Ok(BrokerId::new(bytes))
}

pub(crate) struct Diagnostic;
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

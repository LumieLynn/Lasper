mod listener;
mod registry;

use crate::diagnostic::Diagnostic;
use core::ffi::CStr;
use core::fmt::Write;
use lasper_guest_protocol::BrokerId;

use crate::linux::children::visit_direct_children;
use crate::linux::signals::{BlockedSignals, Signals};
use crate::linux::uapi::{
    errno::{EAGAIN, ECHILD, EINTR, EIO, ESRCH, ETIMEDOUT},
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

pub(crate) trait BrokerResult<T> {
    fn at(self, operation: &'static str) -> Result<T, BrokerError>;
}

impl<T> BrokerResult<T> for Result<T, i32> {
    fn at(self, operation: &'static str) -> Result<T, BrokerError> {
        self.map_err(|errno| BrokerError { operation, errno })
    }
}

/// Runs the guest listener and owns its guardians until they are reaped.
///
/// The socket directory must already exist, be owned by this guest's root, and
/// have mode 0700. A lock excludes another broker before a stale socket is removed.
///
/// # Safety
///
/// Call from a single-threaded process with no children and exclusive child
/// creation and reaping. Reserve `WORKER_STACK_BUDGET_BYTES` for worker setup in
/// addition to the live broker and guardian frames.
///
/// Initialization closes inherited FDs. Their owners on the caller's stack must
/// not be used or dropped afterwards. The caller must exit when this returns
/// because signal state and subreaper status are not restored.
pub unsafe fn run(directory: &CStr) -> Result<(), BrokerError> {
    unsafe { run_inner(directory, None) }
}

/// Holds the broker's endpoints for the wrapper startup handshake.
///
/// The broker binds its listener and sends Ready before the parent attempts
/// `execve`. It waits for the exec-status result before accepting sessions.
pub(crate) struct Startup {
    /// Write end of the Ready pipe. The broker writes byte 1, then closes it.
    pub(crate) ready: Descriptor,
    /// Read end of the status socket. The parent sends a native-endian `i32` errno
    /// on failure. Successful `execve` closes its sole writer through CLOEXEC.
    pub(crate) exec_status: Descriptor,
    /// Pidfd for the parent process, which becomes the payload after `execve`.
    /// The broker checks it before accepting EOF as success and monitors it for exit.
    pub(crate) parent: Descriptor,
}

/// Runs the broker with the wrapper's startup protocol.
///
/// # Safety
///
/// The caller must satisfy [`run`]'s requirements and supply the broker-side
/// endpoints described by [`Startup`].
pub(crate) unsafe fn run_wrapped(directory: &CStr, startup: Startup) -> Result<(), BrokerError> {
    unsafe { run_inner(directory, Some(startup)) }
}

unsafe fn run_inner(directory: &CStr, startup: Option<Startup>) -> Result<(), BrokerError> {
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
    let [ready, exec_status, parent] = startup.as_ref().map_or([-1; 3], |startup| {
        [
            startup.ready.raw(),
            startup.exec_status.raw(),
            startup.parent.raw(),
        ]
    });
    let keep = [
        socket,
        directory,
        lock,
        signals.raw(),
        ready,
        exec_status,
        parent,
    ];
    linux::close_unrelated(&keep).at("close inherited broker descriptors")?;
    let parent = startup
        .map(|startup| startup.handoff(&signals))
        .transpose()?;
    Broker {
        listener: Some(listener),
        signals,
        registry: Registry::new(),
        id,
        failure: None,
        has_orphans: false,
        signals_usable: true,
        parent,
    }
    .drive()
}

impl Startup {
    fn handoff(self, signals: &Signals) -> Result<Descriptor, BrokerError> {
        self.ready.write_all(&[1]).at("report broker Ready")?;
        drop(self.ready);
        let deadline = linux::monotonic_ms()
            .at("read handoff clock")?
            .saturating_add(crate::wrapper::STARTUP_TIMEOUT_MS);
        loop {
            let remaining =
                deadline.saturating_sub(linux::monotonic_ms().at("read handoff clock")?);
            if remaining == 0 {
                return Err(BrokerError {
                    operation: "wait payload exec",
                    errno: ETIMEDOUT,
                });
            }
            let mut events = [
                PollFd::new(self.exec_status.raw()),
                PollFd::new(self.parent.raw()),
                PollFd::new(signals.raw()),
            ];
            match linux::poll(&mut events, Some(remaining)) {
                Ok(()) => {}
                Err(EINTR) => continue,
                Err(errno) => {
                    return Err(BrokerError {
                        operation: "poll payload handoff",
                        errno,
                    })
                }
            }
            if events[1].ready() || events[2].ready() {
                return Err(BrokerError {
                    operation: "wrapper stopped before handoff",
                    errno: EIO,
                });
            }
            if events[0].ready() {
                let mut error = [0; 4];
                let count = self
                    .exec_status
                    .read(&mut error)
                    .at("read payload exec status")?;
                if count != 0 {
                    let errno = i32::from_ne_bytes(error);
                    return Err(BrokerError {
                        operation: "payload exec failed",
                        errno: if count == 4 && (1..=4095).contains(&errno) {
                            errno
                        } else {
                            EIO
                        },
                    });
                }
                // Parent death also closes the channel. EOF indicates successful
                // execve only if the pinned parent process is still alive.
                let mut parent = [PollFd::new(self.parent.raw())];
                linux::poll(&mut parent, Some(0)).at("check payload lifetime")?;
                if parent[0].ready() {
                    return Err(BrokerError {
                        operation: "payload exited during handoff",
                        errno: EIO,
                    });
                }
                return Ok(self.parent);
            }
        }
    }
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
    parent: Option<Descriptor>,
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
            let mut events = [PollFd::new(-1), PollFd::new(-1), PollFd::new(-1)];
            if self.signals_usable {
                events[0] = PollFd::new(self.signals.raw());
            }
            if let Some(listener) = &self.listener {
                events[1] = PollFd::new(listener.raw());
            }
            if let Some(parent) = &self.parent {
                events[2] = PollFd::new(parent.raw());
            }
            // A guardian can leave adopted children after its SIGCHLD was consumed.
            // Periodic polls keep cleanup moving without waiting for another exit.
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
        if let Some(parent) = &self.parent {
            let mut events = [PollFd::new(parent.raw())];
            linux::poll(&mut events, Some(0)).at("check payload lifetime")?;
            if events[0].ready() {
                self.parent = None;
                self.stop();
            }
        }
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

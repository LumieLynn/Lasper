use crate::linux::uapi::{
    errno::{EAGAIN, EIO},
    signal::{SIGCHLD, SIGINT, SIGPIPE, SIGQUIT, SIGTERM},
};
use crate::linux::{self, Descriptor};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorSignal {
    ChildChanged,
    Stop(i32),
}

pub(crate) struct Signals(Descriptor);

// This token carries signal setup across FD pruning without owning a new FD.
#[must_use = "finish signal setup; dropping this token leaves signals blocked"]
pub(crate) struct BlockedSignals {
    _private: (),
}

const MANAGED_MASK: u64 =
    (1 << (SIGINT - 1)) | (1 << (SIGQUIT - 1)) | (1 << (SIGTERM - 1)) | (1 << (SIGCHLD - 1));

impl BlockedSignals {
    pub(crate) fn block() -> Result<Self, i32> {
        linux::block_signals(MANAGED_MASK | (1 << (SIGPIPE - 1)))?;
        linux::ignore_sigpipe()?;
        Ok(Self { _private: () })
    }

    pub(crate) fn finish(self) -> Result<Signals, i32> {
        linux::supervisor_dispositions()?;
        linux::signal_fd(MANAGED_MASK).map(Signals)
    }
}

impl Signals {
    pub(crate) fn raw(&self) -> i32 {
        self.0.raw()
    }

    pub(crate) fn receive(&self) -> Result<Option<SupervisorSignal>, i32> {
        // signalfd records are always 128 bytes; only the first u32 is needed here.
        let mut record = [0; 128];
        match self.0.read(&mut record) {
            Err(EAGAIN) => Ok(None),
            Ok(128) => match i32::from_ne_bytes([record[0], record[1], record[2], record[3]]) {
                SIGCHLD => Ok(Some(SupervisorSignal::ChildChanged)),
                signal @ (SIGINT | SIGQUIT | SIGTERM) => Ok(Some(SupervisorSignal::Stop(signal))),
                _ => Err(EIO),
            },
            Ok(_) => Err(EIO),
            Err(errno) => Err(errno),
        }
    }
}

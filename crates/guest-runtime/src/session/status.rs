use super::linux::{Descriptor, EAGAIN};
use super::GuardianError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum WorkerStage {
    Descriptors = 1,
    Session = 2,
    StandardIo = 3,
    Account = 4,
    Start = 5,
    Exec = 6,
    Signals = 7,
    Projection = 8,
}

impl WorkerStage {
    fn decode(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Descriptors),
            2 => Some(Self::Session),
            3 => Some(Self::StandardIo),
            4 => Some(Self::Account),
            5 => Some(Self::Start),
            6 => Some(Self::Exec),
            7 => Some(Self::Signals),
            8 => Some(Self::Projection),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerFailure {
    pub stage: WorkerStage,
    pub errno: i32,
}

impl WorkerFailure {
    pub const fn new(stage: WorkerStage, errno: i32) -> Self {
        Self { stage, errno }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StatusMessage {
    SessionEstablished,
    Prepared,
    Failed(WorkerFailure),
}

impl StatusMessage {
    pub(super) fn send(self, pipe: &Descriptor) -> Result<(), i32> {
        let (stage, errno) = match self {
            Self::SessionEstablished => (u32::MAX, 0),
            Self::Prepared => (0, 0),
            Self::Failed(failure) => (failure.stage as u32, failure.errno),
        };
        let mut bytes = [0; 8];
        bytes[..4].copy_from_slice(&stage.to_le_bytes());
        bytes[4..].copy_from_slice(&errno.to_le_bytes());
        pipe.write_all(&bytes)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Setup {
    Creating,
    SessionEstablished,
    Prepared,
}

/// The pipe spans setup and exec. Eight-byte records report progress/failure;
/// a clean EOF is interpreted by the process owner together with wait status.
pub(super) struct WorkerStatus {
    pipe: Descriptor,
    bytes: [u8; 8],
    filled: usize,
    closed: bool,
    setup: Setup,
    failure: Option<WorkerFailure>,
}

impl WorkerStatus {
    pub(super) fn new(pipe: Descriptor) -> Self {
        Self {
            pipe,
            bytes: [0; 8],
            filled: 0,
            closed: false,
            setup: Setup::Creating,
            failure: None,
        }
    }

    pub(super) const fn failure(&self) -> Option<WorkerFailure> {
        self.failure
    }
    pub(super) const fn closed(&self) -> bool {
        self.closed
    }
    pub(super) fn prepared(&self) -> bool {
        self.setup == Setup::Prepared
    }
    pub(super) fn session_established(&self) -> bool {
        self.setup != Setup::Creating
    }
    pub(super) fn descriptor(&self) -> i32 {
        if self.closed {
            -1
        } else {
            self.pipe.raw()
        }
    }

    pub(super) fn poll(&mut self) -> Result<(), GuardianError> {
        while !self.closed {
            match self.pipe.read(&mut self.bytes[self.filled..]) {
                Ok(0) => {
                    self.closed = true;
                    if self.filled != 0 {
                        return Err(GuardianError::InvalidStatus);
                    }
                }
                Ok(count) => {
                    self.filled += count;
                    if self.filled == self.bytes.len() {
                        self.filled = 0;
                        self.apply_record()?;
                    }
                }
                Err(EAGAIN) => break,
                Err(errno) => {
                    return Err(GuardianError::System {
                        operation: "read worker status",
                        errno,
                    })
                }
            }
        }
        Ok(())
    }

    fn apply_record(&mut self) -> Result<(), GuardianError> {
        if self.failure.is_some() {
            return Err(GuardianError::InvalidStatus);
        }
        let stage = u32::from_le_bytes(self.bytes[..4].try_into().unwrap());
        let errno = i32::from_le_bytes(self.bytes[4..].try_into().unwrap());
        match (stage, errno, self.setup) {
            (u32::MAX, 0, Setup::Creating) => self.setup = Setup::SessionEstablished,
            (0, 0, Setup::SessionEstablished) => self.setup = Setup::Prepared,
            _ => {
                let stage = WorkerStage::decode(stage)
                    .filter(|_| errno > 0)
                    .ok_or(GuardianError::InvalidStatus)?;
                self.failure = Some(WorkerFailure::new(stage, errno));
            }
        }
        Ok(())
    }
}

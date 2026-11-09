use super::super::{error::SystemResultExt, GuardianError, SessionGuardian};
use crate::linux::uapi::errno::EAGAIN;
use crate::memory::Mapping;
use crate::transport::IoBudget;
use lasper_guest_protocol::{CapturedOutput, ControlLimits, OutputStream};

const OUTPUT_DRAIN_TIMEOUT_MS: u64 = 1_000;

pub(super) struct Capture {
    streams: [Stream; 2],
    remaining: usize,
    drain_deadline: Option<u64>,
    next_stream: usize,
}

struct Stream {
    storage: Option<Mapping>,
    length: usize,
    state: StreamState,
    truncated: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum StreamState {
    Open,
    Eof,
    Abandoned,
}

impl Stream {
    fn new(capacity: usize) -> Result<Self, GuardianError> {
        Ok(Self {
            storage: if capacity == 0 {
                None
            } else {
                Some(Mapping::new(capacity).context("allocate bounded output")?)
            },
            length: 0,
            state: StreamState::Open,
            truncated: false,
        })
    }

    fn view(&self) -> Result<OutputStream<'_>, GuardianError> {
        OutputStream::new(
            self.storage
                .as_ref()
                .map_or(&[][..], |bytes| &bytes.as_ref()[..self.length]),
            self.truncated,
            self.state == StreamState::Eof,
        )
        .map_err(|_| GuardianError::ReplyEncoding)
    }
}

impl Capture {
    pub(super) fn new(limits: ControlLimits) -> Result<Self, GuardianError> {
        let capacity = CapturedOutput::budget(limits);
        // Each stream can use the whole budget. Only `remaining` bytes may be
        // written across both mappings; unused anonymous pages stay untouched.
        Ok(Self {
            streams: [Stream::new(capacity)?, Stream::new(capacity)?],
            remaining: capacity,
            drain_deadline: None,
            next_stream: 0,
        })
    }

    pub(super) fn view(&self) -> Result<CapturedOutput<'_>, GuardianError> {
        CapturedOutput::new(self.streams[0].view()?, self.streams[1].view()?)
            .map_err(|_| GuardianError::ReplyEncoding)
    }

    pub(super) fn finished(&self) -> bool {
        self.streams
            .iter()
            .all(|stream| stream.state != StreamState::Open)
    }
    pub(super) const fn deadline_ms(&self) -> Option<u64> {
        self.drain_deadline
    }

    pub(super) fn after_cleanup(&mut self, now: u64) {
        if !self.finished() {
            self.drain_deadline
                .get_or_insert(now.saturating_add(OUTPUT_DRAIN_TIMEOUT_MS));
        }
    }

    pub(super) fn drain(
        &mut self,
        guardian: &mut SessionGuardian<'_, '_>,
        now: u64,
        budget: &mut IoBudget,
    ) -> Result<(), GuardianError> {
        let mut buffer = [0; 8192];
        for n in 0..2 {
            let index = (self.next_stream + n) % 2;
            let stream = &mut self.streams[index];
            if stream.state != StreamState::Open || !budget.available() {
                continue;
            }
            let length = budget.length(buffer.len());
            budget.attempted();
            let result = if index == 0 {
                guardian.read_stdout(&mut buffer[..length])
            } else {
                guardian.read_stderr(&mut buffer[..length])
            };
            let count = match result {
                Ok(count) => count,
                Err(GuardianError::System { errno: EAGAIN, .. }) => continue,
                Err(error) => return Err(error),
            };
            budget.transferred(count);
            if count == 0 {
                stream.state = StreamState::Eof;
                continue;
            }
            let keep = count.min(self.remaining);
            if keep != 0 {
                let bytes = stream
                    .storage
                    .as_mut()
                    .ok_or(GuardianError::ReplyEncoding)?;
                bytes.as_mut()[stream.length..stream.length + keep]
                    .copy_from_slice(&buffer[..keep]);
                stream.length += keep;
                self.remaining -= keep;
            }
            stream.truncated |= keep < count;
        }
        self.next_stream = (self.next_stream + 1) % 2;
        if self.drain_deadline.is_some_and(|deadline| now >= deadline) {
            self.abandon(guardian);
        }
        if self.finished() {
            self.drain_deadline = None;
        }
        Ok(())
    }

    pub(super) fn abandon(&mut self, guardian: &mut SessionGuardian<'_, '_>) {
        for stream in &mut self.streams {
            if stream.state == StreamState::Open {
                stream.state = StreamState::Abandoned;
                stream.truncated = true;
            }
        }
        self.drain_deadline = None;
        guardian.close_output();
    }
}

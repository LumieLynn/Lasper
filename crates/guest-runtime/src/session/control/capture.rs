use super::super::{error::SystemResultExt, GuardianError, SessionGuardian};
use crate::linux::uapi::errno::EAGAIN;
use crate::memory::Mapping;
use crate::transport::IoBudget;
use lasper_guest_protocol::{CapturedOutput, ControlLimits, OutputStream};

const OUTPUT_DRAIN_TIMEOUT_MS: u64 = 1_000;

pub(super) struct Capture {
    streams: [Stream; 2],
    remaining: usize,
    reserved: [usize; 2],
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
        // Either stream may borrow unused space. Its peer can reclaim that
        // space later without losing the peer's own reserved share.
        Ok(Self {
            streams: [Stream::new(capacity)?, Stream::new(capacity)?],
            remaining: capacity,
            reserved: [capacity / 2, capacity - capacity / 2],
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
            if self.streams[index].state != StreamState::Open || !budget.available() {
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
                self.streams[index].state = StreamState::Eof;
                continue;
            }
            self.retain(index, &buffer[..count])?;
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

    fn retain(&mut self, index: usize, bytes: &[u8]) -> Result<(), GuardianError> {
        let peer = 1 - index;
        let borrowed = self.streams[peer]
            .length
            .saturating_sub(self.reserved[peer]);
        let reclaim = bytes.len().saturating_sub(self.remaining).min(borrowed);
        if reclaim != 0 {
            // Remove the borrower's tail so its retained bytes remain a prefix.
            self.streams[peer].length -= reclaim;
            self.streams[peer].truncated = true;
            self.remaining += reclaim;
        }
        let stream = &mut self.streams[index];
        let keep = bytes.len().min(self.remaining);
        if keep != 0 {
            let storage = stream
                .storage
                .as_mut()
                .ok_or(GuardianError::ReplyEncoding)?;
            storage.as_mut()[stream.length..stream.length + keep].copy_from_slice(&bytes[..keep]);
            stream.length += keep;
            self.remaining -= keep;
        }
        stream.truncated |= keep < bytes.len();
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

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;

    #[test]
    fn late_stderr_reclaims_space_after_stdout_fills_the_budget() {
        let mut capture = Capture::new(ControlLimits::CURRENT).unwrap();
        let budget = capture.remaining;
        capture.retain(0, &vec![b'o'; budget]).unwrap();
        capture.streams[0].state = StreamState::Eof;
        capture.retain(1, b"late error").unwrap();
        let output = capture.view().unwrap();
        assert_eq!(output.stdout().len(), budget - b"late error".len());
        assert!(output.stdout().truncated());
        assert!(output.stdout().complete());
        assert!(!output.stderr().truncated());
        let mut bytes = [0; 10];
        output.stderr().write_into(&mut bytes).unwrap();
        assert_eq!(&bytes, b"late error");
    }

    #[test]
    fn either_stream_can_use_the_full_budget_while_its_peer_is_empty() {
        for index in 0..2 {
            let mut capture = Capture::new(ControlLimits::CURRENT).unwrap();
            let budget = capture.remaining;
            capture.retain(index, &vec![b'x'; budget]).unwrap();
            assert_eq!(capture.streams[index].length, budget);
            assert!(!capture.streams[index].truncated);
        }
    }

    #[test]
    fn both_streams_keep_their_share_regardless_of_arrival_order() {
        for first in 0..2 {
            let mut capture = Capture::new(ControlLimits::CURRENT).unwrap();
            let budget = capture.remaining;
            capture.retain(first, &vec![b'x'; budget + 1]).unwrap();
            capture.retain(1 - first, &vec![b'y'; budget + 1]).unwrap();
            capture.retain(first, b"discarded").unwrap();
            for index in 0..2 {
                assert_eq!(capture.streams[index].length, capture.reserved[index]);
                assert!(capture.streams[index].truncated);
            }
            assert_eq!(capture.remaining, 0);
            let output = capture.view().unwrap();
            assert_eq!(output.stdout().len() + output.stderr().len(), budget);
        }
    }
}

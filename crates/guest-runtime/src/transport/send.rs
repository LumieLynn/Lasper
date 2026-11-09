use core::task::Poll;
use lasper_guest_protocol::{ControlFrameHeader, CONTROL_FRAME_HEADER_BYTES};

use super::ancillary::send_message;
use super::descriptors::SendingDescriptors;
use super::{Direction, GuestControlStream, IoBudget, Operation, TransportError, EAGAIN, EINTR};

#[must_use]
pub struct FrameSender<'a, B> {
    operation: Operation<'a>,
    header: ControlFrameHeader,
    encoded: [u8; CONTROL_FRAME_HEADER_BYTES],
    payload: B,
    descriptors: Option<SendingDescriptors>,
    offset: usize,
}

impl<'a, B: AsRef<[u8]>> FrameSender<'a, B> {
    pub(super) fn new(
        stream: &'a GuestControlStream,
        header: ControlFrameHeader,
        payload: B,
        descriptors: &[i32],
    ) -> Result<Self, TransportError> {
        header
            .validate_shape(payload.as_ref().len(), descriptors.len())
            .map_err(TransportError::InvalidFrame)?;
        stream
            .session
            .get()
            .next_outbound(header)
            .map_err(TransportError::Session)?;
        let operation = Operation::new(stream, Direction::Send)?;
        let descriptors = SendingDescriptors::duplicate(descriptors)?;
        Ok(Self {
            operation,
            header,
            encoded: header.encode(),
            payload,
            descriptors: Some(descriptors),
            offset: 0,
        })
    }

    pub fn as_raw_fd(&self) -> i32 {
        self.operation.stream.fd
    }

    /// Pending keeps the unsent tail and any not-yet-transferred FD copies.
    /// A successful first write transfers SCM_RIGHTS exactly once.
    pub fn advance(&mut self) -> Result<Poll<()>, TransportError> {
        self.advance_with_budget(&mut IoBudget::new())
    }

    pub fn advance_with_budget(
        &mut self,
        budget: &mut IoBudget,
    ) -> Result<Poll<()>, TransportError> {
        match self.write_frame(budget) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.descriptors.take();
                Err(self.operation.fail(error))
            }
        }
    }

    fn write_frame(&mut self, budget: &mut IoBudget) -> Result<Poll<()>, TransportError> {
        self.operation.stream.ensure_usable()?;
        if self.operation.finished {
            return Ok(Poll::Ready(()));
        }
        let payload = self.payload.as_ref();
        self.header
            .validate_shape(payload.len(), self.header.descriptor_count() as usize)
            .map_err(TransportError::InvalidFrame)?;
        let total = self.encoded.len() + payload.len();
        while self.offset < total && budget.available() {
            let head = &self.encoded[self.offset.min(self.encoded.len())..];
            let body = &payload[self.offset.saturating_sub(self.encoded.len())..];
            let head = &head[..budget.length(head.len())];
            let body = &body[..body.len().min(budget.bytes - head.len())];
            let fds = self
                .descriptors
                .as_ref()
                .map_or(&[][..], SendingDescriptors::as_raw_fds);
            budget.attempted();
            let sent = match send_message(self.operation.stream.fd, head, body, fds) {
                Err(TransportError::Syscall(EAGAIN)) => return Ok(Poll::Pending),
                Err(TransportError::Syscall(EINTR)) => continue,
                result => result?,
            };
            if sent == 0 || sent > head.len() + body.len() {
                return Err(TransportError::WriteZero);
            }
            self.operation.started = true;
            self.descriptors.take();
            self.offset += sent;
            budget.transferred(sent);
        }
        if self.offset == total {
            self.operation.finish(self.header)?;
            Ok(Poll::Ready(()))
        } else {
            Ok(Poll::Pending)
        }
    }
}

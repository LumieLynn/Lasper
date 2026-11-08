use core::task::Poll;
use lasper_guest_protocol::{
    ControlFrameHeader, CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES, CONTROL_FRAME_HEADER_BYTES,
};

use super::ancillary::receive_chunk;
use super::descriptors::RawDescriptors;
use super::{
    Direction, GuestControlStream, IoBudget, Operation, ReceivedDescriptors, TransportError,
    EAGAIN, EINTR,
};

/// Reads only the header. Its validated length lets the caller allocate Prepare
/// storage before handing ownership of that storage to the payload receiver.
#[must_use]
pub struct FrameReceiver<'a> {
    operation: Operation<'a>,
    bytes: [u8; CONTROL_FRAME_HEADER_BYTES],
    offset: usize,
    descriptors: Option<RawDescriptors>,
    header: Option<ControlFrameHeader>,
}

impl<'a> FrameReceiver<'a> {
    pub(super) fn new(stream: &'a GuestControlStream) -> Result<Self, TransportError> {
        Ok(Self {
            operation: Operation::new(stream, Direction::Receive)?,
            bytes: [0; CONTROL_FRAME_HEADER_BYTES],
            offset: 0,
            descriptors: None,
            header: None,
        })
    }

    pub fn as_raw_fd(&self) -> i32 {
        self.operation.stream.fd
    }

    /// Advance with a bounded I/O budget. Pending retains progress; call again
    /// from a level-triggered poll loop alongside the guardian's other events.
    pub fn advance(&mut self) -> Result<Poll<ControlFrameHeader>, TransportError> {
        match self.read_header() {
            Ok(result) => Ok(result),
            Err(error) => {
                self.descriptors.take();
                Err(self.operation.fail(error))
            }
        }
    }

    fn read_header(&mut self) -> Result<Poll<ControlFrameHeader>, TransportError> {
        self.operation.stream.ensure_usable()?;
        if let Some(header) = self.header {
            return Ok(Poll::Ready(header));
        }
        let mut budget = IoBudget::new();
        while self.offset < self.bytes.len() && budget.available() {
            let end = if self.offset == 0 {
                CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES
            } else {
                self.bytes.len()
            };
            budget.attempted();
            let (read, descriptors) =
                match receive_chunk(self.as_raw_fd(), &mut self.bytes[self.offset..end]) {
                    Err(TransportError::Syscall(EAGAIN)) => return Ok(Poll::Pending),
                    Err(TransportError::Syscall(EINTR)) => continue,
                    result => result?,
                };
            if read == 0 {
                return Err(TransportError::UnexpectedEof);
            }
            self.operation.started = true;
            if self.offset == 0 {
                self.descriptors = Some(descriptors);
            } else if descriptors.count != 0 {
                return Err(TransportError::UnexpectedDescriptors);
            }
            self.offset += read;
            budget.transferred(read);
        }
        if self.offset != self.bytes.len() {
            return Ok(Poll::Pending);
        }
        let header =
            ControlFrameHeader::decode(&self.bytes).map_err(TransportError::InvalidHeader)?;
        self.operation
            .stream
            .session
            .get()
            .next_inbound(header)
            .map_err(TransportError::Session)?;
        let count = self.descriptors.as_ref().map_or(0, |fds| fds.count);
        if count != header.descriptor_count() as usize {
            return Err(TransportError::DescriptorCountMismatch {
                declared: header.descriptor_count(),
                actual: count,
            });
        }
        self.header = Some(header);
        Ok(Poll::Ready(header))
    }

    pub fn into_payload<B: AsMut<[u8]>>(
        mut self,
        mut buffer: B,
    ) -> Result<PayloadReceiver<'a, B>, TransportError> {
        self.operation.stream.ensure_usable()?;
        let header = self.header.ok_or(TransportError::IncompleteFrame)?;
        let required = header.payload_bytes() as usize;
        if buffer.as_mut().len() < required {
            return Err(self.operation.fail(TransportError::PayloadBufferTooSmall {
                required,
                available: buffer.as_mut().len(),
            }));
        }
        let descriptors = self
            .descriptors
            .take()
            .ok_or(TransportError::IncompleteFrame)?
            .finish(header.descriptor_count())?;
        Ok(PayloadReceiver {
            operation: self.operation,
            header,
            buffer,
            descriptors: Some(descriptors),
            offset: 0,
        })
    }
}

#[must_use]
pub struct PayloadReceiver<'a, B> {
    operation: Operation<'a>,
    header: ControlFrameHeader,
    buffer: B,
    descriptors: Option<ReceivedDescriptors>,
    offset: usize,
}

impl<B: AsMut<[u8]>> PayloadReceiver<'_, B> {
    pub fn as_raw_fd(&self) -> i32 {
        self.operation.stream.fd
    }

    pub fn advance(&mut self) -> Result<Poll<()>, TransportError> {
        match self.read_payload() {
            Ok(result) => Ok(result),
            Err(error) => {
                self.descriptors.take();
                Err(self.operation.fail(error))
            }
        }
    }

    fn read_payload(&mut self) -> Result<Poll<()>, TransportError> {
        self.operation.stream.ensure_usable()?;
        if self.operation.finished {
            return Ok(Poll::Ready(()));
        }
        let required = self.header.payload_bytes() as usize;
        if self.buffer.as_mut().len() < required {
            return Err(TransportError::PayloadBufferTooSmall {
                required,
                available: self.buffer.as_mut().len(),
            });
        }
        let mut budget = IoBudget::new();
        while self.offset < required && budget.available() {
            let end = self.offset + budget.length(required - self.offset);
            budget.attempted();
            let (read, descriptors) = match receive_chunk(
                self.as_raw_fd(),
                &mut self.buffer.as_mut()[self.offset..end],
            ) {
                Err(TransportError::Syscall(EAGAIN)) => return Ok(Poll::Pending),
                Err(TransportError::Syscall(EINTR)) => continue,
                result => result?,
            };
            if descriptors.count != 0 {
                return Err(TransportError::UnexpectedDescriptors);
            }
            if read == 0 {
                return Err(TransportError::UnexpectedEof);
            }
            self.offset += read;
            budget.transferred(read);
        }
        if self.offset == required {
            self.operation.finish(self.header)?;
            Ok(Poll::Ready(()))
        } else {
            Ok(Poll::Pending)
        }
    }

    pub fn finish(mut self) -> Result<GuestReceivedFrame<B>, TransportError> {
        if !self.operation.finished {
            return Err(TransportError::IncompleteFrame);
        }
        let descriptors = self
            .descriptors
            .take()
            .ok_or(TransportError::IncompleteFrame)?;
        Ok(GuestReceivedFrame {
            header: self.header,
            buffer: self.buffer,
            descriptors,
        })
    }
}

/// Owns the caller's payload storage and all received FDs. The storage may be a
/// borrowed slice or a dedicated mapping retained for a prepared request.
pub struct GuestReceivedFrame<B> {
    header: ControlFrameHeader,
    buffer: B,
    descriptors: ReceivedDescriptors,
}

impl<B> GuestReceivedFrame<B> {
    pub const fn header(&self) -> ControlFrameHeader {
        self.header
    }
    pub const fn descriptors(&self) -> &ReceivedDescriptors {
        &self.descriptors
    }
    pub fn into_parts(self) -> (ControlFrameHeader, B, ReceivedDescriptors) {
        (self.header, self.buffer, self.descriptors)
    }
}

impl<B: AsRef<[u8]>> GuestReceivedFrame<B> {
    pub fn payload(&self) -> &[u8] {
        &self.buffer.as_ref()[..self.header.payload_bytes() as usize]
    }
}

use super::{
    invalid_data, invalid_header, invalid_inbound_session, invalid_outbound_frame,
    invalid_outbound_session, recvmsg_once, sendmsg_once, shutdown_stream, validate_descriptors,
    ControlFrameHeader, ControlSessionState, ReceivedControlFrame, UnixControlStream,
    CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES, CONTROL_FRAME_HEADER_BYTES,
};
use std::future::pending;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use tokio::io::Interest;
use tokio::net::UnixStream;

const IO_CHUNK_BYTES: usize = 64 * 1024;
const INTERRUPTED_READ_RETRIES: usize = 8;

pub(crate) enum ControlIoEvent {
    Input(Option<ReceivedControlFrame>),
    Sent,
    WriteFailed(io::Error),
}

#[derive(Default)]
pub(super) struct ControlIo {
    input: Input,
    output: Option<Output>,
    write_failed: bool,
}

impl ControlIo {
    pub(super) fn has_output(&self) -> bool {
        self.output.is_some()
    }

    pub(super) fn write_failed(&self) -> bool {
        self.write_failed
    }

    pub(super) fn is_busy(&self) -> bool {
        self.input.offset != 0 || self.has_output()
    }

    pub(super) fn discard(&mut self) {
        *self = Self::default();
    }

    pub(super) async fn receive_frame(
        &mut self,
        stream: &UnixStream,
        session: ControlSessionState,
    ) -> io::Result<ReceivedControlFrame> {
        loop {
            if let Some(frame) = self.input.advance(stream, session).await? {
                return Ok(frame);
            }
        }
    }
}

impl UnixControlStream {
    /// A partially sent request must finish before cleanup can be queued;
    /// replacing it would make the guest read cleanup bytes as its payload.
    pub(crate) fn queue_frame(
        &mut self,
        header: ControlFrameHeader,
        payload: Box<[u8]>,
    ) -> io::Result<()> {
        if !self.can_continue() || self.io.has_output() {
            return Err(invalid_data("control output is not available"));
        }
        header
            .validate_shape(payload.len(), 0)
            .map_err(invalid_outbound_frame)?;
        let session = self
            .session
            .next_outbound(header)
            .map_err(invalid_outbound_session)?;
        self.io.output = Some(Output {
            header: header.encode(),
            payload,
            offset: 0,
            session,
        });
        Ok(())
    }

    pub(crate) fn has_output(&self) -> bool {
        self.io.has_output()
    }

    /// Freezes the unread byte budget before failure cleanup starts. Later
    /// writes must not keep the owner alive by replenishing its receive queue.
    pub(crate) fn queued_input_bytes(&self) -> io::Result<usize> {
        if self.poisoned {
            return Err(invalid_data("control stream cannot be reused"));
        }
        let mut bytes = 0i32;
        // SAFETY: the stream owns this socket and FIONREAD writes one live int.
        if unsafe { libc::ioctl(self.stream.as_raw_fd(), libc::FIONREAD, &mut bytes) } < 0 {
            return Err(io::Error::last_os_error());
        }
        usize::try_from(bytes).map_err(|_| invalid_data("negative control input length"))
    }

    /// Resumes buffered input without waiting for its missing tail. Successful
    /// reads consume the fixed byte budget; interrupted reads have a retry cap.
    pub(crate) fn try_receive_available(
        &mut self,
        budget: &mut usize,
    ) -> io::Result<Option<ReceivedControlFrame>> {
        if self.poisoned {
            return Err(invalid_data("control stream cannot be reused"));
        }
        let mut interrupted = 0;
        while *budget > 0 {
            let (read, frame) = match self
                .io
                .input
                .try_advance(&self.stream, self.session, *budget)
            {
                Ok(progress) => progress,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error)
                    if error.kind() == io::ErrorKind::Interrupted
                        && interrupted < INTERRUPTED_READ_RETRIES =>
                {
                    interrupted += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            *budget -= read;
            if let Some(frame) = frame {
                self.session = self
                    .session
                    .next_inbound(frame.header())
                    .map_err(invalid_inbound_session)?;
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }

    /// Cancelling this future preserves buffered bytes and received FDs. The
    /// driver can handle another event, then resume the same frame.
    pub(crate) async fn advance_io(&mut self) -> io::Result<ControlIoEvent> {
        if self.poisoned {
            return Err(invalid_data("control stream cannot be reused"));
        }
        let event = tokio::select! {
            // Prefer writes so continuous guest output cannot delay Close.
            biased;
            sent = async {
                match &mut self.io.output {
                    Some(output) => output.advance(&self.stream).await,
                    None => pending().await,
                }
            } => Ok(match sent {
                Ok(()) => ControlIoEvent::Sent,
                Err(error) => ControlIoEvent::WriteFailed(error),
            }),
            input = self.io.input.advance(&self.stream, self.session) =>
                input.map(ControlIoEvent::Input),
        };
        match event {
            Ok(ControlIoEvent::WriteFailed(error)) => {
                // A peer can send its final Exit and close before it reads
                // Close. Losing the write side must not discard that report.
                self.io.output = None;
                self.io.write_failed = true;
                Ok(ControlIoEvent::WriteFailed(error))
            }
            Ok(ControlIoEvent::Sent) => {
                let output = self.io.output.take().expect("completed control output");
                self.session = output.session;
                Ok(ControlIoEvent::Sent)
            }
            Ok(ControlIoEvent::Input(Some(frame))) => {
                match self.session.next_inbound(frame.header()) {
                    Ok(session) => {
                        self.session = session;
                        Ok(ControlIoEvent::Input(Some(frame)))
                    }
                    Err(error) => {
                        self.poisoned = true;
                        shutdown_stream(&self.stream);
                        self.io.discard();
                        Err(invalid_inbound_session(error))
                    }
                }
            }
            Ok(event) => Ok(event),
            Err(error) => {
                self.poisoned = true;
                shutdown_stream(&self.stream);
                self.io.discard();
                Err(error)
            }
        }
    }
}

#[derive(Default)]
struct Input {
    encoded: [u8; CONTROL_FRAME_HEADER_BYTES],
    offset: usize,
    header: Option<ControlFrameHeader>,
    payload: Vec<u8>,
    descriptors: Vec<OwnedFd>,
}

impl Input {
    async fn advance(
        &mut self,
        stream: &UnixStream,
        session: ControlSessionState,
    ) -> io::Result<Option<ReceivedControlFrame>> {
        loop {
            stream.readable().await?;
            match stream.try_io(Interest::READABLE, || {
                self.try_advance(stream, session, IO_CHUNK_BYTES)
            }) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result.map(|(_, frame)| frame),
            }
        }
    }

    fn try_advance(
        &mut self,
        stream: &UnixStream,
        session: ControlSessionState,
        budget: usize,
    ) -> io::Result<(usize, Option<ReceivedControlFrame>)> {
        let read;
        if self.header.is_none() {
            // Read the anchor separately so SCM_RIGHTS arriving at a later
            // byte cannot be accepted as this frame's descriptors.
            let end = if self.offset == 0 {
                CONTROL_FRAME_DESCRIPTOR_ANCHOR_BYTES
            } else {
                self.encoded.len()
            };
            let end = end.min(self.offset + budget);
            let (received, descriptors) =
                recvmsg_once(stream, &mut self.encoded[self.offset..end])?;
            read = received;
            if read == 0 {
                return Err(unexpected_eof());
            }
            if self.offset == 0 {
                self.descriptors = descriptors;
            } else if !descriptors.is_empty() {
                return Err(invalid_data("descriptors followed the frame anchor"));
            }
            self.offset += read;
            if self.offset != self.encoded.len() {
                return Ok((read, None));
            }
            let header = ControlFrameHeader::decode(&self.encoded).map_err(invalid_header)?;
            session
                .next_inbound(header)
                .map_err(invalid_inbound_session)?;
            self.descriptors = validate_descriptors(
                std::mem::take(&mut self.descriptors),
                header.descriptor_count(),
            )?;
            self.payload = vec![0; header.payload_bytes() as usize];
            self.header = Some(header);
        } else if self.offset - CONTROL_FRAME_HEADER_BYTES != self.payload.len() {
            let offset = self.offset - CONTROL_FRAME_HEADER_BYTES;
            let end = self.payload.len().min(offset + budget);
            let (received, descriptors) = recvmsg_once(stream, &mut self.payload[offset..end])?;
            read = received;
            if !descriptors.is_empty() {
                return Err(invalid_data("descriptors followed the frame anchor"));
            }
            if read == 0 {
                return Err(unexpected_eof());
            }
            self.offset += read;
        } else {
            read = 0;
        }
        if self.offset - CONTROL_FRAME_HEADER_BYTES != self.payload.len() {
            return Ok((read, None));
        }
        let frame = ReceivedControlFrame {
            header: self.header.take().expect("complete control header"),
            payload: std::mem::take(&mut self.payload),
            descriptors: std::mem::take(&mut self.descriptors),
        };
        self.offset = 0;
        Ok((read, Some(frame)))
    }
}

struct Output {
    header: [u8; CONTROL_FRAME_HEADER_BYTES],
    payload: Box<[u8]>,
    offset: usize,
    session: ControlSessionState,
}

impl Output {
    async fn advance(&mut self, stream: &UnixStream) -> io::Result<()> {
        loop {
            let written = if self.offset == 0 {
                let end = self.payload.len().min(IO_CHUNK_BYTES);
                sendmsg_once(stream, &self.header, &self.payload[..end], &[]).await?
            } else {
                let bytes = if self.offset < self.header.len() {
                    &self.header[self.offset..]
                } else {
                    let offset = self.offset - self.header.len();
                    &self.payload[offset..self.payload.len().min(offset + IO_CHUNK_BYTES)]
                };
                stream.writable().await?;
                match stream.try_write(bytes) {
                    Ok(written) => written,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            };
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "control output stopped",
                ));
            }
            self.offset += written;
            if self.offset == self.header.len() + self.payload.len() {
                return Ok(());
            }
            // A writable socket may never yield on its own. Give the driver
            // a chance to handle cancellation or deadlines between chunks.
            tokio::task::yield_now().await;
        }
    }
}

fn unexpected_eof() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "control stream closed during a frame",
    )
}

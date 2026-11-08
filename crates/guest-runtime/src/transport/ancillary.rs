use core::mem::{size_of, zeroed};
use core::ptr;
use lasper_guest_protocol::MAX_CONTROL_DESCRIPTORS;

use super::descriptors::{RawDescriptors, MAX_RECEIVED_DESCRIPTORS};
use super::TransportError;
use crate::socket::{
    cmsg_align, cmsg_data, cmsg_len, cmsg_space, recvmsg, sendmsg, ControlMessageHeader, Iovec,
    MessageHeader, MSG_CMSG_CLOEXEC, MSG_CTRUNC, MSG_DONTWAIT, MSG_NOSIGNAL, MSG_TRUNC, SCM_PIDFD,
    SCM_RIGHTS, SOL_SOCKET,
};

#[repr(C, align(16))]
struct AncillaryBuffer<const BYTES: usize>([u8; BYTES]);

const SEND_CONTROL_BYTES: usize = cmsg_space(MAX_CONTROL_DESCRIPTORS as usize * size_of::<i32>());
const RECEIVE_CONTROL_BYTES: usize = cmsg_space(MAX_RECEIVED_DESCRIPTORS * size_of::<i32>());

pub(super) fn send_message(
    fd: i32,
    first: &[u8],
    second: &[u8],
    descriptors: &[i32],
) -> Result<usize, TransportError> {
    if descriptors.len() > MAX_CONTROL_DESCRIPTORS as usize {
        return Err(TransportError::DescriptorCountMismatch {
            declared: MAX_CONTROL_DESCRIPTORS,
            actual: descriptors.len(),
        });
    }
    let mut vectors = [
        Iovec {
            base: ptr::null_mut(),
            length: 0,
        },
        Iovec {
            base: ptr::null_mut(),
            length: 0,
        },
    ];
    let mut vector_count = 0;
    for bytes in [first, second] {
        if !bytes.is_empty() {
            vectors[vector_count] = Iovec {
                base: bytes.as_ptr().cast_mut().cast(),
                length: bytes.len(),
            };
            vector_count += 1;
        }
    }
    let mut control = AncillaryBuffer([0; SEND_CONTROL_BYTES]);
    // SAFETY: MessageHeader contains only integers and raw pointers. Buffers
    // borrowed by sendmsg are installed below and remain live for the call.
    let mut message: MessageHeader = unsafe { zeroed() };
    message.vectors = vectors.as_mut_ptr();
    message.vector_count = vector_count;

    if !descriptors.is_empty() {
        let data_bytes = core::mem::size_of_val(descriptors);
        let control_bytes = cmsg_space(data_bytes);
        message.control = control.0.as_mut_ptr().cast();
        message.control_length = control_bytes;
        let header = message.control.cast::<ControlMessageHeader>();
        // SAFETY: The aligned buffer holds the checked header and FD array.
        unsafe {
            (*header).length = cmsg_len(data_bytes);
            (*header).level = SOL_SOCKET;
            (*header).kind = SCM_RIGHTS;
            ptr::copy_nonoverlapping(
                descriptors.as_ptr().cast::<u8>(),
                cmsg_data(header),
                data_bytes,
            );
        }
    }

    syscall_result(sendmsg(fd, &message, MSG_NOSIGNAL | MSG_DONTWAIT))
}

pub(super) fn receive_chunk(
    fd: i32,
    bytes: &mut [u8],
) -> Result<(usize, RawDescriptors), TransportError> {
    let mut vector = Iovec {
        base: bytes.as_mut_ptr().cast(),
        length: bytes.len(),
    };
    let mut control = AncillaryBuffer([0; RECEIVE_CONTROL_BYTES]);
    // SAFETY: MessageHeader contains only integers and raw pointers. Both
    // writable receive buffers below remain live until recvmsg returns.
    let mut message: MessageHeader = unsafe { zeroed() };
    message.vectors = &mut vector;
    message.vector_count = 1;
    message.control = control.0.as_mut_ptr().cast();
    message.control_length = control.0.len();

    let received = syscall_result(recvmsg(fd, &mut message, MSG_CMSG_CLOEXEC | MSG_DONTWAIT))?;

    let descriptors = decode_ancillary(&message, &control.0)?;
    Ok((received, descriptors))
}

fn decode_ancillary(
    message: &MessageHeader,
    control: &[u8],
) -> Result<RawDescriptors, TransportError> {
    let descriptors = parse_descriptors(message, control)?;
    if message.control_length > control.len() || message.flags & (MSG_TRUNC | MSG_CTRUNC) != 0 {
        return Err(TransportError::Truncated);
    }
    Ok(descriptors)
}

fn parse_descriptors(
    message: &MessageHeader,
    control: &[u8],
) -> Result<RawDescriptors, TransportError> {
    let mut descriptors = RawDescriptors::new();
    let control_bytes = message.control_length.min(control.len());
    let header_bytes = cmsg_align(size_of::<ControlMessageHeader>());
    let mut offset = 0;
    let mut rights_seen = false;
    let mut unexpected_control = false;

    while control_bytes.saturating_sub(offset) >= header_bytes {
        // SAFETY: The complete header fits in control; the byte slice need not be aligned.
        let header = unsafe {
            control
                .as_ptr()
                .add(offset)
                .cast::<ControlMessageHeader>()
                .read_unaligned()
        };
        let data_bytes = header
            .length
            .checked_sub(header_bytes)
            .ok_or(TransportError::Truncated)?;
        let available_bytes = control_bytes - offset - header_bytes;
        let truncated = data_bytes > available_bytes;

        if header.level == SOL_SOCKET && matches!(header.kind, SCM_RIGHTS | SCM_PIDFD) {
            if header.kind == SCM_PIDFD || rights_seen {
                unexpected_control = true;
            }
            rights_seen = true;
            if data_bytes == 0 || !data_bytes.is_multiple_of(size_of::<i32>()) {
                unexpected_control = true;
            }
            // Own every complete FD slot that was delivered, even when the
            // record advertises more bytes than the receive buffer contains.
            let delivered_bytes = data_bytes.min(available_bytes);
            for index in 0..delivered_bytes / size_of::<i32>() {
                // SAFETY: This complete slot is bounded by the record and the
                // returned control length. Payload alignment is not assumed.
                let fd = unsafe {
                    control
                        .as_ptr()
                        .add(offset + header_bytes + index * size_of::<i32>())
                        .cast::<i32>()
                        .read_unaligned()
                };
                if fd >= 0 {
                    descriptors.push(fd);
                } else {
                    unexpected_control = true;
                }
            }
        } else {
            unexpected_control = true;
        }

        if truncated {
            return Err(TransportError::Truncated);
        }
        offset += cmsg_align(header.length);
    }

    if unexpected_control {
        Err(TransportError::UnexpectedDescriptors)
    } else {
        Ok(descriptors)
    }
}

fn syscall_result(result: isize) -> Result<usize, TransportError> {
    if result < 0 {
        Err(TransportError::Syscall((-result) as i32))
    } else {
        Ok(result as usize)
    }
}

#[cfg(test)]
#[path = "../../tests/transport/ancillary.rs"]
mod tests;

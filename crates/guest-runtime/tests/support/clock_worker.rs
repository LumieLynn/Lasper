use super::*;

// These control-loop tests isolate report delivery from account loading. The
// child still has a real session, status pipe, output pipes and pidfd.
pub(in crate::session) fn prepared(worker: WorkerBootstrap<'_>) -> ! {
    if StatusMessage::Prepared.send(&worker.status).is_err() {
        linux::exit(126);
    }
    hold()
}

pub(in crate::session) fn with_output(worker: WorkerBootstrap<'_>) -> ! {
    if StatusMessage::Prepared.send(&worker.status).is_err() {
        linux::exit(126);
    }
    let bytes = [b'x'; 8192];
    for _ in 0..lasper_guest_protocol::MAX_CAPTURED_OUTPUT_BYTES / bytes.len() {
        let mut remaining = &bytes[..];
        while !remaining.is_empty() {
            match linux::write(1, remaining) {
                Ok(0) | Err(_) => linux::exit(126),
                Ok(count) => remaining = &remaining[count..],
            }
        }
    }
    hold()
}

fn hold() -> ! {
    loop {
        let _ = linux::poll(&mut [], Some(10));
    }
}

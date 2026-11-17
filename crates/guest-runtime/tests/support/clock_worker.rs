use super::*;

// These control-loop tests isolate report delivery from account loading. The
// child still has a real session, status pipe, output pipes and pidfd.
pub(in crate::session) fn prepared(worker: WorkerBootstrap<'_>) -> ! {
    if StatusMessage::Prepared.send(&worker.status).is_err() {
        linux::exit(126);
    }
    hold()
}

fn hold() -> ! {
    loop {
        let _ = linux::poll(&mut [], Some(10));
    }
}

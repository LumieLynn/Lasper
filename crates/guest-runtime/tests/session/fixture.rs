use lasper_guest_protocol::{PrepareReadyFrame, ProjectionObservations};
use lasper_guest_runtime::session::{SessionGuardian, WorkerBootstrap, WorkerFailure, WorkerStage};
use std::os::fd::OwnedFd;
#[path = "../support/accounts.rs"]
mod accounts;
pub(super) use accounts::{get, initialize};
pub(super) struct ReceivedReady {
    pub frame: PrepareReadyFrame,
    pub descriptors: Vec<OwnedFd>,
}

pub(super) fn receive_ready(
    worker: &mut SessionGuardian<'_, '_>,
    connection: &lasper_guest_runtime::transport::GuestControlStream,
    now_ms: u64,
) -> ReceivedReady {
    let frame = loop {
        match worker
            .deliver_ready(
                now_ms,
                &mut lasper_guest_runtime::transport::IoBudget::new(),
            )
            .unwrap()
        {
            std::task::Poll::Ready(frame) => break frame,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    };
    // The test's host receives the real Ready and returns its FD copies so the
    // account/PTY assertions can run inside this fixture's namespace.
    let echoed = super::control_wire::receive(connection.as_raw_fd()).unwrap();
    assert_eq!(echoed.header.request_id(), frame.request_id());
    assert_eq!(echoed.header.descriptor_count(), frame.descriptor_count());
    ReceivedReady {
        frame,
        descriptors: echoed.descriptors,
    }
}

pub(super) fn entry(bootstrap: WorkerBootstrap<'_>) -> ! {
    if get().enter_root().is_err() {
        bootstrap.fail(WorkerFailure::new(WorkerStage::Account, 13));
    }
    bootstrap.run(|_| Ok(ProjectionObservations::default()))
}

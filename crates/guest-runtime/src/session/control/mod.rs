//! Receives and validates Prepare before assigning a worker, then drives the
//! connection until cleanup and final reporting finish. A broken connection
//! ends reporting but leaves the guardian responsible for child cleanup.

mod dispatch;
mod driver;
mod input;
mod prepare;
mod report;

use super::error::SystemResultExt;
use super::{GuardianContext, GuardianError, WorkerBootstrap};
use crate::linux;

pub(super) fn run(context: &GuardianContext) -> Result<(), GuardianError> {
    run_with_worker(context, worker_entry)
}

fn worker_entry(worker: WorkerBootstrap<'_>) -> ! {
    worker.run()
}

fn run_with_worker(
    context: &GuardianContext,
    entry: fn(WorkerBootstrap<'_>) -> !,
) -> Result<(), GuardianError> {
    let mut wire = lasper_guest_protocol::WireSessionState::after_hello(context.hello_request_id());
    let storage = prepare::receive(context, &mut wire)?;
    let request_id = storage.header().request_id();
    let request = storage
        .request_with(context.negotiated_protocol())
        .map_err(|error| {
            prepare::reject(context, request_id, GuardianError::PrepareStorage(error))
        })?;
    let accepted_at = linux::monotonic_ms()
        .context("read accepted Prepare clock")
        .map_err(|error| prepare::reject(context, request_id, error))?;
    let (preparation, mut guardian) = prepare::assign(context, &request, accepted_at, entry)
        .map_err(|error| prepare::reject(context, request_id, error))?;
    driver::run(context, &request, preparation, &mut guardian, wire)
}

#[cfg(test)]
#[path = "../../../tests/support/forked_guardian.rs"]
mod test_fixture;

#[cfg(test)]
#[path = "../../../tests/control/preparation.rs"]
mod tests;

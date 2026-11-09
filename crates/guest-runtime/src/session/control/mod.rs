mod capture;
mod dispatch;
mod driver;
mod input;
mod prepare;
mod report;

use super::error::SystemResultExt;
use super::linux;
use super::{GuardianContext, GuardianError, WorkerBootstrap};

pub(super) fn run(context: &GuardianContext) -> Result<(), GuardianError> {
    run_with_worker(context, super::projection::entry)
}

fn run_with_worker(
    context: &GuardianContext,
    entry: fn(WorkerBootstrap<'_>) -> !,
) -> Result<(), GuardianError> {
    let storage = prepare::receive(context)?;
    let request_id = storage.header().request_id();
    let request = storage.request().map_err(|error| {
        prepare::reject(context, request_id, GuardianError::PrepareStorage(error))
    })?;
    let accepted_at = linux::monotonic_ms()
        .context("read accepted Prepare clock")
        .map_err(|error| prepare::reject(context, request_id, error))?;
    let (preparation, mut guardian, capture) =
        prepare::assign(context, &request, accepted_at, entry)
            .map_err(|error| prepare::reject(context, request_id, error))?;
    driver::run(context, &request, preparation, &mut guardian, capture)
}

#[cfg(test)]
#[path = "../../../tests/support/forked_guardian.rs"]
mod test_fixture;

#[cfg(test)]
#[path = "../../../tests/control/preparation.rs"]
mod tests;

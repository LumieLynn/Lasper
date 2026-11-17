use anyhow::{Context, Result};
use std::future::Future;
use tokio::time::{timeout, Duration};

// A held external writer must not keep this diagnostic client alive forever
// after control observation has ended. This is an output drain budget only.
const DRAIN_BUDGET: Duration = Duration::from_secs(1);

pub async fn observe<T>(
    wait: impl Future<Output = T>,
    close: impl Future<Output = T>,
    stop: impl Future<Output = ()>,
    drain: impl Future<Output = Result<()>>,
    mut closing: bool,
) -> Result<T> {
    tokio::pin!(wait, close, stop, drain);
    let mut drained = false;
    let mut output_error = None;
    let completion = loop {
        tokio::select! {
            result = &mut drain, if !drained => {
                drained = true;
                if let Err(error) = result {
                    output_error = Some(error);
                    closing = true;
                }
            }
            completion = &mut wait, if !closing => break completion,
            completion = &mut close, if closing => break completion,
            _ = &mut stop, if !closing => closing = true,
        }
    };
    if let Some(error) = output_error {
        return Err(error);
    }
    if !drained {
        timeout(DRAIN_BUDGET, &mut drain)
            .await
            .context("data streams did not reach EOF within the post-completion drain budget")??;
    }
    Ok(completion)
}

#[cfg(test)]
#[path = "../tests/session.rs"]
mod tests;

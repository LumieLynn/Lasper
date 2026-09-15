use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;

use crate::application::configuration::{ConfigurationService, ConfigurationTarget};
use crate::application::inspection::ResourceInspectionError;
use crate::tui::events::AppEvent;

pub(crate) fn inspect(
    service: Arc<ConfigurationService>,
    target: ConfigurationTarget,
    query: u64,
    events: tokio::sync::mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            Duration::from_secs(10),
            AssertUnwindSafe(service.inspect(&target)).catch_unwind(),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ResourceInspectionError::backend(
                "Configuration inspection stopped unexpectedly",
            )),
            Err(_) => Err(ResourceInspectionError::backend(
                "Configuration inspection timed out",
            )),
        };
        let _ = events
            .send(AppEvent::ConfigurationInspected {
                query,
                target,
                result,
            })
            .await;
    })
}

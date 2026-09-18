use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;

use crate::application::configuration::{
    ConfigurationEdit, ConfigurationService, ConfigurationTarget,
};
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

pub(crate) fn preview(
    service: Arc<ConfigurationService>,
    edit: ConfigurationEdit,
    generation: u64,
    events: tokio::sync::mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    let target = edit.target.clone();
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            Duration::from_secs(10),
            AssertUnwindSafe(service.preview(&edit)).catch_unwind(),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ResourceInspectionError::backend(
                "Configuration preview stopped unexpectedly",
            )),
            Err(_) => Err(ResourceInspectionError::backend(
                "Configuration preview timed out",
            )),
        };
        let _ = events
            .send(AppEvent::ConfigurationPreviewed {
                generation,
                target,
                result,
            })
            .await;
    })
}

pub(crate) fn apply(
    service: Arc<ConfigurationService>,
    edit: ConfigurationEdit,
    generation: u64,
    events: tokio::sync::mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    let target = edit.target.clone();
    tokio::spawn(async move {
        // Do not time out a mutation locally: the adapter must return a
        // definite semantic outcome, especially after RPC dispatch.
        let result = match AssertUnwindSafe(service.apply(&edit)).catch_unwind().await {
            Ok(result) => result,
            Err(_) => Err(ResourceInspectionError::backend(
                "Configuration save stopped unexpectedly; refresh before retrying",
            )),
        };
        let _ = events
            .send(AppEvent::ConfigurationApplied {
                generation,
                target,
                result,
            })
            .await;
    })
}

pub(crate) fn check_x11(
    service: Arc<crate::application::x11::X11AccessService>,
    configuration_target: ConfigurationTarget,
    target: crate::application::sessions::ShellTarget,
    host_socket: crate::domain::x11::HostX11Socket,
    generation: u64,
    events: tokio::sync::mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            Duration::from_secs(15),
            AssertUnwindSafe(service.check(target, host_socket)).catch_unwind(),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(crate::application::x11::X11AccessError::Desktop(
                crate::application::x11::X11DesktopAccessError::new(
                    "X11 access check stopped unexpectedly",
                ),
            )),
            Err(_) => Err(crate::application::x11::X11AccessError::Desktop(
                crate::application::x11::X11DesktopAccessError::new("X11 access check timed out"),
            )),
        };
        let _ = events
            .send(AppEvent::ConfigurationX11Checked {
                generation,
                target: configuration_target,
                result,
            })
            .await;
    })
}

pub(crate) fn authorize_x11(
    service: Arc<crate::application::x11::X11AccessService>,
    configuration_target: ConfigurationTarget,
    target: crate::application::sessions::ShellTarget,
    host_socket: crate::domain::x11::HostX11Socket,
    generation: u64,
    events: tokio::sync::mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            Duration::from_secs(15),
            AssertUnwindSafe(service.authorize(target, host_socket)).catch_unwind(),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(crate::application::x11::X11AccessError::Desktop(
                crate::application::x11::X11DesktopAccessError::new(
                    "X11 authorization stopped unexpectedly; inspect the current ACL before retrying",
                ),
            )),
            Err(_) => Err(crate::application::x11::X11AccessError::Desktop(
                crate::application::x11::X11DesktopAccessError::new(
                    "X11 authorization timed out; inspect the current ACL before retrying",
                ),
            )),
        };
        let _ = events
            .send(AppEvent::ConfigurationX11Authorized {
                generation,
                target: configuration_target,
                result,
            })
            .await;
    })
}

pub(crate) fn revoke_x11(
    service: Arc<crate::application::x11::X11AccessService>,
    configuration_target: ConfigurationTarget,
    target: crate::application::sessions::ShellTarget,
    host_socket: crate::domain::x11::HostX11Socket,
    record_id: String,
    generation: u64,
    events: tokio::sync::mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            Duration::from_secs(15),
            AssertUnwindSafe(service.revoke(target, host_socket, record_id)).catch_unwind(),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(crate::application::x11::X11AccessError::Desktop(
                crate::application::x11::X11DesktopAccessError::new(
                    "X11 revocation stopped unexpectedly; inspect the current ACL before retrying",
                ),
            )),
            Err(_) => Err(crate::application::x11::X11AccessError::Desktop(
                crate::application::x11::X11DesktopAccessError::new(
                    "X11 revocation timed out; inspect the current ACL before retrying",
                ),
            )),
        };
        let _ = events
            .send(AppEvent::ConfigurationX11Revoked {
                generation,
                target: configuration_target,
                result,
            })
            .await;
    })
}

//! Read the guest manager through a short-lived, non-activating bus connection.

mod socket;

use crate::adapters::process::guest_bus::{self, GuestBusError, GuestBusTarget};
use crate::application::sessions::execution::NativeReadiness;
use std::io;
use tokio::time::Instant;
use zbus::proxy::{CacheProperties, MethodFlags};

pub(crate) async fn observe(target: &GuestBusTarget, deadline: Instant) -> NativeReadiness {
    let stream = match guest_bus::connect(target, deadline).await {
        Ok(stream) => stream,
        Err(error) => return connection_error(error),
    };
    match tokio::time::timeout_at(deadline, query(stream, 0)).await {
        Ok(Ok(version)) => NativeReadiness::Ready { version },
        Ok(Err(error)) => query_error(error),
        Err(_) => NativeReadiness::Unresponsive {
            reason: "guest bus authentication or manager query exceeded its deadline".into(),
        },
    }
}

// The connector authenticates as root inside the guest user namespace. Keep
// the EXTERNAL identity explicit; the host-side client's uid can be different.
async fn query(stream: tokio::net::UnixStream, guest_uid: u32) -> zbus::Result<String> {
    let connection = zbus::connection::Builder::socket(socket::GuestBusSocket(stream))
        .user_id(guest_uid)
        .max_queued(8)
        .build()
        .await?;
    let bus = zbus::Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    let owner: zbus::names::OwnedUniqueName = bus
        .call_with_flags(
            "GetNameOwner",
            MethodFlags::NoAutoStart.into(),
            &("org.freedesktop.systemd1",),
        )
        .await?
        .ok_or_else(|| zbus::Error::Failure("guest bus returned no owner reply".into()))?;
    // Pin the call to the observed owner. Never activate or silently switch to
    // a replacement manager, and do not start a property-cache subscription.
    let manager = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(&connection)
        .destination(owner)?
        .path("/org/freedesktop/systemd1")?
        .interface("org.freedesktop.DBus.Properties")?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let version: zbus::zvariant::OwnedValue = manager
        .call_with_flags(
            "Get",
            MethodFlags::NoAutoStart.into(),
            &("org.freedesktop.systemd1.Manager", "Version"),
        )
        .await?
        .ok_or_else(|| zbus::Error::Failure("guest manager returned no version reply".into()))?;
    String::try_from(version).map_err(Into::into)
}

fn connection_error(error: GuestBusError) -> NativeReadiness {
    let error = match error {
        GuestBusError::Socket(error) => error,
        GuestBusError::Preparation(error) => return NativeReadiness::unknown(error),
        GuestBusError::Deadline => {
            return NativeReadiness::Unresponsive {
                reason: error.to_string(),
            }
        }
    };
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => NativeReadiness::NoBus {
            reason: error.to_string(),
        },
        io::ErrorKind::PermissionDenied => NativeReadiness::Denied {
            reason: error.to_string(),
        },
        io::ErrorKind::TimedOut => NativeReadiness::Unresponsive {
            reason: error.to_string(),
        },
        _ => NativeReadiness::unknown(error),
    }
}

fn query_error(error: zbus::Error) -> NativeReadiness {
    match &error {
        zbus::Error::MethodError(name, _, _) => match name.as_str() {
            "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.ServiceUnknown" => NativeReadiness::NoManager,
            "org.freedesktop.DBus.Error.AccessDenied" | "org.freedesktop.DBus.Error.AuthFailed" => {
                NativeReadiness::Denied {
                    reason: error.to_string(),
                }
            }
            _ => NativeReadiness::unknown(error),
        },
        // A textual handshake failure is not sufficient evidence of a policy
        // denial, or of the init used by this guest. Preserve it as unknown.
        _ => NativeReadiness::unknown(error),
    }
}

#[cfg(test)]
mod tests;

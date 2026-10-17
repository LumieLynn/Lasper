//! Session status pipes and machine-removal notifications.

use crate::domain::session::SessionLifecycle;
use crate::ipc::protocol::session::WireSessionLifecycle;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use tokio::io::AsyncWriteExt;
use tokio::net::unix::pipe::Sender;

pub(super) fn channel() -> std::io::Result<(OwnedFd, Sender)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // pipe2 succeeded, so each new descriptor has exactly one owner.
    let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((reader, Sender::from_owned_fd(writer)?))
}

pub(super) async fn write(mut writer: Sender, lifecycle: WireSessionLifecycle) {
    let mut message = serde_json::to_vec(&lifecycle).expect("session status is serializable");
    message.push(b'\n');
    if let Err(error) = writer.write_all(&message).await {
        if error.kind() != std::io::ErrorKind::BrokenPipe {
            log::warn!("Daemon: failed to deliver session status: {error}");
        }
    }
}

pub(super) fn monitor_machine(
    removed: tokio::sync::oneshot::Receiver<SessionLifecycle>,
    writer: Sender,
) {
    tokio::spawn(async move {
        let result = tokio::select! {
            result = removed => result,
            _ = reader_closed(&writer) => return,
        };
        let lifecycle = match result {
            Ok(SessionLifecycle::Exited { success, code }) => {
                WireSessionLifecycle::Exited { success, code }
            }
            _ => WireSessionLifecycle::Failed {
                message: "machine removal monitor closed".into(),
            },
        };
        write(writer, lifecycle).await;
    });
}

async fn reader_closed(writer: &Sender) {
    // A machine PTY has no daemon-owned child to reap. Release the D-Bus
    // subscription when the client closes its status reader instead.
    loop {
        let mut fd = libc::pollfd {
            fd: writer.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        if unsafe { libc::poll(&mut fd, 1, 0) } < 0
            || fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn machine_monitor_releases_its_subscription_when_the_client_closes() {
        let (reader, writer) = channel().unwrap();
        let (mut removed, receiver) = tokio::sync::oneshot::channel();
        monitor_machine(receiver, writer);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), removed.closed())
                .await
                .is_err()
        );
        drop(reader);
        tokio::time::timeout(Duration::from_secs(1), removed.closed())
            .await
            .unwrap();
    }
}

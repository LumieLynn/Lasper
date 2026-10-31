use super::*;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Manager;

#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl Manager {
    #[zbus(property)]
    fn version(&self) -> &str {
        "261.test"
    }
}

#[tokio::test]
#[ignore = "requires dbus-daemon; uses an isolated session bus"]
async fn credential_nul_handoff_authenticates_and_queries_the_current_manager() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bus");
    let address = format!("unix:path={}", path.display());
    let mut bus = tokio::process::Command::new("dbus-daemon")
        .args([
            "--session",
            "--nofork",
            "--print-address=1",
            "--address",
            &address,
        ])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut announced = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        BufReader::new(bus.stdout.take().unwrap()).read_line(&mut announced),
    )
    .await
    .unwrap()
    .unwrap();
    let manager = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.freedesktop.systemd1")
        .unwrap()
        .serve_at("/org/freedesktop/systemd1", Manager)
        .unwrap()
        .build()
        .await
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        stream.write_all(&[0]).await.unwrap();
        query(stream, uzers::get_effective_uid()).await
    })
    .await;
    manager
        .release_name("org.freedesktop.systemd1")
        .await
        .unwrap();
    let absent = tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        stream.write_all(&[0]).await.unwrap();
        query(stream, uzers::get_effective_uid()).await
    })
    .await;
    bus.kill().await.unwrap();
    bus.wait().await.unwrap();

    assert_eq!(result.unwrap().unwrap(), "261.test");
    assert_eq!(
        query_error(absent.unwrap().unwrap_err()),
        NativeReadiness::NoManager
    );
}

#[test]
fn host_namespace_denial_is_distinct_from_guest_socket_denial() {
    let denied = || io::Error::from_raw_os_error(libc::EACCES);
    assert!(matches!(
        connection_error(GuestBusError::Preparation(denied())),
        NativeReadiness::Unknown { .. }
    ));
    assert!(matches!(
        connection_error(GuestBusError::Socket(denied())),
        NativeReadiness::Denied { .. }
    ));
    assert!(matches!(
        connection_error(GuestBusError::Socket(io::Error::from_raw_os_error(
            libc::ENOENT
        ))),
        NativeReadiness::NoBus { .. }
    ));
}

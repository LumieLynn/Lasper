mod fixture;
mod session;
#[path = "../support/target.rs"]
mod target;

use lasper_guest_host_transport::{BrokerConnectError, BrokerConnection};
use lasper_guest_protocol::ControlLimits;
use std::future::Future;
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::time::{Duration, Instant};

fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("--broker-child") {
        fixture::enter_user_namespace();
        if arguments.iter().any(|argument| argument == "--accounts") {
            fixture::install_accounts(std::path::Path::new(&arguments[2]));
        }
        std::io::stdout().write_all(&[1]).unwrap();
        std::io::stdout().flush().unwrap();
        let mut go = [0];
        std::io::stdin().read_exact(&mut go).unwrap();
        assert_eq!(go, [1]);
        if arguments.iter().any(|argument| argument == "--fail-start") {
            use std::os::unix::fs::PermissionsExt;
            // Exercise the real listener's rejection before it creates a socket.
            std::fs::set_permissions(&arguments[2], std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let directory = std::ffi::CString::new(arguments[2].as_bytes()).unwrap();
        // SAFETY: this single-threaded child has no children or retained FD
        // owners. It exits immediately after the broker releases its sessions.
        let result = unsafe { lasper_guest_runtime::broker::run(&directory) };
        if let Err(error) = result {
            eprintln!("host connector broker fixture: {error:?}");
        }
        std::process::exit(if result.is_ok() { 0 } else { 70 });
    }
    if arguments.get(1).map(String::as_str) != Some("--ignored") {
        println!(
            "host broker: run --test broker -- --ignored (requires unprivileged user namespaces)"
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(scenarios());
}

async fn scenarios() {
    let mut broker = fixture::Broker::spawn();
    let machine = target::target_for(broker.pid()).await;
    let path = broker.socket();

    println!("host broker: connect begins before bind and waits for broker startup");
    assert!(!path.exists());
    let first = BrokerConnection::connect(
        Arc::clone(&machine),
        path.to_str().unwrap(),
        ControlLimits::CURRENT,
        Instant::now() + Duration::from_secs(1),
    );
    tokio::pin!(first);
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(first.as_mut().poll(context)))
            .await
            .is_pending()
    );
    broker.start();
    let first = first.await.unwrap();

    println!("host broker: another user namespace is rejected before Hello");
    let result = BrokerConnection::connect(
        target::current_target().await,
        path.to_str().unwrap(),
        ControlLimits::CURRENT,
        Instant::now() + Duration::from_secs(1),
    )
    .await;
    assert!(matches!(result, Err(BrokerConnectError::InvalidPeer(_))));

    println!("host broker: concurrent guardians share the verified listener generation");
    let second = BrokerConnection::connect(
        Arc::clone(&machine),
        path.to_str().unwrap(),
        ControlLimits::CURRENT,
        Instant::now() + Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(
        first.readiness().broker_id(),
        second.readiness().broker_id()
    );
    first.recheck().unwrap();
    second.recheck().unwrap();
    drop(first);
    second.recheck().unwrap();

    println!("host broker: stopping the broker invalidates the retained peer and machine");
    broker.stop().await;
    assert!(second.recheck().is_err());
    drop(second);
    println!("host broker: a failed launch invalidates the waiting machine target");
    let mut failed = fixture::Broker::failed_start();
    let machine = target::target_for(failed.pid()).await;
    let path = failed.socket();
    let attempt = BrokerConnection::connect(
        machine,
        path.to_str().unwrap(),
        ControlLimits::CURRENT,
        Instant::now() + Duration::from_secs(1),
    );
    tokio::pin!(attempt);
    assert!(
        std::future::poll_fn(|context| std::task::Poll::Ready(attempt.as_mut().poll(context)))
            .await
            .is_pending()
    );
    failed.start();
    failed.expect_failed_start().await;
    assert!(matches!(attempt.await, Err(BrokerConnectError::Target(_))));
    println!("host broker: 5 scenarios passed");
    session::run().await;
}

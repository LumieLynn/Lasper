use super::client::Client;
use lasper_guest_protocol::*;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

fn assert_signal_exit(client: &mut Client, signal: i32) {
    let frame = client.finish();
    let exit = super::client::protocol()
        .decode_exit(
            &client.request,
            client.ready.response().preparation_id(),
            frame.header,
            &frame.payload,
            frame.descriptors.len(),
        )
        .unwrap();
    assert_eq!(exit.report().execution(), ExecutionStatus::Started);
    assert_eq!(
        exit.report().exit(),
        Some(ExitStatus::Signaled(signal as u8))
    );
    assert_eq!(exit.report().reason(), ExitReason::WorkerExited);
    assert_eq!(exit.report().cleanup(), CleanupStatus::Complete);
    assert_eq!(exit.report().failure(), None);
    client.wait();
}

fn terminal_drop(privileged: bool) {
    let mut client = Client::prepared(
        "terminal-master-drop",
        privileged,
        ExecutionKind::Terminal,
        &[
            "/bin/sh",
            "-c",
            "trap - HUP; printf ready; exec /bin/sleep 30",
        ],
    );
    let mut terminal = std::fs::File::from(client.descriptors.remove(0));
    client.start();
    let mut ready = [0; 5];
    let mut offset = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while offset < ready.len() {
        let mut event = libc::pollfd {
            fd: terminal.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "worker did not write its ready marker"
        );
        // SAFETY: poll borrows one initialized event for the owned PTY.
        let result = unsafe { libc::poll(&mut event, 1, remaining.as_millis() as i32) };
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        assert_eq!(result, 1, "worker did not write its ready marker");
        let count = terminal.read(&mut ready[offset..]).unwrap();
        assert_ne!(count, 0, "worker exited before its ready marker");
        offset += count;
    }
    assert_eq!(&ready, b"ready");
    drop(terminal);
    // Keep the control connection open; no Cancel or Close caused this exit.
    assert_signal_exit(&mut client, libc::SIGHUP);
}

fn pipe_reader_drop(privileged: bool) {
    let mut client = Client::prepared(
        "pipe-reader-drop",
        privileged,
        ExecutionKind::Piped,
        &["/bin/output-helper", "--pipe-drop-writer"],
    );
    let socket = std::env::temp_dir().join(format!(
        "lasper-account-test-{}/home/alice/output.sock",
        client.guardian.child.id()
    ));
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
    listener.set_nonblocking(true).unwrap();
    client.start();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "worker did not reach its write gate"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("accept pipe writer: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    // Ready handed the only stdout reader to this client. Keep control and
    // stderr open so the exit can only follow closing stdout.
    drop(client.descriptors.remove(2));
    stream.write_all(&[1]).unwrap();
    assert_signal_exit(&mut client, libc::SIGPIPE);
}

pub fn pipe_writer() {
    let mut stream = UnixStream::connect("/home/alice/output.sock").unwrap();
    let mut go = [0];
    stream.read_exact(&mut go).unwrap();
    assert_eq!(go, [1]);
    // Rust startup ignores SIGPIPE. Restore its default and unblock it only in
    // this dedicated helper, after the client has closed the Ready stdout FD.
    unsafe {
        assert_ne!(libc::signal(libc::SIGPIPE, libc::SIG_DFL), libc::SIG_ERR);
        let mut mask = std::mem::zeroed();
        assert_eq!(libc::sigemptyset(&mut mask), 0);
        assert_eq!(libc::sigaddset(&mut mask, libc::SIGPIPE), 0);
        assert_eq!(
            libc::sigprocmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut()),
            0
        );
        libc::write(1, b"x".as_ptr().cast(), 1);
    }
    panic!("write to a pipe without readers did not terminate the worker");
}

pub const SCENARIOS: &[super::Scenario] = &[
    (
        "dropping the last PTY master reports SIGHUP without Close",
        terminal_drop,
    ),
    (
        "dropping the last pipe reader reports SIGPIPE without Close",
        pipe_reader_drop,
    ),
];

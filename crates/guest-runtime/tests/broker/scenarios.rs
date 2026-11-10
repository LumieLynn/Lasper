use super::{
    client,
    fixture::{self, Broker},
};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;

fn concurrent(privileged: bool) {
    let mut broker = Broker::spawn(privileged);
    let mut first = broker.connect();
    let mut second = broker.connect();
    let id = client::hello(&mut first);
    assert_eq!(id, client::hello(&mut second));
    broker.wait_children(2);
    drop(first);
    broker.wait_children(1);
    // A new admission still uses this broker generation while the other
    // guardian owns its original connection.
    let mut third = broker.connect();
    assert_eq!(client::hello(&mut third), id);
    drop(third);
    drop(second);
    broker.wait_children(0);
    assert!(broker.stop().success());
}

fn stop_live_connections(privileged: bool) {
    let mut broker = Broker::spawn(privileged);
    let mut first = broker.connect();
    let mut second = broker.connect();
    client::hello(&mut first);
    client::hello(&mut second);
    broker.wait_children(2);
    assert!(broker.stop().success());
    for stream in [&mut first, &mut second] {
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    }
}

fn killed_guardian(privileged: bool) {
    let mut broker = Broker::spawn(privileged);
    let mut first = broker.connect();
    client::hello(&mut first);
    broker.wait_children(1);
    let victim = broker.children()[0];
    let mut second = broker.connect();
    let id = client::hello(&mut second);
    assert_eq!(unsafe { libc::kill(victim, libc::SIGKILL) }, 0);
    assert_eq!(first.read(&mut [0]).unwrap(), 0);
    broker.wait_children(1);
    let mut third = broker.connect();
    assert_eq!(client::hello(&mut third), id);
    drop(third);
    drop(second);
    assert!(broker.stop().success());
}

fn hello_timeout(privileged: bool) {
    let mut broker = Broker::spawn(privileged);
    let mut partial = broker.connect();
    partial.write_all(b"L").unwrap();
    assert!(matches!(partial.read(&mut [0]), Ok(0)));
    broker.wait_children(0);
    let mut healthy = broker.connect();
    client::hello(&mut healthy);
    drop(healthy);
    assert!(broker.stop().success());
}

fn competing_broker(privileged: bool) {
    let mut broker = Broker::spawn(privileged);
    let inode = std::os::unix::fs::MetadataExt::ino(
        &fs::symlink_metadata(broker.directory.join("guest-exec.sock")).unwrap(),
    );
    let status = fixture::command(&broker.directory, privileged)
        .status()
        .unwrap();
    assert!(!status.success());
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(
            &fs::symlink_metadata(broker.directory.join("guest-exec.sock")).unwrap()
        ),
        inode
    );
    let mut healthy = broker.connect();
    client::hello(&mut healthy);
    drop(healthy);
    assert!(broker.stop().success());
}

fn permissions(privileged: bool) {
    let mut broker = Broker::spawn(privileged);
    assert_private_files(&broker);
    assert!(broker.stop().success());
    fs::set_permissions(&broker.directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!fixture::command(&broker.directory, privileged)
        .status()
        .unwrap()
        .success());
}

fn assert_private_files(broker: &Broker) {
    assert_eq!(
        fs::metadata(broker.directory.join("guest-exec.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(broker.directory.join("broker.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

fn inherited_umask(privileged: bool) {
    for mask in [0o777, 0o277, 0] {
        let mut broker = Broker::with_umask(privileged, mask);
        assert_private_files(&broker);
        assert_umask(&broker, mask);
        let mut connection = broker.connect();
        client::hello(&mut connection);
        drop(connection);
        assert!(broker.stop().success());

        let lock = broker.directory.join("broker.lock");
        let inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&lock).unwrap());
        broker.restart_with_umask(privileged, Some(mask));
        assert_private_files(&broker);
        assert_umask(&broker, mask);
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&fs::metadata(&lock).unwrap()),
            inode,
            "restart replaced the persistent lock"
        );
        let mut connection = broker.connect();
        client::hello(&mut connection);
        drop(connection);
        assert!(broker.stop().success());

        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
        let status = fixture::command(&broker.directory, privileged)
            .args(["--umask", &format!("{mask:o}")])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(70));
        assert_eq!(
            fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o644
        );

        fs::remove_file(&lock).unwrap();
        std::os::unix::fs::symlink("untouched", &lock).unwrap();
        let status = fixture::command(&broker.directory, privileged)
            .args(["--umask", &format!("{mask:o}")])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(70));
        assert_eq!(
            fs::read_link(&lock).unwrap(),
            std::path::Path::new("untouched")
        );
        assert!(!broker.directory.join("untouched").exists());

        fs::remove_file(&lock).unwrap();
        let status = fixture::command(&broker.directory, privileged)
            .args(["--umask", &format!("{mask:o}"), "--fault", "bind"])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(70));
        assert!(!broker.directory.join("guest-exec.sock").exists());
        assert_eq!(
            fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

fn assert_umask(broker: &Broker, expected: libc::mode_t) {
    let status = fs::read_to_string(format!("/proc/{}/status", broker.child.id())).unwrap();
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("Umask:"))
        .unwrap();
    assert_eq!(
        libc::mode_t::from_str_radix(value.trim(), 8).unwrap(),
        expected
    );
}

fn restart_stale(privileged: bool) {
    let mut broker = Broker::spawn(privileged);
    let mut first = broker.connect();
    let old = client::hello(&mut first);
    drop(first);
    broker.wait_children(0);
    broker.child.kill().unwrap();
    broker.wait();
    assert!(broker.directory.join("guest-exec.sock").exists());
    broker.restart(privileged);
    let mut next = broker.connect();
    assert_ne!(client::hello(&mut next), old);
    drop(next);
    assert!(broker.stop().success());
}

fn replacement_not_removed(privileged: bool) {
    let mut broker = Broker::spawn(privileged);
    let path = broker.directory.join("guest-exec.sock");
    fs::remove_file(&path).unwrap();
    let replacement = UnixListener::bind(&path).unwrap();
    assert_eq!(
        unsafe { libc::kill(broker.child.id() as i32, libc::SIGTERM) },
        0
    );
    assert!(broker.wait().success());
    assert!(path.exists());
    drop(replacement);
}

fn execution(privileged: bool) {
    let mut broker = Broker::with_guest(privileged);
    let mut first = broker.connect();
    let id = client::hello(&mut first);
    let request = client::request(&first, id);
    let (ready, descriptors) = client::ready(&first, &request);
    assert_eq!(descriptors.len(), 2);
    client::start(&first, &request, &ready);
    client::finish(&first, &request, &ready);
    drop(descriptors);
    drop(first);
    broker.wait_children(0);
    assert!(broker.stop().success());
    let _ = fs::remove_dir_all(
        std::env::temp_dir().join(format!("lasper-account-test-{}", broker.child.id())),
    );
}

fn orphan_cleanup(privileged: bool) {
    let mut broker = Broker::with_guest(privileged);
    let mut victim = broker.connect();
    let id = client::hello(&mut victim);
    let request = client::request_command(&victim, id, &["/bin/sh", "-c", "sleep 60 & wait"]);
    let (ready, descriptors) = client::ready(&victim, &request);
    client::start(&victim, &request, &ready);
    broker.wait_children(1);
    let guardian = broker.children()[0];
    let mut healthy = broker.connect();
    client::hello(&mut healthy);
    assert_eq!(unsafe { libc::kill(guardian, libc::SIGKILL) }, 0);
    // The pidfd is returned with Ready, so observe the assigned worker rather
    // than guessing which numeric descendant survived reparenting.
    let mut exited = libc::pollfd {
        fd: descriptors[1].as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut exited, 1, 5000) }, 1);
    broker.wait_children(1);
    let mut third = broker.connect();
    client::hello(&mut third);
    drop(third);
    drop(healthy);
    drop(victim);
    drop(descriptors);
    assert!(broker.stop().success());
    let _ = fs::remove_dir_all(
        std::env::temp_dir().join(format!("lasper-account-test-{}", broker.child.id())),
    );
}

fn failed_admission(privileged: bool) {
    for fault in ["fork", "pidfd"] {
        let mut broker = Broker::fault(privileged, fault);
        let baseline = fs::read_dir(format!("/proc/{}/fd", broker.child.id()))
            .unwrap()
            .count();
        for _ in 0..8 {
            let mut connection = broker.connect();
            assert_eq!(connection.read(&mut [0]).unwrap(), 0);
            broker.wait_children(0);
        }
        fixture::wait_until(|| {
            fs::read_dir(format!("/proc/{}/fd", broker.child.id()))
                .unwrap()
                .count()
                == baseline
        });
        assert!(broker.stop().success());
    }
}

fn capacity(privileged: bool) {
    let mut broker = Broker::spawn(privileged);
    let mut connections = Vec::new();
    let mut paused = PausedGuardians(Vec::new());
    for _ in 0..32 {
        let before = broker.children();
        let mut connection = broker.connect();
        client::hello(&mut connection);
        let children = broker.children();
        let child = *children.iter().find(|pid| !before.contains(pid)).unwrap();
        assert_eq!(unsafe { libc::kill(child, libc::SIGSTOP) }, 0);
        paused.0.push(child);
        connections.push(connection);
    }
    broker.wait_children(32);
    let mut rejected = broker.connect();
    assert_eq!(rejected.read(&mut [0]).unwrap(), 0);
    assert_eq!(broker.children().len(), 32);
    // Release one occupied slot through actual guardian exit and wait, then
    // prove a new connection is admitted while the other slots remain owned.
    let first = paused.0.remove(0);
    assert_eq!(unsafe { libc::kill(first, libc::SIGCONT) }, 0);
    drop(connections.remove(0));
    broker.wait_children(31);
    let mut admitted = broker.connect();
    client::hello(&mut admitted);
    drop(admitted);
    drop(paused);
    drop(connections);
    assert!(broker.stop().success());
}

struct PausedGuardians(Vec<i32>);
impl Drop for PausedGuardians {
    fn drop(&mut self) {
        for child in &self.0 {
            unsafe {
                libc::kill(*child, libc::SIGCONT);
            }
        }
    }
}

fn stop_paused_guardian(privileged: bool) {
    let mut broker = Broker::with_guest(privileged);
    let mut connection = broker.connect();
    let id = client::hello(&mut connection);
    let request = client::request_command(&connection, id, &["/bin/sh", "-c", "sleep 60 & wait"]);
    let (ready, descriptors) = client::ready(&connection, &request);
    client::start(&connection, &request, &ready);
    broker.wait_children(1);
    let guardian = broker.children()[0];
    assert_eq!(unsafe { libc::kill(guardian, libc::SIGSTOP) }, 0);
    fixture::wait_until(|| {
        fs::read_to_string(format!("/proc/{guardian}/status"))
            .unwrap()
            .lines()
            .any(|line| line.starts_with("State:") && line.contains("T (stopped)"))
    });
    // No fixture SIGCONT: broker shutdown must wake its own stopped guardian.
    assert!(broker.stop().success());
    let mut event = libc::pollfd {
        fd: descriptors[1].as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut event, 1, 0) },
        1,
        "assigned worker survived the stopped guardian's shutdown"
    );
}

fn stop_workers(privileged: bool) {
    let mut broker = Broker::with_guest(privileged);
    let mut clients = Vec::new();
    for _ in 0..2 {
        let mut stream = broker.connect();
        let id = client::hello(&mut stream);
        let request = client::request_command(&stream, id, &["/bin/sh", "-c", "sleep 60 & wait"]);
        let (ready, descriptors) = client::ready(&stream, &request);
        client::start(&stream, &request, &ready);
        clients.push((stream, descriptors));
    }
    assert!(broker.stop().success());
    for (_, descriptors) in clients {
        let mut event = libc::pollfd {
            fd: descriptors[1].as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut event, 1, 0) },
            1,
            "broker returned with an assigned worker alive"
        );
    }
    let _ = fs::remove_dir_all(
        std::env::temp_dir().join(format!("lasper-account-test-{}", broker.child.id())),
    );
}

pub fn standalone(binary: &str) {
    let mut broker = Broker::binary(binary);
    assert_private_files(&broker);
    let limits = fs::read_to_string(format!("/proc/{}/limits", broker.child.id())).unwrap();
    let stack_limit = limits
        .lines()
        .find(|line| line.starts_with("Max stack size"))
        .unwrap();
    assert!(
        stack_limit
            .split_whitespace()
            .skip(3)
            .take(2)
            .all(|value| value == "262144"),
        "{stack_limit}"
    );
    let maps = fs::read_to_string(format!("/proc/{}/maps", broker.child.id())).unwrap();
    let executable: Vec<_> = maps
        .lines()
        .filter(|line| line.ends_with("/bin/guest-exec"))
        .collect();
    assert!(executable
        .iter()
        .any(|line| line.split_whitespace().nth(1) == Some("r-xp")));
    assert!(
        !executable.iter().any(|line| line
            .split_whitespace()
            .nth(1)
            .is_some_and(|flags| flags.contains('w'))),
        "relocated ELF data remained writable: {executable:?}"
    );
    let mut stream = broker.connect();
    let id = client::hello(&mut stream);
    let request = client::request(&stream, id);
    let (ready, descriptors) = client::ready(&stream, &request);
    client::start(&stream, &request, &ready);
    client::finish(&stream, &request, &ready);
    drop(descriptors);
    drop(stream);
    broker.wait_children(0);
    assert!(broker.stop().success());
}

pub const SCENARIOS: &[super::Scenario] = &[
    (
        "broker resumes a stopped guardian and reaps its worker tree",
        stop_paused_guardian,
    ),
    (
        "guardian capacity is released only after child reaping",
        capacity,
    ),
    (
        "broker stop waits for both active worker trees",
        stop_workers,
    ),
    (
        "fork and assigned pidfd failures release connections and reap children",
        failed_admission,
    ),
    (
        "worker executes through broker Ready, Start and bounded Exit",
        execution,
    ),
    (
        "a killed guardian leaves no worker and preserves a healthy guardian",
        orphan_cleanup,
    ),
    (
        "concurrent Hello sessions have independent lifetimes",
        concurrent,
    ),
    (
        "broker stop reaches both guardians without inherited pipe writers",
        stop_live_connections,
    ),
    (
        "killing one guardian preserves another session",
        killed_guardian,
    ),
    (
        "partial Hello expires without blocking new admissions",
        hello_timeout,
    ),
    (
        "another broker cannot replace the live endpoint",
        competing_broker,
    ),
    (
        "private directory and socket permissions are enforced",
        permissions,
    ),
    (
        "launch umask does not change private file modes or relax existing lock checks",
        inherited_umask,
    ),
    (
        "a stopped generation leaves a recoverable stale socket",
        restart_stale,
    ),
    (
        "shutdown leaves a replacement endpoint untouched",
        replacement_not_removed,
    ),
];

use super::{bootstrap::GuardianProcess, client, wire};
use lasper_guest_protocol::*;
use lasper_guest_runtime::session::{GuardianError, PrepareStorageError};
use std::os::fd::AsRawFd;

fn fault(name: &str) -> Option<(libc::c_long, i32)> {
    match name {
        "prepare-allocation-failure" | "prepare-decode-allocation-failure" => {
            Some((libc::SYS_mmap, libc::ENOMEM))
        }
        "preparation-id-failure" => Some((libc::SYS_getrandom, libc::EIO)),
        "worker-creation-failure" => Some((libc::SYS_clone, libc::EAGAIN)),
        _ => None,
    }
}

pub fn install(name: &str) {
    let Some((number, errno)) = fault(name) else {
        return;
    };
    let decode_only = name == "prepare-decode-allocation-failure";
    let mut filter = vec![
        libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            jt: 0,
            jf: if decode_only { 3 } else { 1 },
            k: number as u32,
        },
    ];
    if decode_only {
        // The small wire payload must allocate successfully. Fail only the
        // aligned scratch/metadata mapping allocated by decode_request.
        let bytes = (MAX_PREPARE_UNESCAPE_BYTES
            .next_multiple_of(core::mem::align_of::<PrepareDecodeStorage<'static>>())
            + core::mem::size_of::<PrepareDecodeStorage<'static>>())
        .next_multiple_of(64 * 1024);
        filter.extend([
            libc::sock_filter {
                code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                jt: 0,
                jf: 0,
                k: (core::mem::offset_of!(libc::seccomp_data, args) + core::mem::size_of::<u64>())
                    as u32,
            },
            libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                jt: 0,
                jf: 1,
                k: bytes as u32,
            },
        ]);
    }
    filter.extend([
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ERRNO | errno as u32,
        },
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: libc::SECCOMP_RET_ALLOW,
        },
    ]);
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr().cast_mut(),
    };
    // Restrict only this fixture child, after Go/Hello and filesystem setup.
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        0
    );
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) },
        0,
        "install syscall fault: {}",
        std::io::Error::last_os_error()
    );
}

pub fn expected(name: &str, result: Result<(), GuardianError>) -> bool {
    match (name, result) {
        ("fatal-frame", Err(GuardianError::Control(_))) => true,
        (
            "prepare-allocation-failure" | "prepare-decode-allocation-failure",
            Err(GuardianError::PrepareStorage(PrepareStorageError::System(libc::ENOMEM))),
        ) => true,
        (
            "preparation-id-failure",
            Err(GuardianError::System {
                errno: libc::EIO, ..
            }),
        ) => true,
        (
            "worker-creation-failure",
            Err(GuardianError::System {
                errno: libc::EAGAIN,
                ..
            }),
        ) => true,
        (
            "malformed-prepare",
            Err(GuardianError::PrepareStorage(PrepareStorageError::Frame(_))),
        ) => true,
        ("partial-control", Err(GuardianError::ControlExpired)) => true,
        ("partial-prepare", Err(GuardianError::PrepareExpired)) => true,
        (_, Ok(())) => {
            fault(name).is_none()
                && !matches!(
                    name,
                    "malformed-prepare" | "partial-control" | "partial-prepare"
                )
        }
        _ => false,
    }
}

fn partial_prepare(privileged: bool) {
    let mut guardian =
        GuardianProcess::spawn(super::command("partial-prepare", privileged)).unwrap();
    guardian.release();
    guardian.hello();
    guardian
        .control
        .set_read_timeout(Some(std::time::Duration::from_secs(7)))
        .unwrap();
    let request = client::request(ExecutionKind::Piped, &["/bin/true"]);
    let header = ControlFrameHeader::new_with_version(
        lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        request.request_id(),
        100,
        0,
    )
    .unwrap();
    wire::send(guardian.control.as_raw_fd(), header, b"{", &[]).unwrap();
    let response = wire::receive(guardian.control.as_raw_fd()).unwrap();
    let error = super::client::protocol()
        .decode_error(
            header,
            &request,
            None,
            response.header,
            &response.payload,
            0,
        )
        .unwrap();
    assert_eq!(error.code(), GuestErrorCode::Expired);
    assert!(guardian.wait().success());
    std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("lasper-account-test-{}", guardian.child.id())),
    )
    .unwrap();
}

fn rejection(privileged: bool, name: &str, errno: Option<i32>) {
    let mut guardian = GuardianProcess::spawn(super::command(name, privileged)).unwrap();
    guardian.release();
    guardian.hello();
    let request = client::request(ExecutionKind::Piped, &["/bin/true"]);
    let mut bytes = [0; 8192];
    let length = super::client::protocol()
        .encode_prepare_frame(&request, &mut bytes)
        .unwrap();
    let mut header = ControlFrameHeader::decode(&bytes[..CONTROL_FRAME_HEADER_BYTES]).unwrap();
    let payload = if name == "malformed-prepare" {
        header = ControlFrameHeader::new_with_version(
            lasper_guest_protocol::ProtocolVersion { major: 1, minor: 0 },
            MessageKind::PrepareRequest,
            request.request_id(),
            1,
            0,
        )
        .unwrap();
        b"{".as_slice()
    } else {
        &bytes[CONTROL_FRAME_HEADER_BYTES..length]
    };
    wire::send(guardian.control.as_raw_fd(), header, payload, &[]).unwrap();
    let response = wire::receive(guardian.control.as_raw_fd()).unwrap();
    let error = super::client::protocol()
        .decode_error(
            header,
            &request,
            None,
            response.header,
            &response.payload,
            response.descriptors.len(),
        )
        .unwrap();
    assert_eq!(
        error.code(),
        if errno.is_some() {
            GuestErrorCode::System
        } else {
            GuestErrorCode::InvalidRequest
        }
    );
    assert_eq!(error.errno(), errno);
    assert!(guardian.wait().success());
    std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("lasper-account-test-{}", guardian.child.id())),
    )
    .unwrap();
}

pub const SCENARIOS: &[super::Scenario] = &[
    (
        "Prepare mapping failure sends correlated Error without allocating a reply",
        |p| rejection(p, "prepare-allocation-failure", Some(libc::ENOMEM)),
    ),
    (
        "Prepare decode mapping failure sends correlated Error after receiving the payload",
        |p| rejection(p, "prepare-decode-allocation-failure", Some(libc::ENOMEM)),
    ),
    ("preparation ID failure sends correlated Error", |p| {
        rejection(p, "preparation-id-failure", Some(libc::EIO))
    }),
    ("worker creation failure sends correlated Error", |p| {
        rejection(p, "worker-creation-failure", Some(libc::EAGAIN))
    }),
    ("malformed Prepare sends correlated Error", |p| {
        rejection(p, "malformed-prepare", None)
    }),
    (
        "expired partial Prepare has its own rejection send budget",
        partial_prepare,
    ),
];

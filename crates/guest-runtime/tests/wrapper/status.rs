use super::*;

fn passcred(fd: i32) -> i32 {
    let mut value = -1i32;
    let mut length = core::mem::size_of::<i32>() as libc::socklen_t;
    assert_eq!(
        unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&mut value as *mut i32).cast(),
                &mut length,
            )
        },
        0
    );
    value
}

#[test]
fn exec_status_channel_is_cloexec_and_carries_errno_without_credentials() {
    let (writer, reader) = linux::seqpacket_pair().unwrap();
    for fd in [writer.raw(), reader.raw()] {
        assert_eq!(passcred(fd), 0);
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
    }
    report_exec_error(&writer, EACCES);
    let mut error = [0; 4];
    assert_eq!(reader.read(&mut error), Ok(4));
    assert_eq!(i32::from_ne_bytes(error), EACCES);
    drop(writer);
    assert_eq!(reader.read(&mut error), Ok(0));
}

#[test]
fn worker_identity_still_enables_credentials_only_on_receiver() {
    let (receiver, sender) = linux::identity_pair().unwrap();
    assert_eq!(passcred(receiver.raw()), 1);
    assert_eq!(passcred(sender.raw()), 0);
}

#[test]
fn broken_exec_status_channel_does_not_deliver_sigpipe() {
    let (writer, reader) = linux::seqpacket_pair().unwrap();
    drop(reader);
    // Change signal state only in a child; the parallel test runner must retain
    // its dispositions/mask. The child uses only async-signal-safe operations.
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        unsafe {
            if libc::signal(libc::SIGPIPE, libc::SIG_DFL) == libc::SIG_ERR {
                libc::_exit(1);
            }
            let mut mask = core::mem::zeroed();
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGPIPE);
            if libc::sigprocmask(libc::SIG_UNBLOCK, &mask, core::ptr::null_mut()) != 0 {
                libc::_exit(1);
            }
        }
        report_exec_error(&writer, EACCES);
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(child, &mut status, 0) };
        if result == child {
            break;
        }
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EINTR);
    }
    assert!(
        libc::WIFEXITED(status),
        "exec-status sender was killed by a signal"
    );
    assert_eq!(libc::WEXITSTATUS(status), 0);
}

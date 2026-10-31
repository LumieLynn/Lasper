//! Single-threaded, host-executable connector. Namespace entry changes where
//! the fixed socket is connected; no guest executable or interpreter is run.

use sendfd::SendWithFd;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::time::Duration;

pub(super) fn run(channel: UnixDatagram) -> io::Result<()> {
    // Also bound the internal entry if its parent disappears before sending.
    channel.set_read_timeout(Some(Duration::from_secs(1)))?;
    channel.set_write_timeout(Some(Duration::from_secs(1)))?;
    unsafe { libc::alarm(2) };
    let result = receive_target(&channel).and_then(|target| connect_in_target(&channel, target));
    if let Err(error) = result {
        let code = error.raw_os_error().unwrap_or(libc::EIO);
        respond(&channel, false, code, &[])?;
    }
    Ok(())
}

fn receive_target(channel: &UnixDatagram) -> io::Result<Vec<OwnedFd>> {
    let mut request = [0u8; 8];
    let (bytes, descriptors) = super::channel::receive(channel.as_fd(), &mut request)?;
    if &request[..bytes] != b"connect" || descriptors.len() != 4 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(descriptors)
}

fn connect_in_target(channel: &UnixDatagram, target: Vec<OwnedFd>) -> io::Result<()> {
    let [pid, mount, user, root]: [OwnedFd; 4] = target
        .try_into()
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let pid_differs = different_namespace(&pid, "pid", libc::CLONE_NEWPID)?;
    let mount_differs = different_namespace(&mount, "mnt", libc::CLONE_NEWNS)?;
    let user_differs = different_namespace(&user, "user", libc::CLONE_NEWUSER)?;
    let admin = has_sys_admin()?;
    if !admin && user_differs {
        enter(&user, libc::CLONE_NEWUSER)?;
    }
    if pid_differs {
        enter(&pid, libc::CLONE_NEWPID)?;
    }
    if mount_differs {
        enter(&mount, libc::CLONE_NEWNS)?;
    }
    if admin && user_differs {
        enter(&user, libc::CLONE_NEWUSER)?;
    }
    check(unsafe { libc::fchdir(root.as_raw_fd()) })?;
    check(unsafe { libc::chroot(c".".as_ptr()) })?;
    check(unsafe { libc::chdir(c"/".as_ptr()) })?;
    if user_differs {
        // Some user namespaces prohibit setgroups. Only the fixed root bus
        // connection is made here; no guest process inherits these groups.
        let result = unsafe { libc::setgroups(0, std::ptr::null()) };
        if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
            return Err(io::Error::last_os_error());
        }
    }
    check(unsafe { libc::setresgid(0, 0, 0) })?;
    check(unsafe { libc::setresuid(0, 0, 0) })?;
    drop((pid, mount, user, root));

    // setns(CLONE_NEWPID) applies to children. This fork is confined to the
    // freshly exec'd single-threaded connector, never the Tokio parent.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(io::Error::last_os_error());
    }
    if child == 0 {
        unsafe { libc::alarm(1) };
        let result = connect_and_send(channel);
        if let Err(error) = result {
            let code = error.raw_os_error().unwrap_or(libc::EIO);
            let _ = respond(channel, true, code, &[]);
        }
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    loop {
        if unsafe { libc::waitpid(child, &mut status, 0) } >= 0 {
            return if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(libc::ETIMEDOUT))
            };
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn connect_and_send(channel: &UnixDatagram) -> io::Result<()> {
    let mut stream = UnixStream::connect("/run/dbus/system_bus_socket")?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    // D-Bus associates the first NUL byte with the connecting guest identity.
    // Send it here, before handing the FD back to a host-namespace zbus client.
    // The runtime adapter suppresses zbus's duplicate initial NUL.
    stream.write_all(&[0])?;
    respond(channel, false, 0, &[stream.as_raw_fd()])?;
    Ok(())
}

fn respond(channel: &UnixDatagram, socket_error: bool, errno: i32, fds: &[i32]) -> io::Result<()> {
    let mut payload = [0u8; 8];
    payload[..4].copy_from_slice(&u32::from(socket_error).to_ne_bytes());
    payload[4..].copy_from_slice(&errno.to_ne_bytes());
    channel.send_with_fd(&payload, fds)?;
    Ok(())
}

fn different_namespace(fd: &OwnedFd, name: &str, kind: i32) -> io::Result<bool> {
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::NS_GET_NSTYPE) } != kind {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let other = File::from(fd.try_clone()?).metadata()?;
    let own = std::fs::metadata(format!("/proc/self/ns/{name}"))?;
    Ok(other.dev() != own.dev() || other.ino() != own.ino())
}

fn has_sys_admin() -> io::Result<bool> {
    use linux_raw_sys::general::{
        __NR_capget, __user_cap_data_struct, __user_cap_header_struct, _LINUX_CAPABILITY_VERSION_3,
    };
    let mut header = __user_cap_header_struct {
        version: _LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data: [__user_cap_data_struct; 2] = unsafe { std::mem::zeroed() };
    if unsafe { libc::syscall(__NR_capget as libc::c_long, &mut header, data.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(data[0].effective & (1 << 21) != 0)
}

fn enter(fd: &OwnedFd, kind: i32) -> io::Result<()> {
    check(unsafe { libc::setns(fd.as_raw_fd(), kind) })
}

fn check(result: i32) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

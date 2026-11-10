use core::ffi::CStr;
use core::fmt::Write;
use core::mem::{align_of, offset_of, size_of};

use crate::linux::filesystem::{self, Statx};
use crate::linux::uapi::{
    errno::{EACCES, EINTR, EINVAL, ENOENT},
    file::{
        AT_FDCWD, O_CLOEXEC, O_CREAT, O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK, O_RDWR, S_IFDIR, S_IFMT,
        S_IFREG, S_IFSOCK,
    },
    nr,
    socket::{AF_UNIX, SOCK_STREAM},
};
use crate::linux::{result_or_errno, Descriptor};
use crate::syscall::{syscall1, syscall2, syscall3, syscall4};

const SOCKET_NAME: &CStr = c"guest-exec.sock";
const LOCK_NAME: &CStr = c"broker.lock";
const LOCK_EXCLUSIVE_NONBLOCKING: usize = 2 | 4;

pub(super) struct Listener {
    socket: Descriptor,
    directory: Descriptor,
    lock: Descriptor,
    inode: u64,
}

impl Listener {
    pub(super) fn bind(path: &CStr) -> Result<Self, i32> {
        // The private lock and socket need owner access regardless of the
        // launcher's mask. Restore it before any guardian can be forked.
        let old_mask = result_or_errno(unsafe { syscall1(nr::UMASK, 0o077) })?;
        let listener = Self::bind_private(path);
        result_or_errno(unsafe { syscall1(nr::UMASK, old_mask) })?;
        listener
    }

    fn bind_private(path: &CStr) -> Result<Self, i32> {
        // Pin the private directory so a rename cannot redirect later writes.
        // SAFETY: The syscall borrows a live path and returns a fresh owned FD.
        let directory = Descriptor::adopt(unsafe {
            syscall4(
                nr::OPENAT,
                AT_FDCWD,
                path.as_ptr() as usize,
                O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC,
                0,
            )
        })?;
        let stat = filesystem::ownership(&directory)?;
        if stat.mode & S_IFMT != S_IFDIR || stat.mode & 0o7777 != 0o700 || stat.uid != 0 {
            return Err(EACCES);
        }
        // SAFETY: directory remains open; LOCK_NAME is NUL-terminated.
        let lock = Descriptor::adopt(unsafe {
            syscall4(
                nr::OPENAT,
                directory.raw() as usize,
                LOCK_NAME.as_ptr() as usize,
                O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK,
                0o600,
            )
        })?;
        let stat = filesystem::ownership(&lock)?;
        if stat.mode & S_IFMT != S_IFREG
            || stat.uid != 0
            || stat.mode & 0o7777 != 0o600
            || stat.links != 1
        {
            return Err(EACCES);
        }
        result_or_errno(unsafe {
            syscall2(nr::FLOCK, lock.raw() as usize, LOCK_EXCLUSIVE_NONBLOCKING)
        })?;
        // Only the broker holding this directory's persistent lock may remove
        // a stale endpoint. The lock file itself is never unlinked.
        match stat_entry(&directory) {
            Ok(stat) if stat.mode & S_IFMT == S_IFSOCK => unlink(&directory)?,
            Ok(_) => return Err(EINVAL),
            Err(ENOENT) => {}
            Err(errno) => return Err(errno),
        }
        let socket = Descriptor::adopt(unsafe {
            syscall3(nr::SOCKET, AF_UNIX, SOCK_STREAM | O_NONBLOCK | O_CLOEXEC, 0)
        })?;
        let address = Address::new(directory.raw())?;
        // SAFETY: Address is the Linux sockaddr_un layout and includes its NUL.
        result_or_errno(unsafe {
            syscall3(
                nr::BIND,
                socket.raw() as usize,
                &address as *const _ as usize,
                address.length(),
            )
        })?;
        let stat = stat_entry(&directory)?;
        let listener = Self {
            socket,
            directory,
            lock,
            inode: stat.inode,
        };
        result_or_errno(unsafe {
            syscall4(
                nr::FCHMODAT,
                listener.directory.raw() as usize,
                SOCKET_NAME.as_ptr() as usize,
                0o600,
                0,
            )
        })?;
        result_or_errno(unsafe {
            syscall2(
                nr::LISTEN,
                listener.socket.raw() as usize,
                super::registry::CAPACITY,
            )
        })?;
        Ok(listener)
    }

    pub(super) fn raw(&self) -> i32 {
        self.socket.raw()
    }

    pub(super) fn descriptors(&self) -> [i32; 3] {
        [self.socket.raw(), self.directory.raw(), self.lock.raw()]
    }

    pub(super) fn accept(&self) -> Result<Descriptor, i32> {
        loop {
            // No peer address is needed; host authorization is performed by the
            // elevated client, not by interpreting mapped peer UID 0 here.
            let result = unsafe {
                syscall4(
                    nr::ACCEPT4,
                    self.raw() as usize,
                    0,
                    0,
                    O_CLOEXEC | O_NONBLOCK,
                )
            };
            if result != -(EINTR as isize) {
                return Descriptor::adopt(result);
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Do not remove a replacement someone installed while we were running.
        if stat_entry(&self.directory)
            .is_ok_and(|stat| stat.inode == self.inode && stat.mode & S_IFMT == S_IFSOCK)
        {
            let _ = unlink(&self.directory);
        }
    }
}

fn stat_entry(directory: &Descriptor) -> Result<Statx, i32> {
    filesystem::stat_at(directory, SOCKET_NAME)
}

fn unlink(directory: &Descriptor) -> Result<(), i32> {
    result_or_errno(unsafe {
        syscall3(
            nr::UNLINKAT,
            directory.raw() as usize,
            SOCKET_NAME.as_ptr() as usize,
            0,
        )
    })
    .map(|_| ())
}

#[repr(C)]
struct Address {
    family: u16,
    path: [u8; 108],
}
impl Address {
    fn new(directory: i32) -> Result<Self, i32> {
        let mut value = Self {
            family: AF_UNIX as u16,
            path: [0; 108],
        };
        let mut path = SocketPath {
            bytes: &mut value.path,
            length: 0,
        };
        write!(path, "/proc/self/fd/{directory}/guest-exec.sock").map_err(|_| EINVAL)?;
        Ok(value)
    }
    fn length(&self) -> usize {
        2 + self.path.iter().position(|byte| *byte == 0).unwrap() + 1
    }
}
struct SocketPath<'a> {
    bytes: &'a mut [u8],
    length: usize,
}
impl core::fmt::Write for SocketPath<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self
            .length
            .checked_add(text.len())
            .ok_or(core::fmt::Error)?;
        if end >= self.bytes.len() {
            return Err(core::fmt::Error);
        }
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}
const _: () = {
    assert!(size_of::<Address>() == 110);
    assert!(align_of::<Address>() == 2);
    assert!(offset_of!(Address, path) == 2);
};

// Shared by the library and the standalone ELF entry. Architecture-specific
// values follow the corresponding Linux UAPI headers.
pub(crate) mod errno {
    pub(crate) const EPERM: i32 = 1;
    pub(crate) const ENOENT: i32 = 2;
    pub(crate) const ESRCH: i32 = 3;
    pub(crate) const EINTR: i32 = 4;
    pub(crate) const EIO: i32 = 5;
    pub(crate) const E2BIG: i32 = 7;
    pub(crate) const ECHILD: i32 = 10;
    pub(crate) const EAGAIN: i32 = 11;
    pub(crate) const ENOMEM: i32 = 12;
    pub(crate) const EACCES: i32 = 13;
    pub(crate) const EEXIST: i32 = 17;
    pub(crate) const EINVAL: i32 = 22;
    pub(crate) const EFBIG: i32 = 27;
}

pub(crate) mod file {
    pub(crate) const AT_FDCWD: usize = -100isize as usize;
    pub(crate) const AT_EMPTY_PATH: usize = 0x1000;
    pub(crate) const O_RDWR: usize = 2;
    pub(crate) const O_NOCTTY: usize = 0o400;
    pub(crate) const O_NONBLOCK: usize = 0o4000;
    #[cfg(target_arch = "x86_64")]
    pub(crate) const O_DIRECTORY: usize = 0o200000;
    // arm64 overrides asm-generic/fcntl.h for AArch32 compatibility.
    #[cfg(target_arch = "aarch64")]
    pub(crate) const O_DIRECTORY: usize = 0o40000;
    pub(crate) const O_CLOEXEC: usize = 0o2000000;
    pub(crate) const O_PATH: usize = 0o10000000;
    pub(crate) const F_GETFD: usize = 1;
    pub(crate) const F_SETFD: usize = 2;
    pub(crate) const F_GETFL: usize = 3;
    pub(crate) const F_SETFL: usize = 4;
    pub(crate) const F_DUPFD_CLOEXEC: usize = 1030;
    pub(crate) const FD_CLOEXEC: usize = 1;
    pub(crate) const X_OK: usize = 1;
    pub(crate) const W_OK: usize = 2;
    pub(crate) const STATX_TYPE: usize = 1;
    pub(crate) const STATX_INO: usize = 0x100;
    pub(crate) const S_IFMT: u16 = 0o170000;
    pub(crate) const S_IFREG: u16 = 0o100000;
    pub(crate) const S_IFSOCK: u16 = 0o140000;
}

pub(crate) mod signal {
    pub(crate) const SIGHUP: i32 = 1;
    pub(crate) const SIGINT: i32 = 2;
    pub(crate) const SIGQUIT: i32 = 3;
    pub(crate) const SIGKILL: i32 = 9;
    pub(crate) const SIGPIPE: i32 = 13;
    pub(crate) const SIGTERM: i32 = 15;
    pub(crate) const SIGCHLD: i32 = 17;
    pub(crate) const SIGCONT: i32 = 18;
    pub(crate) const SIGTSTP: i32 = 20;
    pub(crate) const SIGTTIN: i32 = 21;
    pub(crate) const SIGTTOU: i32 = 22;
    pub(crate) const SIG_BLOCK: usize = 0;
    pub(crate) const SIG_SETMASK: usize = 2;
}

pub(crate) mod socket {
    pub(crate) const AF_UNIX: usize = 1;
    pub(crate) const SOCK_SEQPACKET: usize = 5;
    pub(crate) const SOL_SOCKET: i32 = 1;
    pub(crate) const SO_PASSCRED: usize = 16;
    pub(crate) const SCM_RIGHTS: i32 = 1;
    pub(crate) const SCM_PIDFD: i32 = 4;
    pub(crate) const MSG_CTRUNC: u32 = 8;
    pub(crate) const MSG_TRUNC: u32 = 32;
    pub(crate) const MSG_DONTWAIT: usize = 0x40;
    pub(crate) const MSG_NOSIGNAL: usize = 0x4000;
    pub(crate) const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;
    pub(crate) const SHUT_RDWR: usize = 2;
}

pub(crate) mod terminal {
    pub(crate) const TIOCSCTTY: usize = 0x540e;
    pub(crate) const TIOCSPGRP: usize = 0x5410;
    pub(crate) const TIOCSWINSZ: usize = 0x5414;
    pub(crate) const TIOCSPTLCK: usize = 0x4004_5431;
    pub(crate) const TIOCGPTPEER: usize = 0x5441;
}

pub(crate) mod process {
    pub(crate) const PR_SET_CHILD_SUBREAPER: usize = 36;
    pub(crate) const WNOHANG: usize = 1;
    pub(crate) const WEXITED: usize = 4;
    pub(crate) const WNOWAIT: usize = 0x0100_0000;
    pub(crate) const P_PID: usize = 1;
    pub(crate) const CLD_EXITED: i32 = 1;
}

pub(crate) mod time {
    pub(crate) const CLOCK_MONOTONIC: usize = 1;
}

pub(crate) mod random {
    pub(crate) const GRND_NONBLOCK: usize = 1;
}

pub(crate) mod poll {
    pub(crate) const POLLIN: i16 = 1;
    pub(crate) const POLLOUT: i16 = 4;
    pub(crate) const POLLERR: i16 = 8;
    pub(crate) const POLLHUP: i16 = 16;
    pub(crate) const POLLRDHUP: i16 = 0x2000;
}

pub(crate) mod memory {
    pub(crate) const PROT_READ: usize = 1;
    pub(crate) const PROT_WRITE: usize = 2;
    pub(crate) const MAP_PRIVATE: usize = 2;
    pub(crate) const MAP_ANONYMOUS: usize = 0x20;
}

#[cfg(target_arch = "x86_64")]
pub(crate) mod nr {
    pub(crate) const READ: usize = 0;
    pub(crate) const WRITE: usize = 1;
    pub(crate) const CLOSE: usize = 3;
    pub(crate) const MMAP: usize = 9;
    pub(crate) const MUNMAP: usize = 11;
    pub(crate) const RT_SIGACTION: usize = 13;
    pub(crate) const RT_SIGPROCMASK: usize = 14;
    pub(crate) const IOCTL: usize = 16;
    pub(crate) const GETPID: usize = 39;
    pub(crate) const SENDMSG: usize = 46;
    pub(crate) const RECVMSG: usize = 47;
    pub(crate) const SHUTDOWN: usize = 48;
    pub(crate) const SOCKETPAIR: usize = 53;
    pub(crate) const SETSOCKOPT: usize = 54;
    pub(crate) const CLONE: usize = 56;
    pub(crate) const EXECVE: usize = 59;
    pub(crate) const WAIT4: usize = 61;
    pub(crate) const KILL: usize = 62;
    pub(crate) const FCNTL: usize = 72;
    pub(crate) const CHDIR: usize = 80;
    pub(crate) const FCHMOD: usize = 91;
    pub(crate) const FCHOWN: usize = 93;
    pub(crate) const SETSID: usize = 112;
    pub(crate) const GETGROUPS: usize = 115;
    pub(crate) const SETGROUPS: usize = 116;
    pub(crate) const SETRESUID: usize = 117;
    pub(crate) const GETRESUID: usize = 118;
    pub(crate) const SETRESGID: usize = 119;
    pub(crate) const GETRESGID: usize = 120;
    pub(crate) const PRCTL: usize = 157;
    pub(crate) const GETTID: usize = 186;
    pub(crate) const GETDENTS64: usize = 217;
    pub(crate) const CLOCK_GETTIME: usize = 228;
    pub(crate) const EXIT_GROUP: usize = 231;
    pub(crate) const WAITID: usize = 247;
    pub(crate) const OPENAT: usize = 257;
    pub(crate) const FACCESSAT: usize = 269;
    pub(crate) const PPOLL: usize = 271;
    pub(crate) const SIGNALFD4: usize = 289;
    pub(crate) const DUP3: usize = 292;
    pub(crate) const PIPE2: usize = 293;
    pub(crate) const GETRANDOM: usize = 318;
    pub(crate) const STATX: usize = 332;
    pub(crate) const PIDFD_OPEN: usize = 434;
}

#[cfg(target_arch = "aarch64")]
pub(crate) mod nr {
    pub(crate) const DUP3: usize = 24;
    pub(crate) const FCNTL: usize = 25;
    pub(crate) const IOCTL: usize = 29;
    pub(crate) const FACCESSAT: usize = 48;
    pub(crate) const CHDIR: usize = 49;
    pub(crate) const FCHMOD: usize = 52;
    pub(crate) const FCHOWN: usize = 55;
    pub(crate) const OPENAT: usize = 56;
    pub(crate) const CLOSE: usize = 57;
    pub(crate) const PIPE2: usize = 59;
    pub(crate) const GETDENTS64: usize = 61;
    pub(crate) const READ: usize = 63;
    pub(crate) const WRITE: usize = 64;
    pub(crate) const PPOLL: usize = 73;
    pub(crate) const SIGNALFD4: usize = 74;
    pub(crate) const EXIT_GROUP: usize = 94;
    pub(crate) const WAITID: usize = 95;
    pub(crate) const CLOCK_GETTIME: usize = 113;
    pub(crate) const KILL: usize = 129;
    pub(crate) const RT_SIGACTION: usize = 134;
    pub(crate) const RT_SIGPROCMASK: usize = 135;
    pub(crate) const SETRESUID: usize = 147;
    pub(crate) const GETRESUID: usize = 148;
    pub(crate) const SETRESGID: usize = 149;
    pub(crate) const GETRESGID: usize = 150;
    pub(crate) const SETSID: usize = 157;
    pub(crate) const GETGROUPS: usize = 158;
    pub(crate) const SETGROUPS: usize = 159;
    pub(crate) const PRCTL: usize = 167;
    pub(crate) const GETPID: usize = 172;
    pub(crate) const GETTID: usize = 178;
    pub(crate) const SOCKETPAIR: usize = 199;
    pub(crate) const SETSOCKOPT: usize = 208;
    pub(crate) const SHUTDOWN: usize = 210;
    pub(crate) const SENDMSG: usize = 211;
    pub(crate) const RECVMSG: usize = 212;
    pub(crate) const MUNMAP: usize = 215;
    pub(crate) const CLONE: usize = 220;
    pub(crate) const EXECVE: usize = 221;
    pub(crate) const MMAP: usize = 222;
    pub(crate) const WAIT4: usize = 260;
    pub(crate) const GETRANDOM: usize = 278;
    pub(crate) const STATX: usize = 291;
    pub(crate) const PIDFD_OPEN: usize = 434;
}

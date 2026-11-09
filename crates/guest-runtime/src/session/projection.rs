use core::ffi::CStr;
use lasper_guest_protocol::{
    ProjectionObservations, ProjectionRequests, SocketPathAccess, SocketPathIdentity,
    SocketPathObservation, X11PathObservations, MAX_PROJECTION_PATH_BYTES,
};

use super::linux::{filesystem, result_or_errno};
use super::{WorkerBootstrap, WorkerFailure, WorkerStage};
use crate::linux::uapi::{
    errno::EINVAL,
    file::{AT_FDCWD, S_IFMT, S_IFSOCK, W_OK},
    nr,
};
use crate::syscall::syscall3;

pub(super) fn entry(worker: WorkerBootstrap<'_>) -> ! {
    worker.run(observe)
}

fn observe(requests: &ProjectionRequests) -> Result<ProjectionObservations, WorkerFailure> {
    Ok(ProjectionObservations::new(
        requests
            .wayland()
            .map(|request| observe_path(request.target()))
            .transpose()?,
        requests
            .x11()
            .map(|request| {
                Ok(X11PathObservations::new(
                    observe_path(request.mount_target())?,
                    observe_path(request.client_path())?,
                ))
            })
            .transpose()?,
    ))
}

fn observe_path(path: &str) -> Result<SocketPathObservation, WorkerFailure> {
    let mut bytes = [0; MAX_PROJECTION_PATH_BYTES + 1];
    if path.len() >= bytes.len() || path.as_bytes().contains(&0) {
        return Err(invalid());
    }
    bytes[..path.len()].copy_from_slice(path.as_bytes());
    let path = CStr::from_bytes_with_nul(&bytes[..=path.len()]).map_err(|_| invalid())?;
    let Ok(before) = filesystem::stat_path(path) else {
        return observation(SocketPathAccess::Unavailable, None);
    };
    if before.mode & S_IFMT != S_IFSOCK {
        return observation(SocketPathAccess::NotSocket, None);
    }
    // Account::enter has already made all three UID/GID sets equal and
    // installed supplementary groups, so faccessat checks the selected account.
    let writable =
        result_or_errno(unsafe { syscall3(nr::FACCESSAT, AT_FDCWD, path.as_ptr() as usize, W_OK) })
            .is_ok();
    let identity = SocketPathIdentity::new(before.device(), before.inode);
    let Ok(after) = filesystem::stat_path(path) else {
        return observation(SocketPathAccess::Unavailable, None);
    };
    if after.mode & S_IFMT != S_IFSOCK
        || SocketPathIdentity::new(after.device(), after.inode) != identity
    {
        return observation(SocketPathAccess::Unavailable, None);
    }
    observation(
        if writable {
            SocketPathAccess::Accessible
        } else {
            SocketPathAccess::Denied
        },
        Some(identity),
    )
}

fn observation(
    access: SocketPathAccess,
    identity: Option<SocketPathIdentity>,
) -> Result<SocketPathObservation, WorkerFailure> {
    SocketPathObservation::new(access, identity).map_err(|_| invalid())
}

fn invalid() -> WorkerFailure {
    WorkerFailure::new(WorkerStage::Projection, EINVAL)
}

#[cfg(test)]
#[path = "../../tests/session/projection.rs"]
mod tests;

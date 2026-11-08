use crate::session::linux::result_or_errno;
use crate::session::{WorkerFailure, WorkerStage};
use crate::syscall::{syscall2, syscall3};

use super::{Account, MAX_GROUPS};

const EPERM: i32 = 1;

pub(super) fn switch(account: &Account) -> Result<(), WorkerFailure> {
    let failure = |stage, errno| WorkerFailure::new(stage, errno);
    let mut current = [0; MAX_GROUPS];
    let count =
        groups(&mut current).map_err(|errno| failure(WorkerStage::SupplementaryGroups, errno))?;
    // A denied setgroups is safe only when no group change is needed. Compare
    // sets rather than relying on the order returned by getgroups.
    if count > MAX_GROUPS || current[..count] != *account.groups() {
        result_or_errno(unsafe {
            syscall2(
                nr::SETGROUPS,
                account.group_count,
                account.groups.as_ptr() as usize,
            )
        })
        .map_err(|errno| failure(WorkerStage::SupplementaryGroups, errno))?;
    }
    let gid = account.gid as usize;
    result_or_errno(unsafe { syscall3(nr::SETRESGID, gid, gid, gid) })
        .map_err(|errno| failure(WorkerStage::GroupIdentity, errno))?;
    let uid = account.uid as usize;
    result_or_errno(unsafe { syscall3(nr::SETRESUID, uid, uid, uid) })
        .map_err(|errno| failure(WorkerStage::UserIdentity, errno))?;

    verify_ids(nr::GETRESGID, account.gid)
        .map_err(|errno| failure(WorkerStage::GroupIdentity, errno))?;
    verify_ids(nr::GETRESUID, account.uid)
        .map_err(|errno| failure(WorkerStage::UserIdentity, errno))?;
    let count =
        groups(&mut current).map_err(|errno| failure(WorkerStage::SupplementaryGroups, errno))?;
    if count > MAX_GROUPS || current[..count] != *account.groups() {
        return Err(failure(WorkerStage::SupplementaryGroups, EPERM));
    }
    Ok(())
}

pub(super) fn own_terminal(uid: u32, gid: u32) -> Result<(), i32> {
    result_or_errno(unsafe { syscall3(nr::FCHOWN, 0, uid as usize, gid as usize) })?;
    result_or_errno(unsafe { syscall2(nr::FCHMOD, 0, 0o600) })?;
    Ok(())
}

fn groups(output: &mut [u32; MAX_GROUPS]) -> Result<usize, i32> {
    let count = result_or_errno(unsafe { syscall2(nr::GETGROUPS, 0, 0) })?;
    if count > output.len() {
        return Ok(count);
    }
    let count = result_or_errno(unsafe {
        syscall2(nr::GETGROUPS, output.len(), output.as_mut_ptr() as usize)
    })?;
    output[..count].sort_unstable();
    let mut unique = 0;
    for index in 0..count {
        if unique == 0 || output[index] != output[unique - 1] {
            output[unique] = output[index];
            unique += 1;
        }
    }
    Ok(unique)
}

fn verify_ids(number: usize, expected: u32) -> Result<(), i32> {
    let mut ids = [0u32; 3];
    // SAFETY: These pointers name distinct, writable uid_t/gid_t slots.
    result_or_errno(unsafe {
        syscall3(
            number,
            ids.as_mut_ptr() as usize,
            ids.as_mut_ptr().add(1) as usize,
            ids.as_mut_ptr().add(2) as usize,
        )
    })?;
    if ids == [expected; 3] {
        Ok(())
    } else {
        Err(EPERM)
    }
}

#[cfg(target_arch = "x86_64")]
mod nr {
    pub(super) const FCHMOD: usize = 91;
    pub(super) const FCHOWN: usize = 93;
    pub(super) const GETGROUPS: usize = 115;
    pub(super) const SETGROUPS: usize = 116;
    pub(super) const SETRESUID: usize = 117;
    pub(super) const GETRESUID: usize = 118;
    pub(super) const SETRESGID: usize = 119;
    pub(super) const GETRESGID: usize = 120;
}

#[cfg(target_arch = "aarch64")]
mod nr {
    pub(super) const FCHMOD: usize = 52;
    pub(super) const FCHOWN: usize = 55;
    pub(super) const SETRESUID: usize = 147;
    pub(super) const GETRESUID: usize = 148;
    pub(super) const SETRESGID: usize = 149;
    pub(super) const GETRESGID: usize = 150;
    pub(super) const GETGROUPS: usize = 158;
    pub(super) const SETGROUPS: usize = 159;
}

use crate::linux::uapi::errno::{E2BIG, EEXIST, EINVAL, ENOENT};
mod credentials;
mod files;

use core::ffi::CStr;
use core::mem::MaybeUninit;
use lasper_guest_protocol::MAX_GUEST_USER_NAME_BYTES;
use lasper_local_accounts::{group_entries, passwd_entries};

use crate::session::{WorkerFailure, WorkerStage};

pub(super) const MAX_ACCOUNT_FILE_BYTES: usize = 64 * 1024;
const MAX_GROUPS: usize = 256;
const MAX_PATH_BYTES: usize = 4096;

/// This snapshot stays with the worker across Prepared and Start. Neither the
/// identity reply nor exec resolves the account a second time.
pub(super) struct Account {
    pub(super) username: CString<{ MAX_GUEST_USER_NAME_BYTES + 1 }>,
    pub(super) uid: u32,
    pub(super) gid: u32,
    pub(super) home: CString<{ MAX_PATH_BYTES + 1 }>,
    pub(super) shell: CString<{ MAX_PATH_BYTES + 1 }>,
    groups: [u32; MAX_GROUPS],
    group_count: usize,
}

impl Account {
    pub(super) fn initialize(storage: &mut MaybeUninit<Self>) -> &mut Self {
        storage.write(Self {
            username: CString::empty(),
            uid: 0,
            gid: 0,
            home: CString::empty(),
            shell: CString::empty(),
            groups: [0; MAX_GROUPS],
            group_count: 0,
        })
    }

    pub(super) fn load(&mut self, user: &str, buffer: &mut [u8]) -> Result<(), WorkerFailure> {
        let length = files::read(c"/etc/passwd", buffer)
            .map_err(|errno| WorkerFailure::new(WorkerStage::Account, errno))?;
        self.read_passwd(user, &buffer[..length])?;
        match files::read(c"/etc/group", buffer) {
            Ok(length) => self.read_groups(&buffer[..length])?,
            // Minimal rootfs images can omit the group database. Credential
            // setup must still replace inherited groups with the primary GID.
            Err(ENOENT) => {}
            Err(errno) => return Err(WorkerFailure::new(WorkerStage::GroupDatabase, errno)),
        }
        Ok(())
    }

    fn read_passwd(&mut self, user: &str, content: &[u8]) -> Result<(), WorkerFailure> {
        let error = |errno| WorkerFailure::new(WorkerStage::Account, errno);
        let mut selected = None;
        for entry in passwd_entries(content) {
            // A malformed line might be another definition of the requested
            // account. Execution cannot use the partial snapshot shown by UI.
            let entry = entry.map_err(|_| error(EINVAL))?;
            if entry.username == user {
                if selected.is_some() {
                    return Err(error(EEXIST));
                }
                selected = Some(entry);
            }
        }
        let entry = selected.ok_or(error(ENOENT))?;
        if entry.uid == u32::MAX || entry.gid == u32::MAX {
            // setresuid/setresgid reserve this value for "leave unchanged".
            return Err(error(EINVAL));
        }
        set_account_path(&mut self.home, entry.home, WorkerStage::HomeDirectory)?;
        set_account_path(&mut self.shell, entry.shell, WorkerStage::LoginShell)?;
        self.username.set(user).map_err(error)?;
        self.uid = entry.uid;
        self.gid = entry.gid;
        self.groups[0] = entry.gid;
        self.group_count = 1;
        Ok(())
    }

    fn read_groups(&mut self, content: &[u8]) -> Result<(), WorkerFailure> {
        let error = |errno| WorkerFailure::new(WorkerStage::GroupDatabase, errno);
        for entry in group_entries(content) {
            let entry = entry.map_err(|_| error(EINVAL))?;
            if !entry.contains(self.username.as_str()) {
                continue;
            }
            if entry.gid == u32::MAX {
                return Err(error(EINVAL));
            }
            if self.groups().contains(&entry.gid) {
                continue;
            }
            let next = self.groups.get_mut(self.group_count).ok_or(error(E2BIG))?;
            *next = entry.gid;
            self.group_count += 1;
        }
        self.groups[..self.group_count].sort_unstable();
        Ok(())
    }

    pub(super) fn groups(&self) -> &[u32] {
        &self.groups[..self.group_count]
    }

    pub(super) fn enter(&self, interactive: bool, login: bool) -> Result<(), WorkerFailure> {
        if interactive {
            credentials::own_terminal(self.uid, self.gid)
                .map_err(|errno| WorkerFailure::new(WorkerStage::TerminalOwnership, errno))?;
        }
        credentials::switch(self)?;
        files::enter_home(self.home.as_c_str())
            .map_err(|errno| WorkerFailure::new(WorkerStage::HomeDirectory, errno))?;
        if login {
            let basename = self.shell.as_str().rsplit('/').next().unwrap_or("");
            if matches!(basename, "nologin" | "false") {
                return Err(WorkerFailure::new(WorkerStage::LoginDisabled, 13));
            }
            files::check_executable(self.shell.as_c_str())
                .map_err(|errno| WorkerFailure::new(WorkerStage::LoginShell, errno))?;
        }
        Ok(())
    }
}

fn set_account_path(
    target: &mut CString<{ MAX_PATH_BYTES + 1 }>,
    value: &str,
    stage: WorkerStage,
) -> Result<(), WorkerFailure> {
    if !value.starts_with('/') || value.chars().any(char::is_control) {
        return Err(WorkerFailure::new(stage, EINVAL));
    }
    target
        .set(value)
        .map_err(|errno| WorkerFailure::new(stage, errno))
}

pub(super) struct CString<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> CString<N> {
    const fn empty() -> Self {
        Self {
            bytes: [0; N],
            length: 0,
        }
    }

    fn set(&mut self, value: &str) -> Result<(), i32> {
        if value.len() >= N || value.as_bytes().contains(&0) {
            return Err(EINVAL);
        }
        self.bytes[..value.len()].copy_from_slice(value.as_bytes());
        self.bytes[value.len()] = 0;
        self.length = value.len();
        Ok(())
    }

    pub(super) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap()
    }

    pub(super) fn as_c_str(&self) -> &CStr {
        CStr::from_bytes_with_nul(&self.bytes[..=self.length]).unwrap()
    }
}

#[cfg(test)]
mod tests;

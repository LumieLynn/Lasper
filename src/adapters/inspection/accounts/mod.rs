//! Read local accounts without executing anything from the guest filesystem.

use crate::adapters::storage::rootfs::{RootfsAccess, RootfsTarget};
use crate::domain::machine::accounts::{LocalAccount, LocalAccountObservation};
use lasper_local_accounts::passwd_entries;
use std::io::{self, Read};
use std::path::Path;

// Keep both the read and the serialized deployment result below the IPC frame bound.
const MAX_PASSWD_BYTES: usize = 64 * 1024;
const MAX_ACCOUNT_ENTRIES: usize = 512;

pub(crate) async fn local_accounts(target: &RootfsTarget) -> LocalAccountObservation {
    let path = target.path();
    match tokio::task::spawn_blocking(move || inspect(&path)).await {
        Ok(observation) => observation,
        Err(error) => {
            LocalAccountObservation::Unavailable(format!("account inspection worker: {error}"))
        }
    }
}

fn inspect(root: &Path) -> LocalAccountObservation {
    match read(root) {
        Ok(observation) => observation,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            LocalAccountObservation::Invalid(error.to_string())
        }
        Err(error) => LocalAccountObservation::Unavailable(error.to_string()),
    }
}

fn read(root: &Path) -> io::Result<LocalAccountObservation> {
    let access = RootfsAccess::open(root)?;
    let Some(file) = access.regular_file("etc/passwd")? else {
        return Ok(LocalAccountObservation::Missing);
    };
    let mut content = Vec::new();
    file.take((MAX_PASSWD_BYTES + 1) as u64)
        .read_to_end(&mut content)?;
    if content.len() > MAX_PASSWD_BYTES {
        return Ok(LocalAccountObservation::Invalid(format!(
            "/etc/passwd exceeds the {MAX_PASSWD_BYTES}-byte inspection limit"
        )));
    }
    Ok(parse(&content))
}

fn parse(content: &[u8]) -> LocalAccountObservation {
    let mut accounts = Vec::new();
    let mut rejected_lines = Vec::new();
    for entry in passwd_entries(content) {
        match entry {
            Ok(account) => accounts.push(LocalAccount {
                username: account.username.into(),
                uid: account.uid,
                gid: account.gid,
                home: account.home.into(),
                shell: account.shell.into(),
            }),
            Err(error) => rejected_lines.push(error.line),
        }
        if accounts.len() + rejected_lines.len() > MAX_ACCOUNT_ENTRIES {
            return LocalAccountObservation::Invalid(format!(
                "/etc/passwd exceeds the {MAX_ACCOUNT_ENTRIES}-entry inspection limit"
            ));
        }
    }
    LocalAccountObservation::Read {
        accounts,
        rejected_lines,
    }
}

#[cfg(test)]
mod tests;

//! Read local accounts without executing anything from the guest filesystem.

use crate::adapters::storage::rootfs::access::RootfsAccess;
use crate::adapters::storage::rootfs::RootfsTarget;
use crate::domain::machine::accounts::{LocalAccount, LocalAccountObservation};
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
    for (index, line) in content.split(|byte| *byte == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        match std::str::from_utf8(line).ok().and_then(parse_entry) {
            Some(account) => accounts.push(account),
            None => rejected_lines.push(index + 1),
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

fn parse_entry(line: &str) -> Option<LocalAccount> {
    let mut fields = line.split(':');
    let username = fields.next()?;
    let _password = fields.next()?;
    let uid = fields.next()?;
    let gid = fields.next()?;
    let _gecos = fields.next()?;
    let home = fields.next()?;
    let shell = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    // Observation must not impose the stricter rules Lasper uses to create users.
    if username.is_empty()
        || [username, home, shell]
            .iter()
            .any(|value| value.chars().any(char::is_control))
    {
        return None;
    }
    if ![uid, gid]
        .iter()
        .all(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    Some(LocalAccount {
        username: username.into(),
        uid: uid.parse().ok()?,
        gid: gid.parse().ok()?,
        home: home.into(),
        shell: shell.into(),
    })
}

#[cfg(test)]
mod tests;

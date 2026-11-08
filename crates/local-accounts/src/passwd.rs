use crate::{has_controls, numeric_id, records, InvalidRecord};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PasswdEntry<'a> {
    pub username: &'a str,
    pub uid: u32,
    pub gid: u32,
    pub home: &'a str,
    pub shell: &'a str,
}

/// Observation accepts existing account names and empty home/shell fields.
/// A caller intending to execute must apply its account and path requirements.
pub fn passwd_entries(
    content: &[u8],
) -> impl Iterator<Item = Result<PasswdEntry<'_>, InvalidRecord>> {
    records(content).map(|(line, bytes)| {
        core::str::from_utf8(bytes)
            .ok()
            .and_then(parse_entry)
            .ok_or(InvalidRecord { line })
    })
}

fn parse_entry(line: &str) -> Option<PasswdEntry<'_>> {
    let mut fields = line.split(':');
    let username = fields.next()?;
    let _password = fields.next()?;
    let uid = fields.next()?;
    let gid = fields.next()?;
    let _gecos = fields.next()?;
    let home = fields.next()?;
    let shell = fields.next()?;
    if fields.next().is_some()
        || username.is_empty()
        || [username, home, shell].into_iter().any(has_controls)
    {
        return None;
    }
    Some(PasswdEntry {
        username,
        uid: numeric_id(uid)?,
        gid: numeric_id(gid)?,
        home,
        shell,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_preserve_existing_names_and_line_diagnostics() {
        let mut entries = passwd_entries(b"# comment\n\nalice@example:x:42:18::/home/a:/bin/sh\r\nbad:x:no:18::/:/bin/sh\nsvc:x:1:2:::\n");
        let account = entries.next().unwrap().unwrap();
        assert_eq!(account.username, "alice@example");
        assert_eq!((account.uid, account.gid), (42, 18));
        assert_eq!(account.shell, "/bin/sh");
        assert_eq!(entries.next(), Some(Err(InvalidRecord { line: 4 })));
        assert_eq!(entries.next().unwrap().unwrap().shell, "");
        assert_eq!(entries.next(), None);
    }

    #[test]
    fn malformed_fields_are_not_partial_accounts() {
        for content in [
            &b"bad:x:+1:0::/:/bin/sh"[..],
            b"bad:x:4294967296:0::/:/bin/sh",
            b"bad:x:1:2::/:/bin/\x1bsh",
            b"bad:x:1:2::/:/bin/sh:extra",
            b"bad:\xff:1:2::/:/bin/sh",
        ] {
            assert_eq!(
                passwd_entries(content).next(),
                Some(Err(InvalidRecord { line: 1 }))
            );
        }
    }
}

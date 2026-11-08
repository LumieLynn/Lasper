use crate::{has_controls, numeric_id, records, InvalidRecord};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupEntry<'a> {
    pub name: &'a str,
    pub gid: u32,
    members: &'a str,
}

impl GroupEntry<'_> {
    pub fn contains(&self, username: &str) -> bool {
        !username.is_empty() && self.members.split(',').any(|member| member == username)
    }
}

pub fn group_entries(
    content: &[u8],
) -> impl Iterator<Item = Result<GroupEntry<'_>, InvalidRecord>> {
    records(content).map(|(line, bytes)| {
        core::str::from_utf8(bytes)
            .ok()
            .and_then(parse_entry)
            .ok_or(InvalidRecord { line })
    })
}

fn parse_entry(line: &str) -> Option<GroupEntry<'_>> {
    let mut fields = line.split(':');
    let name = fields.next()?;
    let _password = fields.next()?;
    let gid = fields.next()?;
    let members = fields.next()?;
    if fields.next().is_some() || name.is_empty() || has_controls(name) || has_controls(members) {
        return None;
    }
    if !members.is_empty() && members.split(',').any(str::is_empty) {
        return None;
    }
    Some(GroupEntry {
        name,
        gid: numeric_id(gid)?,
        members,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memberships_match_whole_names_and_allow_empty_groups() {
        let entry = group_entries(b"video:x:44:alice,alice@example\n")
            .next()
            .unwrap()
            .unwrap();
        assert!(entry.contains("alice"));
        assert!(entry.contains("alice@example"));
        assert!(!entry.contains("ali"));
        assert!(!group_entries(b"empty:x:3:\n")
            .next()
            .unwrap()
            .unwrap()
            .contains(""));
    }

    #[test]
    fn incomplete_group_data_is_rejected() {
        for content in [
            &b"g:x:+1:alice"[..],
            b"g:x:1:alice,",
            b"g:x:1:,alice",
            b"g:x:1:alice\0",
            b"g:x:1:a:extra",
        ] {
            assert!(group_entries(content).next().unwrap().is_err());
        }
    }
}

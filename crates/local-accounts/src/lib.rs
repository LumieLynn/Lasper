#![no_std]

//! Local passwd/group records, without file access, NSS or credential changes.

mod group;
mod passwd;

pub use group::{group_entries, GroupEntry};
pub use passwd::{passwd_entries, PasswdEntry};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidRecord {
    pub line: usize,
}

fn records(content: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    content
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            (!line.is_empty() && !line.starts_with(b"#")).then_some((index + 1, line))
        })
}

fn numeric_id(value: &str) -> Option<u32> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn has_controls(value: &str) -> bool {
    value.chars().any(char::is_control)
}

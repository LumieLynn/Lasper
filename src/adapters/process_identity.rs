//! Bounded procfs process-generation observations.

use std::fs::File;
use std::io::{self, Read};
use std::num::NonZeroU64;
use std::path::Path;

const MAX_STAT_BYTES: u64 = 8 * 1024;

pub(crate) fn process_start_time(process: &Path) -> io::Result<NonZeroU64> {
    let mut bytes = Vec::new();
    File::open(process.join("stat"))?
        .take(MAX_STAT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STAT_BYTES {
        return Err(invalid_stat("process stat exceeded its size limit"));
    }
    parse_start_time(&bytes)
}

fn parse_start_time(stat: &[u8]) -> io::Result<NonZeroU64> {
    // comm may contain spaces and parentheses; field 22 starts after its final ')'.
    let end = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(|| invalid_stat("process stat has no command terminator"))?;
    let fields = std::str::from_utf8(&stat[end + 1..])
        .map_err(|_| invalid_stat("process stat has invalid numeric fields"))?;
    let value = fields
        .split_ascii_whitespace()
        .nth(19)
        .ok_or_else(|| invalid_stat("process stat has no start time"))?
        .parse::<u64>()
        .map_err(|_| invalid_stat("process stat has an invalid start time"))?;
    NonZeroU64::new(value).ok_or_else(|| invalid_stat("process start time is zero"))
}

fn invalid_stat(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_time_handles_parentheses_and_spaces_in_process_names() {
        let fields = std::iter::repeat_n("0", 18).collect::<Vec<_>>().join(" ");
        let stat = format!("42 (a process ) name) S {fields} 123 0\n");
        assert_eq!(parse_start_time(stat.as_bytes()).unwrap().get(), 123);
        assert!(parse_start_time(b"42 (process) S").is_err());
        assert!(parse_start_time(stat.replace("123", "0").as_bytes()).is_err());
        let mut non_utf8_name = stat.into_bytes();
        non_utf8_name[5] = 0xff;
        assert_eq!(parse_start_time(&non_utf8_name).unwrap().get(), 123);
    }

    #[test]
    fn stat_reads_are_bounded() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("stat"),
            vec![b' '; MAX_STAT_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(
            process_start_time(directory.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(process_start_time(Path::new("/proc/self")).is_ok());
    }
}

//! Resource-budget validation for uncompressed tar archives.

use crate::adapters::error::{NspawnError, Result};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

pub(super) const MAX_EXPANDED_BYTES: u64 = crate::domain::storage::MAX_DISK_IMAGE_SIZE_BYTES;
const MAX_ENTRIES: u64 = 1_000_000;
const MAX_METADATA_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default)]
struct PaxSizeHints {
    stream_size: Option<u64>,
    logical_size: Option<u64>,
}

impl PaxSizeHints {
    fn overlay(self, overrides: Self) -> Self {
        Self {
            stream_size: overrides.stream_size.or(self.stream_size),
            logical_size: overrides.logical_size.or(self.logical_size),
        }
    }
}

pub(super) fn validate(source: &std::fs::File) -> Result<()> {
    validate_with_limit(source, MAX_EXPANDED_BYTES, MAX_ENTRIES)
}

fn validate_with_limit(source: &std::fs::File, limit: u64, max_entries: u64) -> Result<()> {
    let mut header = [0u8; 512];
    let mut offset = 0u64;
    let mut zero_blocks = 0u8;
    let mut entries = 0u64;
    let mut payload_bytes = 0u64;
    let mut global_pax = PaxSizeHints::default();
    let mut local_pax = PaxSizeHints::default();
    loop {
        read_block(source, offset, &mut header)?;
        offset = checked_offset(offset, header.len() as u64)?;

        if header.iter().all(|byte| *byte == 0) {
            zero_blocks += 1;
            if zero_blocks == 2 {
                return Ok(());
            }
            continue;
        }
        zero_blocks = 0;

        entries = entries
            .checked_add(1)
            .ok_or_else(|| NspawnError::Validation("Tar archive entry count overflowed".into()))?;
        if entries > max_entries {
            return Err(NspawnError::Validation(format!(
                "Tar archive contains more than {max_entries} entries"
            )));
        }

        let header_size = parse_tar_size(&header[124..136])?;
        let entry_type = header[156];
        if matches!(entry_type, b'x' | b'g') {
            reject_large_metadata(header_size, "PAX")?;
            let metadata = read_bytes(source, offset, header_size as usize)?;
            let hints = parse_pax_size_hints(&metadata)?;
            if entry_type == b'g' {
                global_pax = global_pax.overlay(hints);
            } else {
                local_pax = local_pax.overlay(hints);
            }
            offset = checked_offset(offset, padded_size(header_size)?)?;
            continue;
        }

        if matches!(entry_type, b'L' | b'K') {
            reject_large_metadata(header_size, "long-name")?;
            offset = checked_offset(offset, padded_size(header_size)?)?;
            continue;
        }

        let hints = global_pax.overlay(std::mem::take(&mut local_pax));
        let stream_size = hints.stream_size.unwrap_or(header_size);
        let mut logical_size = hints.logical_size.unwrap_or(stream_size).max(stream_size);
        if entry_type == b'S' {
            logical_size = logical_size.max(parse_tar_size(&header[483..495])?);
            offset = skip_oldgnu_sparse_headers(source, offset, header[482] != 0, max_entries)?;
        }

        add_payload(&mut payload_bytes, logical_size, limit)?;
        offset = checked_offset(offset, padded_size(stream_size)?)?;
    }
}

fn reject_large_metadata(size: u64, kind: &str) -> Result<()> {
    if size > MAX_METADATA_BYTES {
        return Err(NspawnError::Validation(format!(
            "Tar {kind} metadata exceeds the {MAX_METADATA_BYTES} byte limit"
        )));
    }
    Ok(())
}

fn skip_oldgnu_sparse_headers(
    source: &std::fs::File,
    mut offset: u64,
    mut extended: bool,
    limit: u64,
) -> Result<u64> {
    let mut count = 0u64;
    while extended {
        count = count
            .checked_add(1)
            .ok_or_else(|| NspawnError::Validation("Tar sparse header count overflowed".into()))?;
        if count > limit {
            return Err(NspawnError::Validation(format!(
                "Tar archive contains more than {limit} sparse extension headers"
            )));
        }
        let mut header = [0u8; 512];
        read_block(source, offset, &mut header)?;
        offset = checked_offset(offset, header.len() as u64)?;
        extended = header[504] != 0;
    }
    Ok(offset)
}

fn add_payload(total: &mut u64, size: u64, limit: u64) -> Result<()> {
    *total = total
        .checked_add(size)
        .ok_or_else(|| NspawnError::Validation("Tar archive payload size overflowed".into()))?;
    if *total > limit {
        return Err(NspawnError::Validation(format!(
            "Tar archive declares more than the {limit} byte extraction limit"
        )));
    }
    Ok(())
}

fn padded_size(size: u64) -> Result<u64> {
    size.checked_add(511)
        .map(|value| value / 512 * 512)
        .ok_or_else(|| NspawnError::Validation("Tar archive entry size overflowed".into()))
}

fn checked_offset(offset: u64, addition: u64) -> Result<u64> {
    offset
        .checked_add(addition)
        .ok_or_else(|| NspawnError::Validation("Tar archive offset overflowed".into()))
}

fn read_block(source: &std::fs::File, offset: u64, block: &mut [u8]) -> Result<()> {
    let mut filled = 0usize;
    while filled < block.len() {
        let position = checked_offset(offset, filled as u64)?;
        let count = source
            .read_at(&mut block[filled..], position)
            .map_err(|error| NspawnError::Io(PathBuf::from("tar archive"), error))?;
        if count == 0 {
            return Err(NspawnError::Validation(
                "Tar archive ended before its end-of-archive blocks".into(),
            ));
        }
        filled += count;
    }
    Ok(())
}

fn read_bytes(source: &std::fs::File, offset: u64, size: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; size];
    read_block(source, offset, &mut bytes)?;
    Ok(bytes)
}

fn parse_pax_size_hints(payload: &[u8]) -> Result<PaxSizeHints> {
    let mut hints = PaxSizeHints::default();
    let mut offset = 0usize;
    while offset < payload.len() {
        let length_end = payload[offset..]
            .iter()
            .position(|byte| *byte == b' ')
            .map(|position| offset + position)
            .ok_or_else(|| NspawnError::Validation("Tar PAX record has no length".into()))?;
        let length = usize::try_from(parse_decimal_size(&payload[offset..length_end])?)
            .map_err(|_| NspawnError::Validation("Tar PAX record is too large".into()))?;
        let record_end = offset
            .checked_add(length)
            .filter(|end| *end <= payload.len())
            .ok_or_else(|| NspawnError::Validation("Tar PAX record is truncated".into()))?;
        if length_end + 1 >= record_end || payload[record_end - 1] != b'\n' {
            return Err(NspawnError::Validation(
                "Tar PAX record has an invalid length".into(),
            ));
        }
        let record = &payload[length_end + 1..record_end - 1];
        let separator = record
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(|| NspawnError::Validation("Tar PAX record has no value".into()))?;
        let value = &record[separator + 1..];
        match &record[..separator] {
            b"size" => hints.stream_size = Some(parse_decimal_size(value)?),
            b"GNU.sparse.realsize" | b"GNU.sparse.size" => {
                hints.logical_size = Some(parse_decimal_size(value)?);
            }
            _ => {}
        }
        offset = record_end;
    }
    Ok(hints)
}

fn parse_decimal_size(value: &[u8]) -> Result<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(NspawnError::Validation(
            "Tar metadata contains an invalid decimal size".into(),
        ));
    }
    value.iter().try_fold(0u64, |total, byte| {
        total
            .checked_mul(10)
            .and_then(|total| total.checked_add((byte - b'0') as u64))
            .ok_or_else(|| NspawnError::Validation("Tar metadata size overflowed".into()))
    })
}

fn parse_tar_size(field: &[u8]) -> Result<u64> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        if field[0] & 0x40 != 0 {
            return Err(NspawnError::Validation(
                "Tar archive contains a negative entry size".into(),
            ));
        }
        return field
            .iter()
            .copied()
            .enumerate()
            .try_fold(0u64, |value, (index, byte)| {
                let byte = if index == 0 { byte & 0x7f } else { byte };
                value
                    .checked_mul(256)
                    .and_then(|value| value.checked_add(byte as u64))
                    .ok_or_else(|| NspawnError::Validation("Tar entry size overflowed".into()))
            });
    }

    let Some(start) = field.iter().position(|byte| !matches!(byte, 0 | b' ')) else {
        return Ok(0);
    };
    let end = field
        .iter()
        .rposition(|byte| !matches!(byte, 0 | b' '))
        .expect("non-empty tar size field")
        + 1;
    field[start..end].iter().try_fold(0u64, |value, byte| {
        if !matches!(byte, b'0'..=b'7') {
            return Err(NspawnError::Validation(
                "Tar archive contains an invalid entry size".into(),
            ));
        }
        value
            .checked_mul(8)
            .and_then(|value| value.checked_add((byte - b'0') as u64))
            .ok_or_else(|| NspawnError::Validation("Tar entry size overflowed".into()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    fn header(size: u64, entry_type: u8) -> [u8; 512] {
        let mut header = [0u8; 512];
        header[0] = b'x';
        write_octal_field(&mut header[124..136], size);
        header[156] = entry_type;
        header
    }

    fn write_octal_field(field: &mut [u8], value: u64) {
        let encoded = format!("{value:0width$o}\0", width = field.len() - 1);
        assert_eq!(encoded.len(), field.len());
        field.copy_from_slice(encoded.as_bytes());
    }

    fn pax_record(key: &str, value: &str) -> Vec<u8> {
        let body = format!(" {key}={value}\n");
        let mut length = body.len() + 1;
        loop {
            let record = format!("{length}{body}");
            if record.len() == length {
                return record.into_bytes();
            }
            length = record.len();
        }
    }

    fn archive(bytes: &[u8]) -> std::fs::File {
        let mut archive = tempfile::tempfile().unwrap();
        archive.write_all(bytes).unwrap();
        archive.seek(SeekFrom::Start(0)).unwrap();
        archive
    }

    fn regular_archive(payload: &[u8]) -> std::fs::File {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header(payload.len() as u64, b'0'));
        bytes.extend_from_slice(payload);
        bytes.resize(512 + padded_size(payload.len() as u64).unwrap() as usize, 0);
        bytes.extend_from_slice(&[0u8; 1024]);
        archive(&bytes)
    }

    #[test]
    fn scan_preserves_fd_offset_and_accepts_valid_archive() {
        let mut archive = regular_archive(b"typed payload");
        archive.seek(SeekFrom::Start(37)).unwrap();

        validate_with_limit(&archive, 1024, 10).unwrap();

        assert_eq!(archive.stream_position().unwrap(), 37);
    }

    #[test]
    fn scan_limits_declared_bytes_and_entry_count() {
        let archive = regular_archive(b"payload larger than the limit");

        assert!(validate_with_limit(&archive, 4, 10)
            .unwrap_err()
            .to_string()
            .contains("extraction limit"));
        assert!(validate_with_limit(&archive, 1024, 0)
            .unwrap_err()
            .to_string()
            .contains("more than 0 entries"));
    }

    #[test]
    fn size_parser_accepts_octal_and_positive_base256_only() {
        assert_eq!(parse_tar_size(b"00000000017\0").unwrap(), 15);

        let mut base256 = [0u8; 12];
        base256[0] = 0x80;
        base256[11] = 16;
        assert_eq!(parse_tar_size(&base256).unwrap(), 16);

        assert!(parse_tar_size(b"0000000008\0\0").is_err());
        base256[0] = 0xc0;
        assert!(parse_tar_size(&base256).is_err());
    }

    #[test]
    fn scan_counts_pax_and_oldgnu_sparse_logical_sizes() {
        let pax = pax_record("GNU.sparse.realsize", "4096");
        let mut pax_bytes = Vec::new();
        pax_bytes.extend_from_slice(&header(pax.len() as u64, b'x'));
        pax_bytes.extend_from_slice(&pax);
        pax_bytes.resize(512 + padded_size(pax.len() as u64).unwrap() as usize, 0);
        pax_bytes.extend_from_slice(&header(0, b'0'));
        pax_bytes.extend_from_slice(&[0u8; 1024]);
        assert!(validate_with_limit(&archive(&pax_bytes), 1024, 10).is_err());

        let mut sparse_header = header(0, b'S');
        write_octal_field(&mut sparse_header[483..495], 4096);
        let mut oldgnu_bytes = Vec::from(sparse_header);
        oldgnu_bytes.extend_from_slice(&[0u8; 1024]);
        assert!(validate_with_limit(&archive(&oldgnu_bytes), 1024, 10).is_err());
    }

    #[test]
    fn scan_uses_pax_stream_size_for_alignment() {
        let pax = pax_record("size", "5");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header(pax.len() as u64, b'x'));
        bytes.extend_from_slice(&pax);
        bytes.resize(512 + padded_size(pax.len() as u64).unwrap() as usize, 0);
        bytes.extend_from_slice(&header(0, b'0'));
        bytes.extend_from_slice(b"12345");
        bytes.resize(bytes.len().div_ceil(512) * 512, 0);
        bytes.extend_from_slice(&[0u8; 1024]);

        validate_with_limit(&archive(&bytes), 5, 10).unwrap();
    }
}

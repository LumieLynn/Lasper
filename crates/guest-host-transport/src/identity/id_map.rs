use std::io::{self, Read};

const MAX_ID_MAP_BYTES: u64 = 32 * 1024;
const MAX_ID_MAP_EXTENTS: usize = 340;

/// Numeric ranges from a procfs uid_map or gid_map. The opener determines
/// which namespace the second column describes; parsing alone cannot do that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct IdMap {
    extents: Vec<Extent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extent {
    inside: u64,
    outside: u64,
    length: u64,
}

impl IdMap {
    pub(super) fn read(reader: impl Read) -> io::Result<Self> {
        let mut bytes = Vec::new();
        reader.take(MAX_ID_MAP_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_ID_MAP_BYTES {
            return Err(invalid_data("identity map exceeded its size limit"));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| invalid_data("identity map is not valid UTF-8"))?;
        Self::parse(text)
    }

    pub(super) fn parse(text: &str) -> io::Result<Self> {
        if text.len() as u64 > MAX_ID_MAP_BYTES {
            return Err(invalid_data("identity map exceeded its size limit"));
        }
        let mut extents = Vec::new();
        for line in text.lines() {
            if extents.len() >= MAX_ID_MAP_EXTENTS {
                return Err(invalid_data("identity map has too many extents"));
            }
            let mut fields = line.split_ascii_whitespace();
            let mut next = || {
                fields
                    .next()
                    .ok_or_else(|| invalid_data("identity map row does not have three fields"))?
                    .parse::<u64>()
                    .map_err(|_| invalid_data("identity map contains a non-numeric field"))
            };
            let inside = next()?;
            let outside = next()?;
            let length = next()?;
            if fields.next().is_some() {
                return Err(invalid_data("identity map row does not have three fields"));
            }
            if length == 0
                || inside
                    .checked_add(length)
                    .is_none_or(|end| end > 1_u64 << 32)
                || outside > u64::from(u32::MAX)
                || (outside != u64::from(u32::MAX)
                    && outside
                        .checked_add(length)
                        .is_none_or(|end| end > 1_u64 << 32))
            {
                return Err(invalid_data(
                    "identity map row is outside the UID/GID range",
                ));
            }
            extents.push(Extent {
                inside,
                outside,
                length,
            });
        }
        if extents.is_empty() {
            return Err(invalid_data("identity map is empty"));
        }
        Ok(Self { extents })
    }

    /// Returns no value for an unmapped ID, the reserved -1 ID, or an
    /// ambiguous range. Procfs prints -1 in column two for an invisible range.
    pub(super) fn map(&self, id: u32) -> Option<u32> {
        if id == u32::MAX {
            return None;
        }
        let id = u64::from(id);
        let mut mapped = None;
        for extent in &self.extents {
            if id < extent.inside || id >= extent.inside + extent.length {
                continue;
            }
            if extent.outside == u64::from(u32::MAX) {
                return None;
            }
            let value = u32::try_from(extent.outside + (id - extent.inside)).ok()?;
            if value == u32::MAX || mapped.replace(value).is_some() {
                return None;
            }
        }
        mapped
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_multiple_extents_without_assuming_one_shift() {
        let map = IdMap::parse("0 1437401088 65536\n70000 200000 100\n").unwrap();
        assert_eq!(map.map(1000), Some(1_437_402_088));
        assert_eq!(map.map(70_042), Some(200_042));
        assert_eq!(map.map(65_536), None);
    }

    #[test]
    fn invisible_and_ambiguous_extents_cannot_authorize_an_id() {
        let map = IdMap::parse("0 4294967295 65536\n70000 200000 100\n").unwrap();
        assert_eq!(map.map(1000), None);
        assert_eq!(map.map(70_042), Some(200_042));
        assert_eq!(IdMap::parse("0 1000 10\n5 2000 10\n").unwrap().map(7), None);
        assert_eq!(
            IdMap::parse("0 0 4294967295\n").unwrap().map(u32::MAX),
            None
        );
    }

    #[test]
    fn rejects_invalid_and_unbounded_input() {
        for text in [
            "",
            "0 1000",
            "0 uid 1",
            "0 1000 0",
            "0 1000 1 2",
            "4294967295 0 2",
            "0 4294967294 3",
            "0 4294967296 1",
        ] {
            assert!(IdMap::parse(text).is_err(), "{text}");
        }
        let excessive = std::iter::repeat_n("0 0 1", MAX_ID_MAP_EXTENTS + 1)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(IdMap::parse(&excessive).is_err());
        assert!(IdMap::read([0xff].as_slice()).is_err());
        let oversized = vec![b' '; MAX_ID_MAP_BYTES as usize + 1];
        assert!(IdMap::parse(std::str::from_utf8(&oversized).unwrap()).is_err());
        assert!(IdMap::read(oversized.as_slice()).is_err());
    }
}

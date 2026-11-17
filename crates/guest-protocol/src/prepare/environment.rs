use super::*;
use crate::json::StringFragments;
use core::{cmp::Ordering, str::Chars};

pub const MAX_PROCESS_ENVIRONMENT_ENTRIES: usize = crate::BUILD_CAPACITIES.environment_entries;
pub const MAX_PROCESS_ENVIRONMENT_BYTES: usize = crate::BUILD_CAPACITIES.environment_bytes;
pub const MAX_DEFAULT_PROCESS_ENVIRONMENT_BYTES: usize =
    crate::BUILD_CAPACITIES.default_environment_bytes;
pub const MAX_PREPARE_UNESCAPE_BYTES: usize = crate::BUILD_CAPACITIES.prepare_unescape_bytes();

#[derive(Clone, Debug, Serialize)]
pub struct EnvironmentEntry<'a> {
    name: CommandArgument<'a>,
    value: CommandArgument<'a>,
}
impl<'a> EnvironmentEntry<'a> {
    pub fn new(name: &'a str, value: &'a str) -> Result<Self, PayloadCodecError> {
        let entry = Self {
            name: CommandArgument::Plain(name),
            value: CommandArgument::Plain(value),
        };
        entry.validate()?;
        Ok(entry)
    }
    fn validate(&self) -> Result<usize, PayloadCodecError> {
        let mut count: usize = 0;
        for (index, ch) in DecodedChars::new(self.name).enumerate() {
            let ch = ch?;
            if !(ch == '_' || ch.is_ascii_alphabetic() || (index > 0 && ch.is_ascii_digit())) {
                return Err(PayloadCodecError::InvalidRequest);
            }
            count += 1;
        }
        if count == 0 {
            return Err(PayloadCodecError::InvalidRequest);
        }
        count
            .checked_add(self.value.decoded_len()?)
            .and_then(|n| n.checked_add(2))
            .ok_or(PayloadCodecError::InvalidRequest)
    }
    pub fn decode_into<'b>(
        &self,
        storage: &'b mut [u8],
    ) -> Result<(&'b str, &'b str), PayloadCodecError> {
        let n = self.name.write_decoded(storage)?;
        let (name, remaining) = storage.split_at_mut(n);
        let v = self.value.write_decoded(remaining)?;
        Ok((
            core::str::from_utf8(name).map_err(|_| PayloadCodecError::InvalidRequest)?,
            core::str::from_utf8(&remaining[..v]).map_err(|_| PayloadCodecError::InvalidRequest)?,
        ))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEntry<'a> {
    #[serde(borrow)]
    name: EscapedStr<'a>,
    #[serde(borrow)]
    value: EscapedStr<'a>,
}
impl<'de: 'a, 'a> Deserialize<'de> for EnvironmentEntry<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = WireEntry::deserialize(deserializer)?;
        let entry = Self {
            name: CommandArgument::Escaped(wire.name),
            value: CommandArgument::Escaped(wire.value),
        };
        entry
            .validate()
            .map_err(|_| D::Error::custom("invalid environment entry"))?;
        Ok(entry)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(transparent)]
pub struct ProcessEnvironment<'a>(Vec<EnvironmentEntry<'a>, MAX_PROCESS_ENVIRONMENT_ENTRIES>);
impl<'a> ProcessEnvironment<'a> {
    pub(super) const fn empty() -> Self {
        Self(Vec::new())
    }

    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    pub(super) fn move_into(&mut self, output: &mut Self) {
        output.0.clear();
        // Move entries in place rather than copying the fixed-capacity array
        // through the guest's stack. Both lists have the same capacity.
        while let Some(entry) = self.0.pop() {
            output.0.push(entry).unwrap_or_else(|_| unreachable!());
        }
        output.0.reverse();
    }

    /// Orders caller-supplied entries for encoding and rejects duplicate names.
    /// Wire decoding validates the received order without changing it.
    pub fn new(
        entries: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, PayloadCodecError> {
        let mut result = Self::default();
        for (name, value) in entries {
            result
                .0
                .push(EnvironmentEntry::new(name, value)?)
                .map_err(|_| PayloadCodecError::InvalidRequest)?;
        }
        result.0.sort_unstable_by_key(|entry| match entry.name {
            CommandArgument::Plain(name) => name,
            CommandArgument::Escaped(_) => unreachable!("new entries contain decoded strings"),
        });
        result.validate(&crate::profile::BUILD_CAPACITIES)?;
        Ok(result)
    }
    pub fn entries(&self) -> &[EnvironmentEntry<'a>] {
        &self.0
    }
    pub fn validate(&self, capacities: &crate::ProfileCapacities) -> Result<(), PayloadCodecError> {
        if self.0.len() > capacities.environment_entries {
            return Err(PayloadCodecError::InvalidRequest);
        }
        let mut previous: Option<&EnvironmentEntry<'_>> = None;
        let mut total = 0usize;
        for entry in &self.0 {
            total = validate_next(previous, entry, total, capacities)?;
            previous = Some(entry);
        }
        Ok(())
    }
}

fn validate_next(
    previous: Option<&EnvironmentEntry<'_>>,
    entry: &EnvironmentEntry<'_>,
    total: usize,
    capacities: &crate::ProfileCapacities,
) -> Result<usize, PayloadCodecError> {
    let total = total
        .checked_add(entry.validate()?)
        .ok_or(PayloadCodecError::InvalidRequest)?;
    if total > capacities.environment_bytes {
        return Err(PayloadCodecError::InvalidRequest);
    }
    if let Some(previous) = previous {
        let mut left = DecodedChars::new(previous.name);
        let mut right = DecodedChars::new(entry.name);
        let ordering = loop {
            match (left.next().transpose()?, right.next().transpose()?) {
                (Some(a), Some(b)) if a == b => continue,
                (Some(a), Some(b)) => break a.cmp(&b),
                (None, Some(_)) => break Ordering::Less,
                (Some(_), None) => break Ordering::Greater,
                (None, None) => break Ordering::Equal,
            }
        };
        if ordering != Ordering::Less {
            return Err(PayloadCodecError::InvalidRequest);
        }
    }
    Ok(total)
}

pub(super) struct EnvironmentSeed<'s, 'a> {
    pub(super) output: &'s mut ProcessEnvironment<'a>,
    pub(super) capacities: &'s crate::ProfileCapacities,
    pub(super) invalid_request: &'s mut bool,
}

impl<'de: 'a, 'a> serde::de::DeserializeSeed<'de> for EnvironmentSeed<'_, 'a> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        decoder.deserialize_seq(self)
    }
}

impl<'de: 'a, 'a> Visitor<'de> for EnvironmentSeed<'_, 'a> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an environment list")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        let mut total = 0;
        while let Some(entry) = sequence.next_element::<EnvironmentEntry<'a>>()? {
            let checked = if self.output.0.len() >= self.capacities.environment_entries {
                Err(PayloadCodecError::InvalidRequest)
            } else {
                validate_next(self.output.0.last(), &entry, total, self.capacities)
            };
            total = checked.map_err(|_| {
                *self.invalid_request = true;
                A::Error::custom("environment must be ordered, unique and within profile capacity")
            })?;
            self.output.0.push(entry).map_err(|_| {
                *self.invalid_request = true;
                A::Error::custom("environment exceeds build storage")
            })?;
        }
        Ok(())
    }
}
struct DecodedChars<'a> {
    plain: Option<Chars<'a>>,
    fragments: Option<StringFragments<'a>>,
    segment: Chars<'a>,
}
impl<'a> DecodedChars<'a> {
    fn new(value: CommandArgument<'a>) -> Self {
        match value {
            CommandArgument::Plain(value) => Self {
                plain: Some(value.chars()),
                fragments: None,
                segment: "".chars(),
            },
            CommandArgument::Escaped(value) => Self {
                plain: None,
                fragments: Some(fragments(value)),
                segment: "".chars(),
            },
        }
    }
}
impl Iterator for DecodedChars<'_> {
    type Item = Result<char, PayloadCodecError>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(plain) = &mut self.plain {
            return plain.next().map(Ok);
        }
        loop {
            if let Some(ch) = self.segment.next() {
                return Some(Ok(ch));
            }
            match self.fragments.as_mut()?.next()? {
                Ok(EscapedStringFragment::NotEscaped(value)) => self.segment = value.chars(),
                Ok(EscapedStringFragment::Escaped(ch)) => return Some(Ok(ch)),
                Err(_) => return Some(Err(PayloadCodecError::InvalidRequest)),
            }
        }
    }
}

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use zeroize::{Zeroize, Zeroizing};

/// Ephemeral secret text which cannot be cloned, debug-printed, or serialized
/// through an ordinary model derive.
pub struct SecretString {
    bytes: Zeroizing<Vec<u8>>,
}

impl SecretString {
    pub fn new(value: String) -> Self {
        Self {
            bytes: Zeroizing::new(value.into_bytes()),
        }
    }

    pub fn expose_secret(&self) -> &str {
        std::str::from_utf8(&self.bytes).expect("SecretString is constructed from valid UTF-8")
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

/// Short-lived serialized secret material. This exists so transport buffers
/// are cleared on every return path, including I/O and size-limit failures.
pub(crate) struct SecretBytes {
    bytes: Zeroizing<Vec<u8>>,
}

impl SecretBytes {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBytes([REDACTED])")
    }
}

pub(crate) fn zeroize_string(value: &mut String) {
    value.zeroize();
}

pub(crate) fn replace_secret_string(value: &mut String, replacement: &str) {
    zeroize_string(value);
    value.push_str(replacement);
}

/// Serde hooks for dedicated secret-bearing wire DTO fields. `SecretString`
/// deliberately does not implement `Serialize` or `Deserialize` itself.
pub(crate) mod serde_secret {
    use super::*;

    pub fn serialize<S>(value: &SecretString, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(value.expose_secret())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SecretString, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(SecretString::new)
    }

    pub mod optional {
        use super::*;

        pub fn serialize<S>(value: &Option<SecretString>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            value
                .as_ref()
                .map(SecretString::expose_secret)
                .serialize(serializer)
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<SecretString>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Option::<String>::deserialize(deserializer).map(|value| value.map(SecretString::new))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_is_redacted() {
        let secret = SecretString::new("do-not-print-me".into());
        let debug = format!("{secret:?}");

        assert_eq!(debug, "SecretString([REDACTED])");
        assert!(!debug.contains("do-not-print-me"));
    }

    #[test]
    fn string_zeroization_clears_the_value() {
        let mut value = "sensitive".to_string();
        zeroize_string(&mut value);

        assert!(value.is_empty());
    }

    #[test]
    fn secret_replacement_drops_the_zeroized_logical_prefix() {
        let mut value = "old-secret".to_string();
        replace_secret_string(&mut value, "NewPassword123");

        assert_eq!(value, "NewPassword123");
        assert!(!value.chars().any(char::is_control));
    }
}

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::sync::atomic::{compiler_fence, Ordering};

/// Ephemeral secret text which cannot be cloned, debug-printed, or serialized
/// through an ordinary model derive.
pub struct SecretString {
    bytes: Vec<u8>,
}

impl SecretString {
    pub fn new(value: String) -> Self {
        Self {
            bytes: value.into_bytes(),
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

impl Drop for SecretString {
    fn drop(&mut self) {
        zeroize_vec_allocation(&mut self.bytes);
    }
}

/// Short-lived serialized secret material. This exists so transport buffers
/// are cleared on every return path, including I/O and size-limit failures.
pub(crate) struct SecretBytes {
    bytes: Vec<u8>,
}

impl SecretBytes {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub(crate) fn push(&mut self, byte: u8) {
        self.bytes.push(byte);
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

impl Drop for SecretBytes {
    fn drop(&mut self) {
        zeroize_vec_allocation(&mut self.bytes);
    }
}

pub(crate) fn zeroize_string(value: &mut String) {
    // SAFETY: replacing every byte with NUL preserves String's UTF-8 invariant.
    unsafe { zeroize_vec_allocation(value.as_mut_vec()) };
}

fn zeroize_vec_allocation(bytes: &mut Vec<u8>) {
    let capacity = bytes.capacity();
    let pointer = bytes.as_mut_ptr();
    for index in 0..capacity {
        // SAFETY: every address up to the Vec's capacity is writable allocated
        // storage, and writing a byte also initializes spare capacity.
        unsafe { std::ptr::write_volatile(pointer.add(index), 0) };
    }
    compiler_fence(Ordering::SeqCst);
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
    fn zeroization_clears_every_owned_byte() {
        let mut bytes = b"sensitive".to_vec();
        zeroize_vec_allocation(&mut bytes);
        assert!(bytes.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn string_zeroization_preserves_valid_utf8() {
        let mut value = "sensitive".to_string();
        zeroize_string(&mut value);

        assert_eq!(value, "\0".repeat(9));
    }
}

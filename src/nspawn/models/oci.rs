use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// A registry reference accepted by systemd's `importctl pull-oci`.
///
/// This intentionally models systemd's OCI reference grammar rather than
/// skopeo transport URLs. Local layouts and archive transports are not OCI
/// registry references and are rejected here.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct OciReference(String);

impl OciReference {
    pub fn new(reference: impl Into<String>) -> Result<Self, OciReferenceError> {
        let reference = reference.into();
        validate_oci_reference(&reference).map_err(|reason| OciReferenceError {
            reference: reference.clone(),
            reason,
        })?;
        Ok(Self(reference))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OciReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for OciReference {
    type Error = OciReferenceError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for OciReference {
    type Error = OciReferenceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for OciReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OciReferenceError {
    reference: String,
    reason: &'static str,
}

impl fmt::Display for OciReferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid OCI reference {:?}: {}",
            self.reference, self.reason
        )
    }
}

impl std::error::Error for OciReferenceError {}

fn validate_oci_reference(reference: &str) -> Result<(), &'static str> {
    if reference.is_empty() || reference.len() > 512 {
        return Err("expected 1-512 characters");
    }
    if reference.trim() != reference || reference.chars().any(char::is_control) {
        return Err("whitespace and control characters are not allowed");
    }
    if reference.starts_with('-') || reference.contains("://") || reference.contains('@') {
        return Err("expected a registry image reference, not a transport URL or digest");
    }

    // Match systemd's current oci_ref_parse(): the final ':' introduces a
    // tag, then the first '/' separates an optional registry from the image.
    let (without_tag, tag) = match reference.rsplit_once(':') {
        Some((image, tag)) => (image, Some(tag)),
        None => (reference, None),
    };
    if let Some(tag) = tag {
        validate_tag(tag)?;
    }

    let (registry, image) = match without_tag.split_once('/') {
        Some((registry, image)) => (Some(registry), image),
        None => (None, without_tag),
    };
    if let Some(registry) = registry {
        validate_registry(registry)?;
    }
    validate_image(image)
}

fn validate_registry(registry: &str) -> Result<(), &'static str> {
    if registry.is_empty() || registry.starts_with('.') || registry.ends_with('.') {
        return Err("registry name is invalid");
    }
    if registry.split('.').any(|label| {
        label.is_empty()
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return Err("registry name is invalid");
    }
    Ok(())
}

fn validate_image(image: &str) -> Result<(), &'static str> {
    if image.is_empty() {
        return Err("image name is empty");
    }
    for component in image.split('/') {
        let mut bytes = component.bytes();
        let Some(first) = bytes.next() else {
            return Err("image path components cannot be empty");
        };
        if !(first.is_ascii_lowercase()
            || first.is_ascii_digit()
            || matches!(first, b'_' | b'-' | b'+'))
        {
            return Err("image names must use systemd's lowercase OCI grammar");
        }
        if !bytes.all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'-' | b'+')
        }) {
            return Err("image names must use systemd's lowercase OCI grammar");
        }
    }
    Ok(())
}

fn validate_tag(tag: &str) -> Result<(), &'static str> {
    let bytes = tag.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return Err("tag must contain 1-128 characters");
    }
    if !(bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-'))
    {
        return Err("tag is not valid according to the OCI distribution grammar");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_systemd_registry_references() {
        for reference in [
            "nginx",
            "library/nginx",
            "docker.io/library/nginx:latest",
            "quay.io/fedora/fedora:44",
        ] {
            assert!(OciReference::new(reference).is_ok(), "{reference}");
        }
    }

    #[test]
    fn rejects_skopeo_transports_and_non_systemd_grammar() {
        for reference in [
            "",
            "docker://ubuntu",
            "oci:/tmp/layout:latest",
            "dir:/tmp/layout",
            "docker.io/Library/Nginx",
            "docker.io/library/nginx@sha256:abc",
            "../escape",
            "--force",
        ] {
            assert!(OciReference::new(reference).is_err(), "{reference}");
        }
    }

    #[test]
    fn deserialization_revalidates_the_reference() {
        assert!(
            serde_json::from_str::<OciReference>(r#""docker.io/library/nginx:latest""#).is_ok()
        );
        assert!(serde_json::from_str::<OciReference>(r#""docker://nginx""#).is_err());
    }
}

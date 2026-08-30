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

    // A colon before the first image slash belongs to the registry port; only
    // a colon in the image portion introduces a tag. This preserves both
    // registry:port/image and registry:port/image:tag.
    let last_slash = reference.rfind('/');
    let tag_separator = match (reference.rfind(':'), last_slash) {
        (Some(colon), Some(slash)) if colon > slash => Some(colon),
        (Some(colon), None) => Some(colon),
        _ => None,
    };
    let (without_tag, tag) = match tag_separator {
        Some(colon) => (&reference[..colon], Some(&reference[colon + 1..])),
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
    let (host, port) = match registry.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (registry, None),
    };
    if host.is_empty() || host.len() > 253 || host.starts_with('.') {
        return Err("registry name is invalid");
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty()
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    {
        return Err("registry name is invalid");
    }
    if let Some(port) = port {
        if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("registry name is invalid");
        }
        match port.parse::<u16>() {
            Ok(port) if port != 0 => {}
            _ => return Err("registry name is invalid"),
        }
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
            "localhost:5000/library/nginx",
            "registry.internal:5000/team/image:dev",
            "registry_name.example/team/image:latest",
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
            "localhost:0/library/nginx",
            "localhost:65536/library/nginx",
            "localhost:not-a-port/library/nginx",
            ":5000/library/nginx",
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

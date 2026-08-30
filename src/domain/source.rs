//! Source values used by provisioning.
//!
//! This module deliberately contains source intent and format detection only.
//! Provider-specific validation and command construction remain outside the
//! domain until their contracts are migrated.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapMethod {
    Debootstrap,
    Pacstrap,
    Dnf5,
    Artifact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactValidationError {
    EmptyPath,
    ControlCharacter,
    FormatMismatch,
}

impl fmt::Display for ArtifactValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyPath => "Artifact path cannot be empty",
            Self::ControlCharacter => "Artifact path contains control characters",
            Self::FormatMismatch => "Artifact format does not match its file extension",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ArtifactValidationError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFormat {
    #[default]
    Auto,
    Tar,
    Raw,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSpec {
    pub path: String,
    #[serde(default)]
    pub format: ArtifactFormat,
}

impl ArtifactSpec {
    pub fn from_path(path: impl Into<String>) -> Self {
        let path = path.into();
        let format = if looks_like_raw_artifact(&path) {
            ArtifactFormat::Raw
        } else {
            ArtifactFormat::Tar
        };
        Self { path, format }
    }

    pub fn validate(&self) -> Result<(), ArtifactValidationError> {
        if self.path.trim().is_empty() {
            return Err(ArtifactValidationError::EmptyPath);
        }
        if self.path.chars().any(char::is_control) {
            return Err(ArtifactValidationError::ControlCharacter);
        }
        let looks_raw = looks_like_raw_artifact(&self.path);
        let looks_tar = looks_like_tar_artifact(&self.path);
        if (self.format == ArtifactFormat::Raw && looks_tar)
            || (self.format == ArtifactFormat::Tar && looks_raw)
        {
            return Err(ArtifactValidationError::FormatMismatch);
        }
        Ok(())
    }

    pub fn expanded_path(&self) -> String {
        if self.path == "~" || self.path.starts_with("~/") {
            if let Some(home) = dirs::home_dir() {
                return if self.path == "~" {
                    home.to_string_lossy().into_owned()
                } else {
                    home.join(&self.path[2..]).to_string_lossy().into_owned()
                };
            }
        }
        self.path.clone()
    }

    pub fn is_external_storage(&self) -> bool {
        self.resolved_format() == ArtifactFormat::Raw
    }

    pub fn resolved_format(&self) -> ArtifactFormat {
        match self.format {
            ArtifactFormat::Auto if looks_like_raw_artifact(&self.path) => ArtifactFormat::Raw,
            ArtifactFormat::Auto => ArtifactFormat::Tar,
            explicit => explicit,
        }
    }
}

fn artifact_basename_without_compression(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    [".gz", ".xz", ".zst", ".bz2"]
        .iter()
        .find_map(|suffix| lower.strip_suffix(suffix).map(str::to_string))
        .unwrap_or(lower)
}

fn looks_like_raw_artifact(path: &str) -> bool {
    let base = artifact_basename_without_compression(path);
    base.ends_with(".raw") || base.ends_with(".img")
}

fn looks_like_tar_artifact(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    artifact_basename_without_compression(path).ends_with(".tar") || lower.ends_with(".tgz")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_auto_format_keeps_raw_images_external() {
        assert!(ArtifactSpec::from_path("/tmp/rootfs.raw").is_external_storage());
        assert!(ArtifactSpec::from_path("/tmp/rootfs.img").is_external_storage());
        assert!(ArtifactSpec {
            path: "/tmp/rootfs.raw.xz".into(),
            format: ArtifactFormat::Auto,
        }
        .is_external_storage());
        assert!(!ArtifactSpec::from_path("/tmp/rootfs.tar.xz").is_external_storage());
    }

    #[test]
    fn artifact_validation_rejects_ambiguous_or_unsafe_paths() {
        assert_eq!(
            ArtifactSpec {
                path: String::new(),
                format: ArtifactFormat::Auto,
            }
            .validate(),
            Err(ArtifactValidationError::EmptyPath)
        );
        assert_eq!(
            ArtifactSpec {
                path: "/tmp/rootfs.tar\n".into(),
                format: ArtifactFormat::Auto,
            }
            .validate(),
            Err(ArtifactValidationError::ControlCharacter)
        );
        assert_eq!(
            ArtifactSpec {
                path: "/tmp/rootfs.raw".into(),
                format: ArtifactFormat::Tar,
            }
            .validate(),
            Err(ArtifactValidationError::FormatMismatch)
        );
    }
}

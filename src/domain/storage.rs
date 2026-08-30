//! Storage values used by provisioning intents.
//!
//! These types describe a requested disk image independently of the host
//! commands used to create, mount, or format it. Adapter code owns those
//! commands and maps validation errors into its local error type.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

pub const MAX_DISK_IMAGE_SIZE_BYTES: u64 = 64 * 1024 * 1024 * 1024 * 1024;
pub const MAX_DISK_IMAGE_PARTITIONS: u32 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskImageValidationError {
    InvalidSize,
    SizeOverflow,
    UnsupportedUnit,
    SizeOutOfRange,
    InvalidPartition,
}

impl fmt::Display for DiskImageValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidSize => "invalid disk image size; use an integer such as 10G or 500M",
            Self::SizeOverflow => "disk image size is outside the supported range",
            Self::UnsupportedUnit => "unsupported disk image size unit; use B, K, M, G, or T",
            Self::SizeOutOfRange => "disk image size is outside the supported range",
            Self::InvalidPartition => "disk image partition must be between 1 and 128",
        };
        f.write_str(message)
    }
}

impl std::error::Error for DiskImageValidationError {}

/// Parse the bounded integer-unit syntax accepted for raw image creation.
pub fn parse_disk_image_size(value: &str) -> Result<u64, DiskImageValidationError> {
    if value.is_empty() || value.trim() != value || value.len() > 32 {
        return Err(DiskImageValidationError::InvalidSize);
    }
    let digit_count = value.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return Err(DiskImageValidationError::InvalidSize);
    }
    let amount = value[..digit_count]
        .parse::<u64>()
        .map_err(|_| DiskImageValidationError::SizeOverflow)?;
    let unit = value[digit_count..].to_ascii_uppercase();
    let factor = match unit.as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024_u64.pow(2),
        "G" | "GB" | "GIB" => 1024_u64.pow(3),
        "T" | "TB" | "TIB" => 1024_u64.pow(4),
        _ => return Err(DiskImageValidationError::UnsupportedUnit),
    };
    let bytes = amount
        .checked_mul(factor)
        .ok_or(DiskImageValidationError::SizeOverflow)?;
    if bytes == 0 || bytes > MAX_DISK_IMAGE_SIZE_BYTES {
        return Err(DiskImageValidationError::SizeOutOfRange);
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiskImageFilesystem {
    #[default]
    Ext4,
    Xfs,
    Btrfs,
}

impl DiskImageFilesystem {
    pub const ALL: [Self; 3] = [Self::Ext4, Self::Xfs, Self::Btrfs];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ext4 => "ext4",
            Self::Xfs => "xfs",
            Self::Btrfs => "btrfs",
        }
    }
}

impl fmt::Display for DiskImageFilesystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiskImageSource {
    CreateNew {
        size: String,
        fs_type: DiskImageFilesystem,
    },
    ImportExisting {
        path: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DiskImagePartition(u32);

impl DiskImagePartition {
    pub fn new(number: u32) -> Result<Self, DiskImageValidationError> {
        if (1..=MAX_DISK_IMAGE_PARTITIONS).contains(&number) {
            Ok(Self(number))
        } else {
            Err(DiskImageValidationError::InvalidPartition)
        }
    }

    pub fn number(self) -> u32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for DiskImagePartition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let number = u32::deserialize(deserializer)?;
        Self::new(number).map_err(serde::de::Error::custom)
    }
}

/// Configuration for disk image storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskImageConfig {
    pub source: DiskImageSource,
    pub use_partition_table: bool,
    #[serde(default)]
    pub root_partition: Option<DiskImagePartition>,
}

impl Default for DiskImageConfig {
    fn default() -> Self {
        Self {
            source: DiskImageSource::CreateNew {
                size: "10G".to_string(),
                fs_type: DiskImageFilesystem::Ext4,
            },
            use_partition_table: true,
            root_partition: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_image_size_parser_accepts_bounded_integer_units() {
        assert_eq!(parse_disk_image_size("500M").unwrap(), 500 * 1024 * 1024);
        assert_eq!(
            parse_disk_image_size("2GiB").unwrap(),
            2 * 1024 * 1024 * 1024
        );
        assert!(parse_disk_image_size("0G").is_err());
        assert!(parse_disk_image_size("1.5G").is_err());
        assert!(parse_disk_image_size("10XB").is_err());
        assert!(parse_disk_image_size(" 10G").is_err());
    }

    #[test]
    fn disk_image_partition_is_bounded_and_validated_on_deserialization() {
        assert_eq!(DiskImagePartition::new(1).unwrap().number(), 1);
        assert_eq!(
            DiskImagePartition::new(MAX_DISK_IMAGE_PARTITIONS)
                .unwrap()
                .number(),
            MAX_DISK_IMAGE_PARTITIONS
        );
        assert!(DiskImagePartition::new(0).is_err());
        assert!(DiskImagePartition::new(MAX_DISK_IMAGE_PARTITIONS + 1).is_err());
        assert!(serde_json::from_str::<DiskImagePartition>("0").is_err());
    }

    #[test]
    fn legacy_disk_image_config_defaults_to_automatic_root_selection() {
        let json = r#"{
            "source": {"CreateNew": {"size": "2G", "fs_type": "ext4"}},
            "use_partition_table": true
        }"#;
        let config: DiskImageConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.root_partition, None);
    }
}

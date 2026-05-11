//! Configuration file loading for Lasper.
//!
//! Reads `~/.config/lasper/lasper.toml`, which holds both `[theme]` color
//! overrides and `[settings]` for general application options.

use crate::ui::theme::PartialTheme;
use std::path::PathBuf;

/// General application settings (`[settings]` section).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Always request root elevation on startup (equivalent to -e / --elevate).
    pub elevate: bool,
    /// Force CLI-only mode — skip DBus entirely. Useful for debugging.
    #[serde(rename = "cli-mode")]
    pub cli_mode: bool,
}

/// Top-level sections in lasper.toml.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// `None` when the `[theme]` section is absent from the file.
    /// `Some(PartialTheme::default())` when `[theme]` is present but empty.
    pub theme: Option<PartialTheme>,
    pub settings: AppSettings,
}

/// Full config parse. Returns `None` if the file is missing or malformed.
pub fn load_config() -> Option<AppConfig> {
    let path = config_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    match toml::from_str::<AppConfig>(&content) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            log::warn!("Failed to parse config {}: {}", path.display(), e);
            None
        }
    }
}

/// Early config read — returns only `[settings]` before the TUI is initialized.
pub fn load_settings() -> Option<AppSettings> {
    load_config().map(|c| c.settings)
}

/// Path to the user config file: `~/.config/lasper/lasper.toml`.
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("lasper").join("lasper.toml"))
}

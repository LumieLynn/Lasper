//! Configuration file loading for Lasper.
//!
//! Reads `~/.config/lasper/lasper.toml`, which holds `[theme]` color overrides,
//! `[settings]` for general application options, and typed bootstrap profiles.

use crate::adapters::elevated::ElevationMethod;
use crate::domain::bootstrap::{
    DebootstrapSpec, Dnf5Spec, PacstrapSpec, RootfsSourceSpec, DEFAULT_BOOTSTRAP_PROFILE,
};
use crate::domain::nvidia::NvidiaCdiSource;
use crate::domain::source::{ArtifactSpec, BootstrapMethod};
use crate::tui::foundation::theme::PartialTheme;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_SCROLLBACK_LINES: usize = 2_000;

/// General application settings (`[settings]` section).
#[derive(Debug, serde::Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Always request root elevation on startup (equivalent to -e / --elevate).
    pub elevate: bool,
    /// Privilege launcher used when elevation is enabled.
    #[serde(rename = "elevation-method")]
    pub elevation_method: ElevationMethod,
    /// Use runtime-state and systemd command backends instead of Lasper's DBus backend.
    #[serde(rename = "systemd-tools")]
    pub systemd_tools: bool,
    /// Maximum log lines retained per container buffer.
    #[serde(rename = "log-buffer-lines")]
    pub log_buffer_lines: usize,
    /// Maximum terminal scrollback rows retained per embedded session.
    #[serde(rename = "scrollback-lines")]
    pub scrollback_lines: usize,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            elevate: false,
            elevation_method: ElevationMethod::Sudo,
            systemd_tools: false,
            log_buffer_lines: 0,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
        }
    }
}

/// Top-level sections in lasper.toml.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// `None` when the `[theme]` section is absent from the file.
    /// `Some(PartialTheme::default())` when `[theme]` is present but empty.
    pub theme: Option<PartialTheme>,
    pub settings: AppSettings,
    pub nvidia: NvidiaSettings,
    pub bootstrap: BootstrapSettings,
}

/// NVIDIA CDI acquisition policy used by discovery, deployment, and machine
/// start reconciliation.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NvidiaSettings {
    #[serde(rename = "cdi-source")]
    pub cdi_source: NvidiaCdiSourceMode,
    #[serde(rename = "cdi-file")]
    pub cdi_file: Option<PathBuf>,
}

impl NvidiaSettings {
    pub fn source(&self) -> NvidiaCdiSource {
        match self.cdi_source {
            NvidiaCdiSourceMode::Generate => NvidiaCdiSource::Generate,
            NvidiaCdiSourceMode::Existing => NvidiaCdiSource::existing(self.cdi_file.clone()),
        }
    }
}

impl Default for NvidiaSettings {
    fn default() -> Self {
        Self {
            cdi_source: NvidiaCdiSourceMode::Generate,
            cdi_file: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NvidiaCdiSourceMode {
    #[default]
    Generate,
    Existing,
}

/// Typed rootfs source methods and profiles used to preconfigure the wizard.
///
/// Profiles live below their method, so profile names never double as provider
/// identifiers. This section intentionally has no generic `flags` or executable
/// path field.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BootstrapSettings {
    #[serde(rename = "default-method")]
    pub default_method: Option<BootstrapMethod>,
    pub methods: BootstrapMethods,
}

impl BootstrapSettings {
    pub fn resolve(&self) -> ResolvedBootstrapSettings {
        let mut diagnostics = Vec::new();
        let profiles: Vec<_> = BootstrapMethod::ALL
            .into_iter()
            .flat_map(|method| self.methods.profiles(method))
            .filter(|profile| {
                let reason = if profile.name.trim().is_empty()
                    || profile.name.chars().any(char::is_control)
                {
                    Some("Profile names must be non-empty and contain no control characters".into())
                } else {
                    let validation = if profile.name == DEFAULT_BOOTSTRAP_PROFILE {
                        profile.source.validate_default_preset()
                    } else {
                        profile.source.validate()
                    };
                    validation.err().map(|error| error.to_string())
                };
                if let Some(reason) = reason {
                    diagnostics.push(BootstrapDiagnostic {
                        setting: format!(
                            "bootstrap.methods.{}.profiles.{}",
                            profile.method,
                            quoted_config_key(&profile.name)
                        ),
                        reason: format!("Profile ignored: {reason}"),
                    });
                    false
                } else {
                    true
                }
            })
            .collect();
        for method in BootstrapMethod::ALL {
            let name = self.methods.default_profile(method);
            if name != DEFAULT_BOOTSTRAP_PROFILE
                && !profiles
                    .iter()
                    .any(|profile| profile.method == method && profile.name == name)
            {
                diagnostics.push(BootstrapDiagnostic {
                    setting: format!("bootstrap.methods.{method}.default-profile"),
                    reason: format!(
                        "Profile {} is missing or invalid; the built-in {method} form will be used instead",
                        quoted_config_key(name)
                    ),
                });
            }
        }
        let default_profile = self.default_method.map(|method| {
            let name = self.methods.default_profile(method);
            if profiles
                .iter()
                .any(|profile| profile.method == method && profile.name == name)
            {
                name.to_string()
            } else {
                DEFAULT_BOOTSTRAP_PROFILE.to_string()
            }
        });
        ResolvedBootstrapSettings {
            default_method: self.default_method,
            default_profile,
            profiles,
            diagnostics,
        }
    }
}

/// Format a user-supplied key as a single-line TOML basic string.
fn quoted_config_key(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            character if character.is_control() => {
                quoted.push_str(&format!("\\u{:04X}", u32::from(character)));
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

pub struct ResolvedBootstrapSettings {
    pub default_method: Option<BootstrapMethod>,
    pub default_profile: Option<String>,
    pub profiles: Vec<ResolvedBootstrapProfile>,
    pub diagnostics: Vec<BootstrapDiagnostic>,
}

#[derive(Debug)]
pub struct BootstrapDiagnostic {
    pub setting: String,
    pub reason: String,
}

pub struct ResolvedBootstrapProfile {
    pub method: BootstrapMethod,
    pub name: String,
    pub source: RootfsSourceSpec,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BootstrapMethods {
    debootstrap: BootstrapMethodProfiles<DebootstrapSpec>,
    pacstrap: BootstrapMethodProfiles<PacstrapSpec>,
    dnf5: BootstrapMethodProfiles<Dnf5Spec>,
    artifact: BootstrapMethodProfiles<ArtifactSpec>,
}

impl BootstrapMethods {
    fn default_profile(&self, method: BootstrapMethod) -> &str {
        let configured = match method {
            BootstrapMethod::Debootstrap => self.debootstrap.default_profile.as_ref(),
            BootstrapMethod::Pacstrap => self.pacstrap.default_profile.as_ref(),
            BootstrapMethod::Dnf5 => self.dnf5.default_profile.as_ref(),
            BootstrapMethod::Artifact => self.artifact.default_profile.as_ref(),
        };
        configured
            .map(String::as_str)
            .unwrap_or(DEFAULT_BOOTSTRAP_PROFILE)
    }

    fn profiles(&self, method: BootstrapMethod) -> Vec<ResolvedBootstrapProfile> {
        match method {
            BootstrapMethod::Debootstrap => self
                .debootstrap
                .resolve_profiles(method, RootfsSourceSpec::Debootstrap),
            BootstrapMethod::Pacstrap => self
                .pacstrap
                .resolve_profiles(method, RootfsSourceSpec::Pacstrap),
            BootstrapMethod::Dnf5 => self.dnf5.resolve_profiles(method, RootfsSourceSpec::Dnf5),
            BootstrapMethod::Artifact => self
                .artifact
                .resolve_profiles(method, RootfsSourceSpec::Artifact),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BootstrapMethodProfiles<T> {
    #[serde(rename = "default-profile")]
    default_profile: Option<String>,
    profiles: BTreeMap<String, T>,
}

impl<T> Default for BootstrapMethodProfiles<T> {
    fn default() -> Self {
        Self {
            default_profile: None,
            profiles: BTreeMap::new(),
        }
    }
}

impl<T: Clone> BootstrapMethodProfiles<T> {
    fn resolve_profiles(
        &self,
        method: BootstrapMethod,
        source: impl Fn(T) -> RootfsSourceSpec,
    ) -> Vec<ResolvedBootstrapProfile> {
        self.profiles
            .iter()
            .map(|(name, spec)| ResolvedBootstrapProfile {
                method,
                name: name.clone(),
                source: source(spec.clone()),
            })
            .collect()
    }
}

#[derive(Debug)]
pub struct ConfigDiagnostic {
    pub summary: String,
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub diagnostic: Option<ConfigDiagnostic>,
}

/// Load the complete configuration once during startup.
///
/// Missing files use defaults silently. Unreadable or invalid files also use
/// defaults, but retain a diagnostic for the log and status banner.
pub fn load_config() -> LoadedConfig {
    let Some(path) = config_path() else {
        return LoadedConfig::default();
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadedConfig::default();
        }
        Err(error) => {
            return LoadedConfig {
                config: AppConfig::default(),
                diagnostic: Some(ConfigDiagnostic {
                    summary: format!("Config ignored: {}", error),
                    detail: format!("Failed to read config {}: {}", path.display(), error),
                }),
            };
        }
    };
    parse_config(&content, &path)
}

fn parse_config(content: &str, path: &Path) -> LoadedConfig {
    match toml::from_str::<AppConfig>(content) {
        Ok(config) => LoadedConfig {
            config,
            diagnostic: None,
        },
        Err(error) => {
            let location = error
                .span()
                .map(|span| line_column(content, span.start))
                .map(|(line, column)| format!(":{line}:{column}"))
                .unwrap_or_default();
            let reason = error
                .message()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("config");
            LoadedConfig {
                config: AppConfig::default(),
                diagnostic: Some(ConfigDiagnostic {
                    summary: format!("Config ignored ({name}{location}): {reason}"),
                    detail: format!("Failed to parse config {}: {}", path.display(), error),
                }),
            }
        }
    }
}

fn line_column(content: &str, offset: usize) -> (usize, usize) {
    let prefix = &content.as_bytes()[..offset.min(content.len())];
    let line = prefix.iter().filter(|byte| **byte == b'\n').count() + 1;
    let column = prefix
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(prefix.len() + 1, |position| prefix.len() - position);
    (line, column)
}

/// Path to the user config file: `~/.config/lasper/lasper.toml`.
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("lasper").join("lasper.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::bootstrap::DebootstrapReleaseSignaturePolicy;

    #[test]
    fn elevation_method_defaults_to_sudo_and_does_not_enable_elevation_itself() {
        assert_eq!(
            AppSettings::default().elevation_method,
            ElevationMethod::Sudo
        );
        for (name, method) in [
            ("sudo", ElevationMethod::Sudo),
            ("run0", ElevationMethod::Run0),
        ] {
            let config: AppConfig =
                toml::from_str(&format!("[settings]\nelevation-method = \"{name}\"\n")).unwrap();
            assert_eq!(config.settings.elevation_method, method);
            assert!(!config.settings.elevate);
        }
        assert!(
            toml::from_str::<AppConfig>("[settings]\nelevation-method = \"pkexec\"\n").is_err()
        );
    }

    #[test]
    fn systemd_tools_setting_selects_the_transport() {
        for enabled in [false, true] {
            let config: AppConfig =
                toml::from_str(&format!("[settings]\nsystemd-tools = {enabled}\n")).unwrap();
            assert_eq!(config.settings.systemd_tools, enabled);
        }
        assert!(!AppSettings::default().systemd_tools);
    }

    #[test]
    fn nvidia_cdi_source_defaults_to_generation_and_accepts_existing_files() {
        assert_eq!(
            AppConfig::default().nvidia.source(),
            NvidiaCdiSource::Generate
        );

        let config: AppConfig = toml::from_str(
            r#"
                [nvidia]
                cdi-source = "existing"
                cdi-file = "/var/run/cdi/nvidia.yaml"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.nvidia.source(),
            NvidiaCdiSource::existing(Some("/var/run/cdi/nvidia.yaml".into()))
        );
    }

    #[test]
    fn removed_cli_mode_setting_does_not_select_the_transport() {
        for settings in ["cli-mode = true", "systemd-tools = false\ncli-mode = true"] {
            let config: AppConfig = toml::from_str(&format!("[settings]\n{settings}\n")).unwrap();
            assert!(!config.settings.systemd_tools);
        }
    }

    #[test]
    fn systemd_tools_badge_accepts_its_theme_key() {
        let config: AppConfig = toml::from_str("[theme]\nbadge_systemd_tools = 'cyan'\n").unwrap();
        assert!(config.theme.unwrap().badge_systemd_tools.is_some());
    }

    #[test]
    fn unknown_theme_keys_produce_a_config_diagnostic() {
        for key in [
            "accentt",
            "wizard_footer",
            "highlight_secondary",
            "badge_cli",
        ] {
            let loaded = parse_config(
                &format!("[theme]\n{key} = 'cyan'\n"),
                Path::new("lasper.toml"),
            );
            let diagnostic = loaded.diagnostic.unwrap();
            assert!(diagnostic.summary.contains("unknown field"));
            assert!(diagnostic.summary.contains(key));
        }
    }

    #[test]
    fn bootstrap_profiles_deserialize_as_typed_provider_specs() {
        let config: AppConfig = toml::from_str(
            r#"
                [bootstrap]
                default-method = "debootstrap"

                [bootstrap.methods.debootstrap]
                default-profile = "debian"

                [bootstrap.methods.debootstrap.profiles.debian]
                suite = "bookworm"
                architecture = "amd64"
                mirror = "https://deb.debian.org/debian"
                packages = ["sudo", "zsh"]
                exclude_packages = ["nano"]
                extra_suites = ["bookworm-updates"]
                usr_merge = "merged"
                dependency_resolution = "skip_resolution"
                log_extra_dependencies = true

                [bootstrap.methods.debootstrap.profiles.debian.policy]
                transport = "https_only"
                release_signatures = "required"
            "#,
        )
        .unwrap();

        let resolved = config.bootstrap.resolve();
        assert_eq!(resolved.default_method, Some(BootstrapMethod::Debootstrap));
        assert_eq!(resolved.default_profile.as_deref(), Some("debian"));
        match &resolved.profiles[0].source {
            RootfsSourceSpec::Debootstrap(spec) => {
                assert_eq!(spec.suite, "bookworm");
                assert_eq!(spec.architecture.as_deref(), Some("amd64"));
                assert!(spec.inherit_default_packages);
                assert_eq!(spec.exclude_packages, ["nano"]);
                assert_eq!(
                    spec.policy.release_signatures,
                    DebootstrapReleaseSignaturePolicy::Required
                );
            }
            _ => panic!("expected debootstrap profile"),
        }
    }

    #[test]
    fn arbitrary_bootstrap_flags_are_rejected() {
        let parsed = toml::from_str::<AppConfig>(
            r#"
                [bootstrap.methods.debootstrap.profiles.bad]
                suite = "bookworm"
                flags = ["--no-check-sig"]
            "#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn bootstrap_profiles_can_disable_implicit_default_packages() {
        let config: AppConfig = toml::from_str(
            r#"
                [bootstrap.methods.debootstrap.profiles.legacy]
                suite = "bullseye"
                inherit_default_packages = false
                packages = ["systemd-sysv", "dbus"]
            "#,
        )
        .unwrap();

        let resolved = config.bootstrap.resolve();
        let RootfsSourceSpec::Debootstrap(spec) = &resolved.profiles[0].source else {
            panic!("expected debootstrap profile");
        };
        assert!(!spec.inherit_default_packages);
        assert_eq!(spec.packages, ["systemd-sysv", "dbus"]);
    }

    #[test]
    fn disabled_debootstrap_signature_policy_maps_to_typed_spec() {
        let config: AppConfig = toml::from_str(
            r#"
                [bootstrap.methods.debootstrap.profiles.insecure]
                suite = "bookworm"

                [bootstrap.methods.debootstrap.profiles.insecure.policy]
                release_signatures = "disabled"
            "#,
        )
        .unwrap();
        let resolved = config.bootstrap.resolve();
        let RootfsSourceSpec::Debootstrap(spec) = &resolved.profiles[0].source else {
            panic!("expected debootstrap profile");
        };
        assert_eq!(
            spec.policy.release_signatures,
            DebootstrapReleaseSignaturePolicy::Disabled
        );
    }

    #[test]
    fn pacstrap_and_dnf5_profiles_deserialize_provider_specific_policies() {
        let config: AppConfig = toml::from_str(
            r#"
                [bootstrap.methods.pacstrap.profiles.arch]
                packages = ["zsh"]
                cache = "target"
                isolation = "unshare"
                dependency_checks = "skip_checks"

                [bootstrap.methods.pacstrap.profiles.arch.policy]
                keyring = "initialize_empty"
                mirrorlist = "do_not_copy"
                pacman_config = "copy_host"

                [bootstrap.methods.dnf5.profiles.fedora]
                releasever = "43"
                architecture = "x86_64"
                packages = ["systemd"]
                exclude_packages = ["kernel-debug*"]
                only_repositories = ["fedora", "updates"]
                metadata = "refresh"
                repository = "host"

                [bootstrap.methods.dnf5.profiles.fedora.policy]
                package_signatures = "repository_config"
                weak_dependencies = "disabled"
                documentation = "exclude"
                best_candidate = "required"
            "#,
        )
        .unwrap();

        let resolved = config.bootstrap.resolve();
        assert_eq!(resolved.profiles.len(), 2);
        assert!(resolved
            .profiles
            .iter()
            .all(|profile| profile.source.validate().is_ok()));
    }

    #[test]
    fn provider_policy_rejects_fields_it_cannot_implement() {
        let parsed = toml::from_str::<AppConfig>(
            r#"
                [bootstrap.methods.dnf5.profiles.bad]
                releasever = "43"
                packages = ["systemd"]
                repository = "host"

                [bootstrap.methods.dnf5.profiles.bad.policy]
                metadata_signatures = "disabled"
            "#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn dnf5_named_profile_requires_explicit_repository_source() {
        let config = toml::from_str::<AppConfig>(
            r#"
                [bootstrap.methods.dnf5.profiles.bad]
                releasever = "43"
                packages = ["systemd"]
            "#,
        )
        .unwrap();
        let resolved = config.bootstrap.resolve();
        assert!(resolved.profiles.is_empty());
        assert_eq!(resolved.diagnostics.len(), 1);
        assert_eq!(
            resolved.diagnostics[0].setting,
            "bootstrap.methods.dnf5.profiles.\"bad\""
        );
        assert!(resolved.diagnostics[0]
            .reason
            .contains("repository=\"host\""));
    }

    #[test]
    fn default_profiles_accept_policy_without_form_fields() {
        let config = toml::from_str::<AppConfig>(
            r#"
                [bootstrap.methods.debootstrap.profiles.default.policy]
                release_signatures = "required"

                [bootstrap.methods.pacstrap.profiles.default.policy]
                pacman_config = "copy_host"

                [bootstrap.methods.dnf5.profiles.default.policy]
                package_signatures = "disabled"
            "#,
        )
        .unwrap();
        let resolved = config.bootstrap.resolve();

        assert_eq!(resolved.profiles.len(), 3);
        assert!(resolved.diagnostics.is_empty());
        assert!(resolved
            .profiles
            .iter()
            .all(|profile| profile.source.validate_default_preset().is_ok()));
    }

    #[test]
    fn default_method_selects_its_own_default_profile() {
        let config: AppConfig = toml::from_str(
            r#"
                [bootstrap]
                default-method = "debootstrap"

                [bootstrap.methods.debootstrap]
                default-profile = "ubuntu-resolute"

                [bootstrap.methods.debootstrap.profiles."ubuntu-resolute"]
                suite = "resolute"
            "#,
        )
        .unwrap();

        let resolved = config.bootstrap.resolve();
        assert_eq!(resolved.default_method, Some(BootstrapMethod::Debootstrap));
        assert_eq!(resolved.default_profile.as_deref(), Some("ubuntu-resolute"));
    }

    #[test]
    fn default_method_uses_implicit_default_profile() {
        let config: AppConfig = toml::from_str(
            r#"
                [bootstrap]
                default-method = "debootstrap"
            "#,
        )
        .unwrap();

        let resolved = config.bootstrap.resolve();
        assert_eq!(resolved.default_method, Some(BootstrapMethod::Debootstrap));
        assert_eq!(resolved.default_profile.as_deref(), Some("default"));
    }

    #[test]
    fn identical_profile_names_are_scoped_by_method() {
        let config: AppConfig = toml::from_str(
            r#"
                [bootstrap]
                default-method = "pacstrap"

                [bootstrap.methods.debootstrap.profiles.default]
                suite = "bookworm"

                [bootstrap.methods.pacstrap]
                default-profile = "default"

                [bootstrap.methods.pacstrap.profiles.default]
                packages = ["base-devel"]
            "#,
        )
        .unwrap();

        let resolved = config.bootstrap.resolve();
        assert_eq!(resolved.default_method, Some(BootstrapMethod::Pacstrap));
        assert_eq!(resolved.default_profile.as_deref(), Some("default"));
        assert_eq!(resolved.profiles.len(), 2);
    }

    #[test]
    fn duplicate_profile_table_reports_diagnostic_and_uses_defaults() {
        let loaded = parse_config(
            r#"
                [settings]
                elevate = true

                [bootstrap.methods.debootstrap.profiles.debian]
                suite = "bookworm"

                [bootstrap.methods.debootstrap.profiles.debian]
                suite = "trixie"
            "#,
            Path::new("lasper.toml"),
        );

        assert!(!loaded.config.settings.elevate);
        let diagnostic = loaded.diagnostic.expect("duplicate table diagnostic");
        assert!(diagnostic.summary.contains("lasper.toml:"));
        assert!(diagnostic.summary.contains("duplicate key"));
    }

    #[test]
    fn invalid_profiles_are_reported_without_losing_valid_settings() {
        let loaded = parse_config(
            r#"
                [settings]
                elevate = true

                [bootstrap.methods.debootstrap.profiles.valid]
                suite = "trixie"

                [bootstrap.methods.debootstrap.profiles.incomplete]

                [bootstrap.methods.pacstrap.profiles." "]
                packages = ["base"]
            "#,
            Path::new("lasper.toml"),
        );
        assert!(loaded.diagnostic.is_none());
        assert!(loaded.config.settings.elevate);
        let resolved = loaded.config.bootstrap.resolve();
        assert_eq!(resolved.profiles.len(), 1);
        assert_eq!(resolved.profiles[0].name, "valid");
        assert_eq!(resolved.diagnostics.len(), 2);
        assert!(resolved.diagnostics[0].setting.contains("incomplete"));
        assert!(resolved.diagnostics[0].reason.contains("suite"));
        assert!(resolved.diagnostics[1].reason.contains("Profile names"));
    }

    #[test]
    fn unavailable_default_profiles_report_the_setting_and_use_builtin_forms() {
        let config: AppConfig = toml::from_str(
            r#"
                [bootstrap]
                default-method = "debootstrap"

                [bootstrap.methods.debootstrap]
                default-profile = "missing"

                [bootstrap.methods.dnf5]
                default-profile = "incomplete"

                [bootstrap.methods.dnf5.profiles.incomplete]
                releasever = "43"
            "#,
        )
        .unwrap();
        let resolved = config.bootstrap.resolve();
        assert_eq!(resolved.default_method, Some(BootstrapMethod::Debootstrap));
        assert_eq!(resolved.default_profile.as_deref(), Some("default"));
        assert_eq!(resolved.diagnostics.len(), 3);
        assert_eq!(
            resolved.diagnostics[1].setting,
            "bootstrap.methods.debootstrap.default-profile"
        );
        assert!(resolved.diagnostics[1].reason.contains("\"missing\""));
        assert_eq!(
            resolved.diagnostics[2].setting,
            "bootstrap.methods.dnf5.default-profile"
        );
    }

    #[test]
    fn every_method_checks_its_configured_default_profile() {
        let content: String = BootstrapMethod::ALL
            .into_iter()
            .map(|method| format!("[bootstrap.methods.{method}]\ndefault-profile = \"missing\"\n"))
            .collect();
        let config: AppConfig = toml::from_str(&content).unwrap();
        let resolved = config.bootstrap.resolve();
        assert_eq!(resolved.diagnostics.len(), BootstrapMethod::ALL.len());
        for (diagnostic, method) in resolved.diagnostics.iter().zip(BootstrapMethod::ALL) {
            assert_eq!(
                diagnostic.setting,
                format!("bootstrap.methods.{method}.default-profile")
            );
        }
    }

    #[test]
    fn diagnostic_profile_keys_preserve_special_characters_as_toml() {
        let names = [
            "bad\"name",
            "bad\\name",
            "bad\0name",
            "bad\u{200b}name",
            "bad\nname",
            "bad\rname",
            "bad\tname",
            "bad\u{007f}name",
        ];
        let mut config = AppConfig::default();
        for name in names {
            config
                .bootstrap
                .methods
                .debootstrap
                .profiles
                .insert(name.into(), DebootstrapSpec::default());
        }
        let resolved = config.bootstrap.resolve();
        assert_eq!(resolved.diagnostics.len(), names.len());
        for diagnostic in resolved.diagnostics {
            let content = format!("{} = true", diagnostic.setting);
            assert_eq!(content.lines().count(), 1);
            let parsed: toml::Value = toml::from_str(&content).unwrap();
            let profiles = parsed["bootstrap"]["methods"]["debootstrap"]["profiles"]
                .as_table()
                .unwrap();
            assert_eq!(profiles.len(), 1);
            assert!(names.contains(&profiles.keys().next().unwrap().as_str()));
        }
    }
}

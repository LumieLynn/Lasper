//! Configuration file loading for Lasper.
//!
//! Reads `~/.config/lasper/lasper.toml`, which holds `[theme]` color overrides,
//! `[settings]` for general application options, and typed bootstrap profiles.

use crate::domain::bootstrap::{
    DebootstrapSpec, Dnf5Spec, PacstrapSpec, RootfsSourceSpec, DEFAULT_BOOTSTRAP_PROFILE,
};
use crate::domain::source::{ArtifactSpec, BootstrapMethod};
use crate::tui::theme::PartialTheme;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// General application settings (`[settings]` section).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Always request root elevation on startup (equivalent to -e / --elevate).
    pub elevate: bool,
    /// Use runtime-state and systemd command backends instead of Lasper's DBus backend.
    #[serde(rename = "cli-mode")]
    pub cli_mode: bool,
    /// Maximum log lines retained per container buffer.
    #[serde(rename = "log-buffer-lines")]
    pub log_buffer_lines: usize,
}

/// Top-level sections in lasper.toml.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// `None` when the `[theme]` section is absent from the file.
    /// `Some(PartialTheme::default())` when `[theme]` is present but empty.
    pub theme: Option<PartialTheme>,
    pub settings: AppSettings,
    pub bootstrap: BootstrapSettings,
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
        let default_profile = self
            .default_method
            .map(|method| self.methods.default_profile(method).to_string());
        ResolvedBootstrapSettings {
            default_method: self.default_method,
            default_profile,
            profiles: self.methods.profiles(),
        }
    }
}

pub struct ResolvedBootstrapSettings {
    pub default_method: Option<BootstrapMethod>,
    pub default_profile: Option<String>,
    pub profiles: Vec<ResolvedBootstrapProfile>,
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

    fn profiles(&self) -> Vec<ResolvedBootstrapProfile> {
        let mut profiles = Vec::new();
        profiles.extend(self.debootstrap.profiles.iter().map(|(name, spec)| {
            ResolvedBootstrapProfile {
                method: BootstrapMethod::Debootstrap,
                name: name.clone(),
                source: RootfsSourceSpec::Debootstrap(spec.clone()),
            }
        }));
        profiles.extend(self.pacstrap.profiles.iter().map(|(name, spec)| {
            ResolvedBootstrapProfile {
                method: BootstrapMethod::Pacstrap,
                name: name.clone(),
                source: RootfsSourceSpec::Pacstrap(spec.clone()),
            }
        }));
        profiles.extend(
            self.dnf5
                .profiles
                .iter()
                .map(|(name, spec)| ResolvedBootstrapProfile {
                    method: BootstrapMethod::Dnf5,
                    name: name.clone(),
                    source: RootfsSourceSpec::Dnf5(spec.clone()),
                }),
        );
        profiles.extend(self.artifact.profiles.iter().map(|(name, spec)| {
            ResolvedBootstrapProfile {
                method: BootstrapMethod::Artifact,
                name: name.clone(),
                source: RootfsSourceSpec::Artifact(spec.clone()),
            }
        }));
        profiles
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
        assert!(resolved.profiles[0].source.validate().is_err());
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
}

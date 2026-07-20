//! Typed rootfs source and bootstrap specifications.
//!
//! These values are shared by the wizard, configuration loader, and the
//! elevated bootstrap operation.  They deliberately describe provider
//! semantics instead of accepting arbitrary command-line arguments.

use crate::nspawn::errors::{NspawnError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_BOOTSTRAP_PROFILE: &str = "default";

/// Packages required for the systemd-nspawn runtime contract. Profile
/// `packages` are appended to these provider baselines.
pub const DEBOOTSTRAP_BASE_PACKAGES: &[&str] = &[
    "systemd-sysv",
    "libpam-systemd",
    "dbus",
    "dbus-user-session",
];
pub const PACSTRAP_BASE_PACKAGES: &[&str] = &["base"];
pub const DNF5_BASE_PACKAGES: &[&str] = &[
    "systemd",
    "systemd-pam",
    "dbus",
    "shadow-utils",
    "util-linux",
    "dnf5",
    "systemd-networkd",
    "systemd-resolved",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapMethod {
    Debootstrap,
    Pacstrap,
    Dnf5,
    Artifact,
}

impl BootstrapMethod {
    pub fn required_tool(self) -> Option<&'static str> {
        match self {
            Self::Debootstrap => Some("debootstrap"),
            Self::Pacstrap => Some("pacstrap"),
            Self::Dnf5 => Some("dnf5"),
            Self::Artifact => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebootstrapTransportPolicy {
    #[default]
    ProviderDefault,
    HttpsOnly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebootstrapReleaseSignaturePolicy {
    #[default]
    ProviderDefault,
    Required,
    Disabled,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebootstrapUsrMergeMode {
    #[default]
    ProviderDefault,
    Merged,
    Unmerged,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebootstrapDependencyMode {
    #[default]
    Resolve,
    SkipResolution,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DebootstrapSignatureOptionStyle {
    Sig,
    Gpg,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebootstrapPolicy {
    #[serde(default)]
    pub transport: DebootstrapTransportPolicy,
    #[serde(default)]
    pub release_signatures: DebootstrapReleaseSignaturePolicy,
    #[serde(default)]
    pub allowed_mirror_hosts: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebootstrapSpec {
    #[serde(default)]
    pub suite: String,
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub mirror: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub exclude_packages: Vec<String>,
    #[serde(default)]
    pub extra_suites: Vec<String>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default)]
    pub usr_merge: DebootstrapUsrMergeMode,
    #[serde(default)]
    pub dependency_resolution: DebootstrapDependencyMode,
    #[serde(default)]
    pub log_extra_dependencies: bool,
    #[serde(default)]
    pub policy: DebootstrapPolicy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacstrapCacheMode {
    Target,
    #[default]
    Host,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacmanKeyringMode {
    #[default]
    CopyHost,
    DoNotCopy,
    InitializeEmpty,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacmanMirrorlistMode {
    #[default]
    CopyHost,
    DoNotCopy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacstrapIsolationMode {
    #[default]
    Host,
    Unshare,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacstrapPacmanConfigMode {
    #[default]
    ProviderDefault,
    CopyHost,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacstrapDependencyMode {
    #[default]
    Check,
    SkipChecks,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacstrapPolicy {
    #[serde(default)]
    pub keyring: PacmanKeyringMode,
    #[serde(default)]
    pub mirrorlist: PacmanMirrorlistMode,
    #[serde(default)]
    pub pacman_config: PacstrapPacmanConfigMode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacstrapSpec {
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub cache: PacstrapCacheMode,
    #[serde(default)]
    pub isolation: PacstrapIsolationMode,
    #[serde(default)]
    pub dependency_checks: PacstrapDependencyMode,
    #[serde(default)]
    pub policy: PacstrapPolicy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dnf5RepositorySource {
    #[default]
    #[serde(skip)]
    Unspecified,
    Host,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dnf5PackageSignaturePolicy {
    #[default]
    RepositoryConfig,
    Disabled,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dnf5MetadataMode {
    #[default]
    ProviderDefault,
    Refresh,
    CacheOnly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dnf5WeakDependencyPolicy {
    #[default]
    ProviderDefault,
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dnf5DocumentationPolicy {
    #[default]
    ProviderDefault,
    Exclude,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dnf5BestCandidatePolicy {
    #[default]
    ProviderDefault,
    Required,
    AllowOlder,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dnf5Policy {
    #[serde(default)]
    pub package_signatures: Dnf5PackageSignaturePolicy,
    #[serde(default)]
    pub weak_dependencies: Dnf5WeakDependencyPolicy,
    #[serde(default)]
    pub documentation: Dnf5DocumentationPolicy,
    #[serde(default)]
    pub best_candidate: Dnf5BestCandidatePolicy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dnf5Spec {
    #[serde(default)]
    pub releasever: String,
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub exclude_packages: Vec<String>,
    #[serde(default)]
    pub only_repositories: Vec<String>,
    #[serde(default)]
    pub enable_repositories: Vec<String>,
    #[serde(default)]
    pub disable_repositories: Vec<String>,
    #[serde(default)]
    pub metadata: Dnf5MetadataMode,
    /// DNF5 needs repository configuration while the installroot is empty.
    /// Named profiles declare this; the built-in editable source supplies Host.
    #[serde(default)]
    pub repository: Dnf5RepositorySource,
    #[serde(default)]
    pub policy: Dnf5Policy,
}

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

    pub fn validate(&self) -> Result<()> {
        if self.path.trim().is_empty() {
            return Err(validation("Artifact path cannot be empty"));
        }
        if self.path.chars().any(char::is_control) {
            return Err(validation("Artifact path contains control characters"));
        }
        let looks_raw = looks_like_raw_artifact(&self.path);
        let looks_tar = looks_like_tar_artifact(&self.path);
        if (self.format == ArtifactFormat::Raw && looks_tar)
            || (self.format == ArtifactFormat::Tar && looks_raw)
        {
            return Err(validation(
                "Artifact format does not match its file extension",
            ));
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

/// A source that can be selected as a configured profile in `lasper.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", content = "config", rename_all = "snake_case")]
pub enum RootfsSourceSpec {
    Debootstrap(DebootstrapSpec),
    Pacstrap(PacstrapSpec),
    Dnf5(Dnf5Spec),
    Artifact(ArtifactSpec),
}

impl RootfsSourceSpec {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Debootstrap(spec) => spec.validate(),
            Self::Pacstrap(spec) => spec.validate(),
            Self::Dnf5(spec) => spec.validate(),
            Self::Artifact(spec) => spec.validate(),
        }
    }

    /// Validate a partial preset for the wizard's editable `default` form.
    pub fn validate_default_preset(&self) -> Result<()> {
        match self {
            Self::Debootstrap(spec) => spec.validate_default_preset(),
            Self::Pacstrap(spec) => spec.validate(),
            Self::Dnf5(spec) => spec.validate_default_preset(),
            Self::Artifact(spec) => spec.validate(),
        }
    }

    pub fn is_external_storage(&self) -> bool {
        matches!(self, Self::Artifact(spec) if spec.is_external_storage())
    }

    pub fn required_tool(&self) -> Option<&'static str> {
        self.method().required_tool()
    }

    pub fn method(&self) -> BootstrapMethod {
        match self {
            Self::Debootstrap(_) => BootstrapMethod::Debootstrap,
            Self::Pacstrap(_) => BootstrapMethod::Pacstrap,
            Self::Dnf5(_) => BootstrapMethod::Dnf5,
            Self::Artifact(_) => BootstrapMethod::Artifact,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", content = "config", rename_all = "snake_case")]
pub enum BootstrapSpec {
    Debootstrap(DebootstrapSpec),
    Pacstrap(PacstrapSpec),
    Dnf5(Dnf5Spec),
}

impl BootstrapSpec {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Debootstrap(spec) => spec.validate(),
            Self::Pacstrap(spec) => spec.validate(),
            Self::Dnf5(spec) => spec.validate(),
        }
    }
}

impl DebootstrapSpec {
    pub fn validate(&self) -> Result<()> {
        validate_token("suite", &self.suite)?;
        self.validate_optional_fields()
    }

    fn validate_default_preset(&self) -> Result<()> {
        if !self.suite.is_empty() {
            validate_token("suite", &self.suite)?;
        }
        self.validate_optional_fields()
    }

    fn validate_optional_fields(&self) -> Result<()> {
        if let Some(architecture) = &self.architecture {
            validate_token("architecture", architecture)?;
        }
        for package in &self.packages {
            validate_package(package)?;
        }
        for package in &self.exclude_packages {
            validate_package(package)?;
        }
        for suite in &self.extra_suites {
            validate_token("extra suite", suite)?;
        }
        for component in &self.components {
            validate_token("component", component)?;
        }
        if let Some(variant) = &self.variant {
            validate_token("variant", variant)?;
        }
        validate_debootstrap_policy(&self.policy, self.mirror.as_deref())
    }

    #[cfg(test)]
    pub fn args(&self, target: &Path, include_sudo: bool) -> Result<Vec<String>> {
        self.args_with_signature_style(target, include_sudo, DebootstrapSignatureOptionStyle::Sig)
    }

    pub(crate) fn args_with_signature_style(
        &self,
        target: &Path,
        include_sudo: bool,
        signature_style: DebootstrapSignatureOptionStyle,
    ) -> Result<Vec<String>> {
        self.validate()?;
        let mut args = Vec::new();
        if let Some(architecture) = &self.architecture {
            args.push(format!("--arch={architecture}"));
        }
        let packages = effective_packages(DEBOOTSTRAP_BASE_PACKAGES, include_sudo, &self.packages);
        args.push(format!("--include={}", packages.join(",")));
        if !self.exclude_packages.is_empty() {
            args.push(format!("--exclude={}", self.exclude_packages.join(",")));
        }
        if !self.extra_suites.is_empty() {
            args.push(format!("--extra-suites={}", self.extra_suites.join(",")));
        }
        if let Some(variant) = &self.variant {
            args.push(format!("--variant={variant}"));
        }
        if !self.components.is_empty() {
            args.push(format!("--components={}", self.components.join(",")));
        }
        match self.usr_merge {
            DebootstrapUsrMergeMode::ProviderDefault => {}
            DebootstrapUsrMergeMode::Merged => args.push("--merged-usr".into()),
            DebootstrapUsrMergeMode::Unmerged => args.push("--no-merged-usr".into()),
        }
        if self.dependency_resolution == DebootstrapDependencyMode::SkipResolution {
            args.push("--no-resolve-deps".into());
        }
        if self.log_extra_dependencies {
            args.push("--log-extra-deps".into());
        }
        match self.policy.release_signatures {
            DebootstrapReleaseSignaturePolicy::ProviderDefault => {}
            DebootstrapReleaseSignaturePolicy::Required => args.push(
                match signature_style {
                    DebootstrapSignatureOptionStyle::Sig => "--force-check-sig",
                    DebootstrapSignatureOptionStyle::Gpg => "--force-check-gpg",
                }
                .into(),
            ),
            DebootstrapReleaseSignaturePolicy::Disabled => args.push(
                match signature_style {
                    DebootstrapSignatureOptionStyle::Sig => "--no-check-sig",
                    DebootstrapSignatureOptionStyle::Gpg => "--no-check-gpg",
                }
                .into(),
            ),
        }
        args.push(self.suite.clone());
        args.push(target.to_string_lossy().into_owned());
        if let Some(mirror) = &self.mirror {
            args.push(mirror.clone());
        }
        Ok(args)
    }
}

impl PacstrapSpec {
    pub fn validate(&self) -> Result<()> {
        for package in &self.packages {
            validate_package(package)?;
        }
        Ok(())
    }

    pub fn args(&self, target: &Path, include_sudo: bool) -> Result<Vec<String>> {
        self.validate()?;
        let mut args = Vec::new();
        if self.cache == PacstrapCacheMode::Host {
            args.push("-c".into());
        }
        if self.isolation == PacstrapIsolationMode::Unshare {
            args.push("-N".into());
        }
        if self.dependency_checks == PacstrapDependencyMode::SkipChecks {
            args.push("-D".into());
        }
        if self.policy.pacman_config == PacstrapPacmanConfigMode::CopyHost {
            args.push("-P".into());
        }
        match self.policy.keyring {
            PacmanKeyringMode::CopyHost => {}
            PacmanKeyringMode::DoNotCopy => args.push("-G".into()),
            PacmanKeyringMode::InitializeEmpty => args.push("-K".into()),
        }
        if self.policy.mirrorlist == PacmanMirrorlistMode::DoNotCopy {
            args.push("-M".into());
        }
        args.push(target.to_string_lossy().into_owned());
        args.extend(effective_packages(
            PACSTRAP_BASE_PACKAGES,
            include_sudo,
            &self.packages,
        ));
        Ok(args)
    }
}

impl Dnf5Spec {
    pub fn validate(&self) -> Result<()> {
        validate_token("releasever", &self.releasever)?;
        if self.repository != Dnf5RepositorySource::Host {
            return Err(validation(
                "dnf5 requires repository=\"host\" until typed repository configuration is implemented",
            ));
        }
        self.validate_optional_fields()
    }

    fn validate_default_preset(&self) -> Result<()> {
        if !self.releasever.is_empty() {
            validate_token("releasever", &self.releasever)?;
        }
        self.validate_optional_fields()
    }

    fn validate_optional_fields(&self) -> Result<()> {
        if let Some(architecture) = &self.architecture {
            validate_token("architecture", architecture)?;
        }
        for package in self.packages.iter().chain(&self.exclude_packages) {
            validate_package(package)?;
        }
        for repository in self
            .only_repositories
            .iter()
            .chain(&self.enable_repositories)
            .chain(&self.disable_repositories)
        {
            validate_repository_selector(repository)?;
        }
        if !self.only_repositories.is_empty()
            && (!self.enable_repositories.is_empty() || !self.disable_repositories.is_empty())
        {
            return Err(validation(
                "dnf5 only_repositories cannot be combined with enable_repositories or disable_repositories",
            ));
        }
        Ok(())
    }

    pub fn args(&self, target: &Path, include_sudo: bool) -> Result<Vec<String>> {
        self.validate()?;
        let mut args = vec![
            "--installroot".into(),
            target.to_string_lossy().into_owned(),
            "--releasever".into(),
            self.releasever.clone(),
        ];
        if let Some(architecture) = &self.architecture {
            args.push(format!("--forcearch={architecture}"));
        }
        if self.repository == Dnf5RepositorySource::Host {
            args.push("--use-host-config".into());
        }
        match self.metadata {
            Dnf5MetadataMode::ProviderDefault => {}
            Dnf5MetadataMode::Refresh => args.push("--refresh".into()),
            Dnf5MetadataMode::CacheOnly => args.push("--cacheonly".into()),
        }
        if !self.only_repositories.is_empty() {
            args.push(format!("--repo={}", self.only_repositories.join(",")));
        }
        if !self.enable_repositories.is_empty() {
            args.push(format!(
                "--enable-repo={}",
                self.enable_repositories.join(",")
            ));
        }
        if !self.disable_repositories.is_empty() {
            args.push(format!(
                "--disable-repo={}",
                self.disable_repositories.join(",")
            ));
        }
        if !self.exclude_packages.is_empty() {
            args.push(format!("--exclude={}", self.exclude_packages.join(",")));
        }
        if self.policy.package_signatures == Dnf5PackageSignaturePolicy::Disabled {
            args.push("--no-gpgchecks".into());
        }
        match self.policy.weak_dependencies {
            Dnf5WeakDependencyPolicy::ProviderDefault => {}
            Dnf5WeakDependencyPolicy::Enabled => {
                args.push("--setopt=install_weak_deps=True".into())
            }
            Dnf5WeakDependencyPolicy::Disabled => {
                args.push("--setopt=install_weak_deps=False".into())
            }
        }
        if self.policy.documentation == Dnf5DocumentationPolicy::Exclude {
            args.push("--no-docs".into());
        }
        match self.policy.best_candidate {
            Dnf5BestCandidatePolicy::ProviderDefault => {}
            Dnf5BestCandidatePolicy::Required => args.push("--best".into()),
            Dnf5BestCandidatePolicy::AllowOlder => args.push("--no-best".into()),
        }
        args.push("--assumeyes".into());
        args.push("install".into());
        args.extend(effective_packages(
            DNF5_BASE_PACKAGES,
            include_sudo,
            &self.packages,
        ));
        Ok(args)
    }
}

fn effective_packages(baseline: &[&str], include_sudo: bool, additional: &[String]) -> Vec<String> {
    let mut packages =
        Vec::with_capacity(baseline.len() + additional.len() + usize::from(include_sudo));
    for package in baseline
        .iter()
        .copied()
        .chain(include_sudo.then_some("sudo"))
        .chain(additional.iter().map(String::as_str))
    {
        if !packages.iter().any(|existing| existing == package) {
            packages.push(package.to_string());
        }
    }
    packages
}

fn validate_debootstrap_policy(policy: &DebootstrapPolicy, mirror: Option<&str>) -> Result<()> {
    if policy.transport == DebootstrapTransportPolicy::HttpsOnly && mirror.is_none() {
        return Err(validation(
            "https-only bootstrap policy requires an explicit mirror",
        ));
    }
    if let Some(mirror) = mirror {
        let parsed = url::Url::parse(mirror)
            .map_err(|error| validation(format!("Invalid bootstrap mirror: {error}")))?;
        if policy.transport == DebootstrapTransportPolicy::HttpsOnly && parsed.scheme() != "https" {
            return Err(validation("Bootstrap mirror must use https"));
        }
        if !policy.allowed_mirror_hosts.is_empty() {
            let host = parsed
                .host_str()
                .ok_or_else(|| validation("Bootstrap mirror has no host"))?;
            if !policy
                .allowed_mirror_hosts
                .iter()
                .any(|allowed| allowed == host)
            {
                return Err(validation(format!(
                    "Bootstrap mirror host '{host}' is not in the source allowlist"
                )));
            }
        }
    } else if !policy.allowed_mirror_hosts.is_empty() {
        return Err(validation(
            "Bootstrap source allowlist requires an explicit mirror",
        ));
    }
    Ok(())
}

fn validate_token(field: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._:+/@".contains(&byte))
    {
        return Err(validation(format!("Invalid bootstrap {field}")));
    }
    Ok(())
}

fn validate_package(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with('-')
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
        || value
            .chars()
            .any(|c| matches!(c, ';' | '|' | '&' | '`' | '\'' | '"'))
    {
        return Err(validation(format!("Invalid bootstrap package '{value}'")));
    }
    Ok(())
}

fn validate_repository_selector(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._*?".contains(&byte))
    {
        return Err(validation(format!(
            "Invalid dnf5 repository selector '{value}'"
        )));
    }
    Ok(())
}

fn validation(message: impl Into<String>) -> NspawnError {
    NspawnError::Validation(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn debootstrap_args_are_typed_and_ordered() {
        let spec = DebootstrapSpec {
            suite: "bookworm".into(),
            architecture: Some("amd64".into()),
            mirror: Some("https://deb.debian.org/debian".into()),
            packages: vec!["zsh".into()],
            exclude_packages: vec!["nano".into()],
            extra_suites: vec!["bookworm-updates".into()],
            variant: Some("minbase".into()),
            components: vec!["main".into(), "contrib".into()],
            usr_merge: DebootstrapUsrMergeMode::Merged,
            dependency_resolution: DebootstrapDependencyMode::SkipResolution,
            log_extra_dependencies: true,
            policy: DebootstrapPolicy {
                transport: DebootstrapTransportPolicy::HttpsOnly,
                release_signatures: DebootstrapReleaseSignaturePolicy::Required,
                ..DebootstrapPolicy::default()
            },
        };
        assert_eq!(
            spec.args(Path::new("/var/lib/machines/test"), true)
                .unwrap(),
            vec![
                "--arch=amd64",
                "--include=systemd-sysv,libpam-systemd,dbus,dbus-user-session,sudo,zsh",
                "--exclude=nano",
                "--extra-suites=bookworm-updates",
                "--variant=minbase",
                "--components=main,contrib",
                "--merged-usr",
                "--no-resolve-deps",
                "--log-extra-deps",
                "--force-check-sig",
                "bookworm",
                "/var/lib/machines/test",
                "https://deb.debian.org/debian"
            ]
        );
    }

    #[test]
    fn debootstrap_rejects_untrusted_mirror_host() {
        let spec = DebootstrapSpec {
            suite: "bookworm".into(),
            mirror: Some("https://evil.example/debian".into()),
            policy: DebootstrapPolicy {
                allowed_mirror_hosts: vec!["deb.debian.org".into()],
                ..DebootstrapPolicy::default()
            },
            ..DebootstrapSpec::default()
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn pacstrap_policy_maps_host_integration_modes() {
        let spec = PacstrapSpec {
            packages: vec!["zsh".into()],
            cache: PacstrapCacheMode::Target,
            isolation: PacstrapIsolationMode::Unshare,
            dependency_checks: PacstrapDependencyMode::SkipChecks,
            policy: PacstrapPolicy {
                keyring: PacmanKeyringMode::InitializeEmpty,
                mirrorlist: PacmanMirrorlistMode::DoNotCopy,
                pacman_config: PacstrapPacmanConfigMode::CopyHost,
            },
        };
        assert_eq!(
            spec.args(Path::new("/var/lib/machines/test"), false)
                .unwrap(),
            vec![
                "-N",
                "-D",
                "-P",
                "-K",
                "-M",
                "/var/lib/machines/test",
                "base",
                "zsh"
            ]
        );
    }

    #[test]
    fn dnf5_named_spec_requires_host_repository_mode() {
        let spec = Dnf5Spec {
            releasever: "43".into(),
            packages: vec!["systemd".into()],
            ..Dnf5Spec::default()
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn dnf5_package_signature_policy_maps_only_to_package_gpg_switch() {
        let spec = Dnf5Spec {
            releasever: "43".into(),
            architecture: Some("x86_64".into()),
            packages: vec!["systemd".into()],
            exclude_packages: vec!["kernel-debug*".into()],
            only_repositories: vec!["fedora".into(), "updates".into()],
            metadata: Dnf5MetadataMode::Refresh,
            repository: Dnf5RepositorySource::Host,
            policy: Dnf5Policy {
                package_signatures: Dnf5PackageSignaturePolicy::Disabled,
                weak_dependencies: Dnf5WeakDependencyPolicy::Disabled,
                documentation: Dnf5DocumentationPolicy::Exclude,
                best_candidate: Dnf5BestCandidatePolicy::Required,
            },
            ..Dnf5Spec::default()
        };
        let args = spec
            .args(Path::new("/var/lib/machines/test"), false)
            .unwrap();
        assert_eq!(
            args,
            vec![
                "--installroot",
                "/var/lib/machines/test",
                "--releasever",
                "43",
                "--forcearch=x86_64",
                "--use-host-config",
                "--refresh",
                "--repo=fedora,updates",
                "--exclude=kernel-debug*",
                "--no-gpgchecks",
                "--setopt=install_weak_deps=False",
                "--no-docs",
                "--best",
                "--assumeyes",
                "install",
                "systemd",
                "systemd-pam",
                "dbus",
                "shadow-utils",
                "util-linux",
                "dnf5",
                "systemd-networkd",
                "systemd-resolved"
            ]
        );
    }

    #[test]
    fn provider_baselines_allow_empty_additional_packages_and_remove_duplicates() {
        let dnf = Dnf5Spec {
            releasever: "44".into(),
            packages: vec!["systemd-pam".into(), "zsh".into(), "sudo".into()],
            repository: Dnf5RepositorySource::Host,
            ..Dnf5Spec::default()
        };
        let dnf_args = dnf.args(Path::new("/var/lib/machines/test"), true).unwrap();
        assert_eq!(
            dnf_args.iter().filter(|arg| *arg == "systemd-pam").count(),
            1
        );
        assert_eq!(dnf_args.iter().filter(|arg| *arg == "sudo").count(), 1);
        assert!(dnf_args.iter().any(|arg| arg == "zsh"));

        let debootstrap = DebootstrapSpec {
            suite: "resolute".into(),
            ..DebootstrapSpec::default()
        };
        assert!(debootstrap
            .args(Path::new("/var/lib/machines/test"), false)
            .unwrap()
            .iter()
            .any(|arg| { arg == "--include=systemd-sysv,libpam-systemd,dbus,dbus-user-session" }));

        let empty_dnf = Dnf5Spec {
            releasever: "44".into(),
            repository: Dnf5RepositorySource::Host,
            ..Dnf5Spec::default()
        };
        assert!(empty_dnf.validate().is_ok());
    }

    #[test]
    fn dnf5_rejects_conflicting_repository_selection_modes() {
        let spec = Dnf5Spec {
            releasever: "43".into(),
            packages: vec!["systemd".into()],
            repository: Dnf5RepositorySource::Host,
            only_repositories: vec!["fedora".into()],
            enable_repositories: vec!["updates".into()],
            ..Dnf5Spec::default()
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn debootstrap_legacy_signature_spelling_is_typed() {
        let spec = DebootstrapSpec {
            suite: "bookworm".into(),
            policy: DebootstrapPolicy {
                release_signatures: DebootstrapReleaseSignaturePolicy::Required,
                ..DebootstrapPolicy::default()
            },
            ..DebootstrapSpec::default()
        };
        let args = spec
            .args_with_signature_style(
                Path::new("/var/lib/machines/test"),
                false,
                DebootstrapSignatureOptionStyle::Gpg,
            )
            .unwrap();
        assert!(args.iter().any(|arg| arg == "--force-check-gpg"));
    }

    #[test]
    fn option_shaped_packages_are_rejected() {
        let spec = PacstrapSpec {
            packages: vec!["--config=/tmp/host-pacman.conf".into()],
            ..PacstrapSpec::default()
        };
        assert!(spec.validate().is_err());
    }

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
    fn each_command_profile_exposes_its_frontend_dependency() {
        assert_eq!(
            RootfsSourceSpec::Dnf5(Dnf5Spec::default()).required_tool(),
            Some("dnf5")
        );
        assert_eq!(
            RootfsSourceSpec::Debootstrap(DebootstrapSpec::default()).required_tool(),
            Some("debootstrap")
        );
        assert_eq!(
            RootfsSourceSpec::Pacstrap(PacstrapSpec::default()).required_tool(),
            Some("pacstrap")
        );
        assert_eq!(
            RootfsSourceSpec::Artifact(ArtifactSpec::from_path("rootfs.tar")).required_tool(),
            None
        );
    }
}

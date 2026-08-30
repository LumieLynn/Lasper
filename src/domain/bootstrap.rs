//! Provider-neutral bootstrap intent values.
//!
//! These structures describe what should be installed and enforce the
//! provider-neutral invariants of that intent. Host command argument
//! construction remains in the provisioning adapter.

use crate::domain::source::{ArtifactSpec, ArtifactValidationError};
use serde::{Deserialize, Serialize};

pub const DEFAULT_BOOTSTRAP_PROFILE: &str = "default";

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default = "default_inherit_default_packages")]
    pub inherit_default_packages: bool,
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

impl Default for DebootstrapSpec {
    fn default() -> Self {
        Self {
            suite: String::new(),
            architecture: None,
            mirror: None,
            packages: Vec::new(),
            inherit_default_packages: true,
            exclude_packages: Vec::new(),
            extra_suites: Vec::new(),
            variant: None,
            components: Vec::new(),
            usr_merge: DebootstrapUsrMergeMode::default(),
            dependency_resolution: DebootstrapDependencyMode::default(),
            log_extra_dependencies: false,
            policy: DebootstrapPolicy::default(),
        }
    }
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacstrapSpec {
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default = "default_inherit_default_packages")]
    pub inherit_default_packages: bool,
    #[serde(default)]
    pub cache: PacstrapCacheMode,
    #[serde(default)]
    pub isolation: PacstrapIsolationMode,
    #[serde(default)]
    pub dependency_checks: PacstrapDependencyMode,
    #[serde(default)]
    pub policy: PacstrapPolicy,
}

impl Default for PacstrapSpec {
    fn default() -> Self {
        Self {
            packages: Vec::new(),
            inherit_default_packages: true,
            cache: PacstrapCacheMode::default(),
            isolation: PacstrapIsolationMode::default(),
            dependency_checks: PacstrapDependencyMode::default(),
            policy: PacstrapPolicy::default(),
        }
    }
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dnf5Spec {
    #[serde(default)]
    pub releasever: String,
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default = "default_inherit_default_packages")]
    pub inherit_default_packages: bool,
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

impl Default for Dnf5Spec {
    fn default() -> Self {
        Self {
            releasever: String::new(),
            architecture: None,
            packages: Vec::new(),
            inherit_default_packages: true,
            exclude_packages: Vec::new(),
            only_repositories: Vec::new(),
            enable_repositories: Vec::new(),
            disable_repositories: Vec::new(),
            metadata: Dnf5MetadataMode::default(),
            repository: Dnf5RepositorySource::default(),
            policy: Dnf5Policy::default(),
        }
    }
}

const fn default_inherit_default_packages() -> bool {
    true
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", content = "config", rename_all = "snake_case")]
pub enum BootstrapSpec {
    Debootstrap(DebootstrapSpec),
    Pacstrap(PacstrapSpec),
    Dnf5(Dnf5Spec),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BootstrapValidationError {
    #[error("Invalid bootstrap {field}")]
    InvalidToken { field: &'static str },
    #[error("Invalid bootstrap package '{0}'")]
    InvalidPackage(String),
    #[error("Invalid dnf5 repository selector '{0}'")]
    InvalidRepositorySelector(String),
    #[error("https-only bootstrap policy requires an explicit mirror")]
    HttpsMirrorRequired,
    #[error("Invalid bootstrap mirror: {0}")]
    InvalidMirror(String),
    #[error("Bootstrap mirror must use https")]
    InsecureMirror,
    #[error("Bootstrap mirror has no host")]
    MissingMirrorHost,
    #[error("Bootstrap mirror host '{0}' is not in the source allowlist")]
    MirrorHostNotAllowed(String),
    #[error("Bootstrap source allowlist requires an explicit mirror")]
    MirrorAllowlistRequiresMirror,
    #[error(
        "dnf5 requires repository=\"host\" until typed repository configuration is implemented"
    )]
    UnsupportedDnf5Repository,
    #[error(
        "dnf5 only_repositories cannot be combined with enable_repositories or disable_repositories"
    )]
    ConflictingDnf5RepositorySelection,
    #[error(transparent)]
    Artifact(#[from] ArtifactValidationError),
}

impl RootfsSourceSpec {
    pub fn validate(&self) -> Result<(), BootstrapValidationError> {
        match self {
            Self::Debootstrap(spec) => spec.validate(),
            Self::Pacstrap(spec) => spec.validate(),
            Self::Dnf5(spec) => spec.validate(),
            Self::Artifact(spec) => spec.validate().map_err(Into::into),
        }
    }

    /// Validate a partial preset for the wizard's editable `default` form.
    pub fn validate_default_preset(&self) -> Result<(), BootstrapValidationError> {
        match self {
            Self::Debootstrap(spec) => spec.validate_default_preset(),
            Self::Pacstrap(spec) => spec.validate(),
            Self::Dnf5(spec) => spec.validate_default_preset(),
            Self::Artifact(spec) => spec.validate().map_err(Into::into),
        }
    }

    pub fn is_external_storage(&self) -> bool {
        matches!(self, Self::Artifact(spec) if spec.is_external_storage())
    }

    pub fn required_tool(&self) -> Option<&'static str> {
        match self {
            Self::Debootstrap(_) => Some("debootstrap"),
            Self::Pacstrap(_) => Some("pacstrap"),
            Self::Dnf5(_) => Some("dnf5"),
            Self::Artifact(_) => None,
        }
    }
}

impl BootstrapSpec {
    pub fn validate(&self) -> Result<(), BootstrapValidationError> {
        match self {
            Self::Debootstrap(spec) => spec.validate(),
            Self::Pacstrap(spec) => spec.validate(),
            Self::Dnf5(spec) => spec.validate(),
        }
    }

    pub fn inherits_default_packages(&self) -> bool {
        match self {
            Self::Debootstrap(spec) => spec.inherit_default_packages,
            Self::Pacstrap(spec) => spec.inherit_default_packages,
            Self::Dnf5(spec) => spec.inherit_default_packages,
        }
    }
}

impl DebootstrapSpec {
    pub fn validate(&self) -> Result<(), BootstrapValidationError> {
        validate_token("suite", &self.suite)?;
        self.validate_optional_fields()
    }

    fn validate_default_preset(&self) -> Result<(), BootstrapValidationError> {
        if !self.suite.is_empty() {
            validate_token("suite", &self.suite)?;
        }
        self.validate_optional_fields()
    }

    fn validate_optional_fields(&self) -> Result<(), BootstrapValidationError> {
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
}

impl PacstrapSpec {
    pub fn validate(&self) -> Result<(), BootstrapValidationError> {
        for package in &self.packages {
            validate_package(package)?;
        }
        Ok(())
    }
}

impl Dnf5Spec {
    pub fn validate(&self) -> Result<(), BootstrapValidationError> {
        validate_token("releasever", &self.releasever)?;
        if self.repository != Dnf5RepositorySource::Host {
            return Err(BootstrapValidationError::UnsupportedDnf5Repository);
        }
        self.validate_optional_fields()
    }

    fn validate_default_preset(&self) -> Result<(), BootstrapValidationError> {
        if !self.releasever.is_empty() {
            validate_token("releasever", &self.releasever)?;
        }
        self.validate_optional_fields()
    }

    fn validate_optional_fields(&self) -> Result<(), BootstrapValidationError> {
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
            return Err(BootstrapValidationError::ConflictingDnf5RepositorySelection);
        }
        Ok(())
    }
}

fn validate_debootstrap_policy(
    policy: &DebootstrapPolicy,
    mirror: Option<&str>,
) -> Result<(), BootstrapValidationError> {
    if policy.transport == DebootstrapTransportPolicy::HttpsOnly && mirror.is_none() {
        return Err(BootstrapValidationError::HttpsMirrorRequired);
    }
    if let Some(mirror) = mirror {
        let parsed = url::Url::parse(mirror)
            .map_err(|error| BootstrapValidationError::InvalidMirror(error.to_string()))?;
        if policy.transport == DebootstrapTransportPolicy::HttpsOnly && parsed.scheme() != "https" {
            return Err(BootstrapValidationError::InsecureMirror);
        }
        if !policy.allowed_mirror_hosts.is_empty() {
            let host = parsed
                .host_str()
                .ok_or(BootstrapValidationError::MissingMirrorHost)?;
            if !policy
                .allowed_mirror_hosts
                .iter()
                .any(|allowed| allowed == host)
            {
                return Err(BootstrapValidationError::MirrorHostNotAllowed(
                    host.to_string(),
                ));
            }
        }
    } else if !policy.allowed_mirror_hosts.is_empty() {
        return Err(BootstrapValidationError::MirrorAllowlistRequiresMirror);
    }
    Ok(())
}

fn validate_token(field: &'static str, value: &str) -> Result<(), BootstrapValidationError> {
    if value.is_empty()
        || value.len() > 128
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._:+/@".contains(&byte))
    {
        return Err(BootstrapValidationError::InvalidToken { field });
    }
    Ok(())
}

fn validate_package(value: &str) -> Result<(), BootstrapValidationError> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with('-')
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
        || value
            .chars()
            .any(|c| matches!(c, ';' | '|' | '&' | '`' | '\'' | '"'))
    {
        return Err(BootstrapValidationError::InvalidPackage(value.to_string()));
    }
    Ok(())
}

fn validate_repository_selector(value: &str) -> Result<(), BootstrapValidationError> {
    if value.is_empty()
        || value.len() > 128
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._*?".contains(&byte))
    {
        return Err(BootstrapValidationError::InvalidRepositorySelector(
            value.to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_validation_is_owned_by_the_intent_model() {
        let invalid_package = PacstrapSpec {
            packages: vec!["--config=/tmp/host-pacman.conf".into()],
            ..PacstrapSpec::default()
        };
        assert!(matches!(
            invalid_package.validate(),
            Err(BootstrapValidationError::InvalidPackage(_))
        ));

        let missing_repository = Dnf5Spec {
            releasever: "43".into(),
            ..Dnf5Spec::default()
        };
        assert_eq!(
            missing_repository.validate(),
            Err(BootstrapValidationError::UnsupportedDnf5Repository)
        );
    }

    #[test]
    fn source_metadata_does_not_depend_on_a_host_adapter() {
        assert_eq!(
            RootfsSourceSpec::Debootstrap(DebootstrapSpec::default()).required_tool(),
            Some("debootstrap")
        );
        assert!(
            RootfsSourceSpec::Artifact(ArtifactSpec::from_path("rootfs.raw")).is_external_storage()
        );
    }
}

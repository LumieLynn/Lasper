//! Provider-neutral bootstrap intent values.
//!
//! These structures describe what should be installed. Provider command
//! validation and argument construction remain in the nspawn adapter while
//! this module owns the serialized provisioning data.

use crate::domain::source::ArtifactSpec;
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

//! Host command projections for typed bootstrap intent.

use crate::domain::bootstrap::{
    BootstrapValidationError, DebootstrapDependencyMode, DebootstrapReleaseSignaturePolicy,
    DebootstrapSpec, DebootstrapUsrMergeMode, Dnf5BestCandidatePolicy, Dnf5DocumentationPolicy,
    Dnf5MetadataMode, Dnf5PackageSignaturePolicy, Dnf5RepositorySource, Dnf5Spec,
    Dnf5WeakDependencyPolicy, PacmanKeyringMode, PacmanMirrorlistMode, PacstrapCacheMode,
    PacstrapDependencyMode, PacstrapIsolationMode, PacstrapPacmanConfigMode, PacstrapSpec,
};
use crate::nspawn::errors::{NspawnError, Result};
use std::path::Path;

/// Provider defaults for a bootable systemd-nspawn guest. Profiles inherit
/// these packages unless `inherit_default_packages` is disabled.
pub const DEBOOTSTRAP_DEFAULT_PACKAGES: &[&str] = &[
    "systemd-sysv",
    "libpam-systemd",
    "dbus",
    "dbus-user-session",
    "systemd-resolved",
];
pub const PACSTRAP_DEFAULT_PACKAGES: &[&str] = &["base"];
pub const DNF5_DEFAULT_PACKAGES: &[&str] = &[
    "systemd",
    "systemd-pam",
    "dbus",
    "shadow-utils",
    "util-linux",
    "dnf5",
    "systemd-networkd",
    "systemd-resolved",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DebootstrapSignatureOptionStyle {
    Sig,
    Gpg,
}

#[cfg(test)]
fn debootstrap_args(
    spec: &DebootstrapSpec,
    target: &Path,
    include_sudo: bool,
) -> Result<Vec<String>> {
    debootstrap_args_with_signature_style(
        spec,
        target,
        include_sudo,
        DebootstrapSignatureOptionStyle::Sig,
    )
}

pub(crate) fn debootstrap_args_with_signature_style(
    spec: &DebootstrapSpec,
    target: &Path,
    include_sudo: bool,
    signature_style: DebootstrapSignatureOptionStyle,
) -> Result<Vec<String>> {
    validate_intent(spec.validate())?;
    let mut args = Vec::new();
    if let Some(architecture) = &spec.architecture {
        args.push(format!("--arch={architecture}"));
    }
    let packages = effective_packages(
        DEBOOTSTRAP_DEFAULT_PACKAGES,
        spec.inherit_default_packages,
        include_sudo,
        &spec.packages,
    );
    if !packages.is_empty() {
        args.push(format!("--include={}", packages.join(",")));
    }
    if !spec.exclude_packages.is_empty() {
        args.push(format!("--exclude={}", spec.exclude_packages.join(",")));
    }
    if !spec.extra_suites.is_empty() {
        args.push(format!("--extra-suites={}", spec.extra_suites.join(",")));
    }
    if let Some(variant) = &spec.variant {
        args.push(format!("--variant={variant}"));
    }
    if !spec.components.is_empty() {
        args.push(format!("--components={}", spec.components.join(",")));
    }
    match spec.usr_merge {
        DebootstrapUsrMergeMode::ProviderDefault => {}
        DebootstrapUsrMergeMode::Merged => args.push("--merged-usr".into()),
        DebootstrapUsrMergeMode::Unmerged => args.push("--no-merged-usr".into()),
    }
    if spec.dependency_resolution == DebootstrapDependencyMode::SkipResolution {
        args.push("--no-resolve-deps".into());
    }
    if spec.log_extra_dependencies {
        args.push("--log-extra-deps".into());
    }
    match spec.policy.release_signatures {
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
    args.push(spec.suite.clone());
    args.push(target.to_string_lossy().into_owned());
    if let Some(mirror) = &spec.mirror {
        args.push(mirror.clone());
    }
    Ok(args)
}

pub(crate) fn pacstrap_args(
    spec: &PacstrapSpec,
    target: &Path,
    include_sudo: bool,
) -> Result<Vec<String>> {
    validate_intent(spec.validate())?;
    let mut args = Vec::new();
    if spec.cache == PacstrapCacheMode::Host {
        args.push("-c".into());
    }
    if spec.isolation == PacstrapIsolationMode::Unshare {
        args.push("-N".into());
    }
    if spec.dependency_checks == PacstrapDependencyMode::SkipChecks {
        args.push("-D".into());
    }
    if spec.policy.pacman_config == PacstrapPacmanConfigMode::CopyHost {
        args.push("-P".into());
    }
    match spec.policy.keyring {
        PacmanKeyringMode::CopyHost => {}
        PacmanKeyringMode::DoNotCopy => args.push("-G".into()),
        PacmanKeyringMode::InitializeEmpty => args.push("-K".into()),
    }
    if spec.policy.mirrorlist == PacmanMirrorlistMode::DoNotCopy {
        args.push("-M".into());
    }
    let packages = effective_packages(
        PACSTRAP_DEFAULT_PACKAGES,
        spec.inherit_default_packages,
        include_sudo,
        &spec.packages,
    );
    if packages.is_empty() {
        return Err(validation(
                "pacstrap requires at least one effective package when default package inheritance is disabled; an empty pacstrap invocation would install base implicitly",
            ));
    }
    args.push(target.to_string_lossy().into_owned());
    args.extend(packages);
    Ok(args)
}

pub(crate) fn dnf5_args(spec: &Dnf5Spec, target: &Path, include_sudo: bool) -> Result<Vec<String>> {
    validate_intent(spec.validate())?;
    let mut args = vec![
        "--installroot".into(),
        target.to_string_lossy().into_owned(),
        "--releasever".into(),
        spec.releasever.clone(),
    ];
    if let Some(architecture) = &spec.architecture {
        args.push(format!("--forcearch={architecture}"));
    }
    if spec.repository == Dnf5RepositorySource::Host {
        args.push("--use-host-config".into());
    }
    match spec.metadata {
        Dnf5MetadataMode::ProviderDefault => {}
        Dnf5MetadataMode::Refresh => args.push("--refresh".into()),
        Dnf5MetadataMode::CacheOnly => args.push("--cacheonly".into()),
    }
    if !spec.only_repositories.is_empty() {
        args.push(format!("--repo={}", spec.only_repositories.join(",")));
    }
    if !spec.enable_repositories.is_empty() {
        args.push(format!(
            "--enable-repo={}",
            spec.enable_repositories.join(",")
        ));
    }
    if !spec.disable_repositories.is_empty() {
        args.push(format!(
            "--disable-repo={}",
            spec.disable_repositories.join(",")
        ));
    }
    if !spec.exclude_packages.is_empty() {
        args.push(format!("--exclude={}", spec.exclude_packages.join(",")));
    }
    if spec.policy.package_signatures == Dnf5PackageSignaturePolicy::Disabled {
        args.push("--no-gpgchecks".into());
    }
    match spec.policy.weak_dependencies {
        Dnf5WeakDependencyPolicy::ProviderDefault => {}
        Dnf5WeakDependencyPolicy::Enabled => args.push("--setopt=install_weak_deps=True".into()),
        Dnf5WeakDependencyPolicy::Disabled => args.push("--setopt=install_weak_deps=False".into()),
    }
    if spec.policy.documentation == Dnf5DocumentationPolicy::Exclude {
        args.push("--no-docs".into());
    }
    match spec.policy.best_candidate {
        Dnf5BestCandidatePolicy::ProviderDefault => {}
        Dnf5BestCandidatePolicy::Required => args.push("--best".into()),
        Dnf5BestCandidatePolicy::AllowOlder => args.push("--no-best".into()),
    }
    let packages = effective_packages(
        DNF5_DEFAULT_PACKAGES,
        spec.inherit_default_packages,
        include_sudo,
        &spec.packages,
    );
    if packages.is_empty() {
        return Err(validation(
                "dnf5 requires at least one effective package when default package inheritance is disabled",
            ));
    }
    args.push("--assumeyes".into());
    args.push("install".into());
    args.extend(packages);
    Ok(args)
}

fn effective_packages(
    defaults: &[&str],
    inherit_defaults: bool,
    include_sudo: bool,
    additional: &[String],
) -> Vec<String> {
    let mut packages =
        Vec::with_capacity(defaults.len() + additional.len() + usize::from(include_sudo));
    for package in defaults
        .iter()
        .copied()
        .filter(|_| inherit_defaults)
        .chain(include_sudo.then_some("sudo"))
        .chain(additional.iter().map(String::as_str))
    {
        if !packages.iter().any(|existing| existing == package) {
            packages.push(package.to_string());
        }
    }
    packages
}

fn validate_intent(result: std::result::Result<(), BootstrapValidationError>) -> Result<()> {
    result.map_err(|error| validation(error.to_string()))
}

fn validation(message: impl Into<String>) -> NspawnError {
    NspawnError::Validation(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::bootstrap::{
        DebootstrapPolicy, DebootstrapTransportPolicy, Dnf5Policy, PacstrapPolicy,
    };
    use std::path::Path;

    #[test]
    fn debootstrap_args_are_typed_and_ordered() {
        let spec = DebootstrapSpec {
            suite: "bookworm".into(),
            architecture: Some("amd64".into()),
            mirror: Some("https://deb.debian.org/debian".into()),
            packages: vec!["zsh".into()],
            inherit_default_packages: true,
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
            debootstrap_args(&spec, Path::new("/var/lib/machines/test"), true).unwrap(),
            vec![
                "--arch=amd64",
                "--include=systemd-sysv,libpam-systemd,dbus,dbus-user-session,systemd-resolved,sudo,zsh",
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
    fn pacstrap_policy_maps_host_integration_modes() {
        let spec = PacstrapSpec {
            packages: vec!["zsh".into()],
            inherit_default_packages: true,
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
            pacstrap_args(&spec, Path::new("/var/lib/machines/test"), false).unwrap(),
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
        let args = dnf5_args(&spec, Path::new("/var/lib/machines/test"), false).unwrap();
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
    fn provider_defaults_allow_empty_additional_packages_and_remove_duplicates() {
        let dnf = Dnf5Spec {
            releasever: "44".into(),
            packages: vec!["systemd-pam".into(), "zsh".into(), "sudo".into()],
            repository: Dnf5RepositorySource::Host,
            ..Dnf5Spec::default()
        };
        let dnf_args = dnf5_args(&dnf, Path::new("/var/lib/machines/test"), true).unwrap();
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
        assert!(debootstrap_args(
            &debootstrap,
            Path::new("/var/lib/machines/test"),
            false
        )
            .unwrap()
            .iter()
            .any(|arg| {
                arg == "--include=systemd-sysv,libpam-systemd,dbus,dbus-user-session,systemd-resolved"
            }));
    }

    #[test]
    fn provider_defaults_can_be_disabled_without_dropping_feature_packages() {
        let debootstrap = DebootstrapSpec {
            suite: "bullseye".into(),
            packages: vec!["systemd-sysv".into(), "dbus".into()],
            inherit_default_packages: false,
            ..DebootstrapSpec::default()
        };
        let debootstrap_args =
            debootstrap_args(&debootstrap, Path::new("/var/lib/machines/test"), true).unwrap();
        assert!(debootstrap_args
            .iter()
            .any(|arg| arg == "--include=sudo,systemd-sysv,dbus"));
        assert!(debootstrap_args
            .iter()
            .all(|arg| !arg.contains("systemd-resolved")));

        let pacstrap = PacstrapSpec {
            packages: vec!["zsh".into()],
            inherit_default_packages: false,
            ..PacstrapSpec::default()
        };
        let pacstrap_args =
            pacstrap_args(&pacstrap, Path::new("/var/lib/machines/test"), false).unwrap();
        assert!(pacstrap_args.iter().any(|arg| arg == "zsh"));
        assert!(pacstrap_args.iter().all(|arg| arg != "base"));

        let dnf = Dnf5Spec {
            releasever: "43".into(),
            packages: vec!["fedora-release-container".into()],
            inherit_default_packages: false,
            repository: Dnf5RepositorySource::Host,
            ..Dnf5Spec::default()
        };
        let dnf_args = dnf5_args(&dnf, Path::new("/var/lib/machines/test"), false).unwrap();
        assert!(dnf_args.iter().any(|arg| arg == "fedora-release-container"));
        assert!(dnf_args.iter().all(|arg| arg != "systemd"));
    }

    #[test]
    fn debootstrap_omits_empty_include_when_defaults_are_disabled() {
        let spec = DebootstrapSpec {
            suite: "bullseye".into(),
            inherit_default_packages: false,
            ..DebootstrapSpec::default()
        };
        let args = debootstrap_args(&spec, Path::new("/var/lib/machines/test"), false).unwrap();
        assert!(args.iter().all(|arg| !arg.starts_with("--include=")));
    }

    #[test]
    fn providers_reject_empty_effective_install_sets_without_reintroducing_defaults() {
        let spec = PacstrapSpec {
            inherit_default_packages: false,
            ..PacstrapSpec::default()
        };
        let error = pacstrap_args(&spec, Path::new("/var/lib/machines/test"), false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("install base implicitly"));

        let spec = Dnf5Spec {
            releasever: "43".into(),
            inherit_default_packages: false,
            repository: Dnf5RepositorySource::Host,
            ..Dnf5Spec::default()
        };
        let error = dnf5_args(&spec, Path::new("/var/lib/machines/test"), false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("at least one effective package"));
    }

    #[test]
    fn feature_packages_make_an_empty_explicit_set_installable() {
        let pacstrap = PacstrapSpec {
            inherit_default_packages: false,
            ..PacstrapSpec::default()
        };
        let args = pacstrap_args(&pacstrap, Path::new("/var/lib/machines/test"), true).unwrap();
        assert!(args.iter().any(|arg| arg == "sudo"));
        assert!(args.iter().all(|arg| arg != "base"));

        let dnf = Dnf5Spec {
            releasever: "43".into(),
            inherit_default_packages: false,
            repository: Dnf5RepositorySource::Host,
            ..Dnf5Spec::default()
        };
        let args = dnf5_args(&dnf, Path::new("/var/lib/machines/test"), true).unwrap();
        assert!(args.iter().any(|arg| arg == "sudo"));
        assert!(args.iter().all(|arg| arg != "systemd"));
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
        let args = debootstrap_args_with_signature_style(
            &spec,
            Path::new("/var/lib/machines/test"),
            false,
            DebootstrapSignatureOptionStyle::Gpg,
        )
        .unwrap();
        assert!(args.iter().any(|arg| arg == "--force-check-gpg"));
    }
}

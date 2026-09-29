//! systemd-nspawn settings-source resolution.
//!
//! This module owns the host-side search policy for `.nspawn` files. It is
//! separate from `nspawn_file`, which parses and renders file contents, and
//! from `store`, which selects a direct or elevated execution route.

use std::path::{Path, PathBuf};

use crate::domain::machine::MachineName;
use crate::domain::runtime::ImageName;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NspawnConfigSourceKind {
    Administrator,
    Runtime,
    ImageAdjacent,
}

/// One candidate in systemd-nspawn's settings search order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NspawnConfigSource {
    kind: NspawnConfigSourceKind,
    path: PathBuf,
}

impl NspawnConfigSource {
    fn new(kind: NspawnConfigSourceKind, path: PathBuf) -> Self {
        Self { kind, path }
    }

    pub(crate) const fn kind(&self) -> NspawnConfigSourceKind {
        self.kind
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Settings candidates for a validated machine or image name.
///
/// The effective name, rather than an image filename, determines the
/// `.nspawn` basename. The image-adjacent candidate is included only for
/// image inspection; machine inspection and writes use trusted candidates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NspawnConfigSources {
    candidates: Vec<NspawnConfigSource>,
}

impl NspawnConfigSources {
    pub(crate) fn for_machine(machine: &MachineName) -> Self {
        Self::from_roots(
            machine.as_str(),
            false,
            crate::paths::nspawn_config_dir(),
            crate::paths::nspawn_runtime_config_dir(),
            crate::paths::machines_dir(),
        )
    }

    pub(crate) fn for_image(image: &ImageName) -> Self {
        Self::from_roots(
            image.as_str(),
            true,
            crate::paths::nspawn_config_dir(),
            crate::paths::nspawn_runtime_config_dir(),
            crate::paths::machines_dir(),
        )
    }

    #[cfg(test)]
    pub(crate) fn from_test_roots(
        name: &str,
        image_adjacent: bool,
        administrator: &Path,
        runtime: &Path,
        machines: &Path,
    ) -> Self {
        Self::from_roots(
            name,
            image_adjacent,
            administrator.to_path_buf(),
            runtime.to_path_buf(),
            machines.to_path_buf(),
        )
    }

    fn from_roots(
        name: &str,
        image_adjacent: bool,
        administrator: PathBuf,
        runtime: PathBuf,
        machines: PathBuf,
    ) -> Self {
        let filename = format!("{name}.nspawn");
        let mut candidates = vec![
            NspawnConfigSource::new(
                NspawnConfigSourceKind::Administrator,
                administrator.join(&filename),
            ),
            NspawnConfigSource::new(NspawnConfigSourceKind::Runtime, runtime.join(&filename)),
        ];
        if image_adjacent {
            candidates.push(NspawnConfigSource::new(
                NspawnConfigSourceKind::ImageAdjacent,
                machines.join(filename),
            ));
        }
        Self { candidates }
    }

    pub(crate) fn candidates(&self) -> &[NspawnConfigSource] {
        &self.candidates
    }

    pub(crate) fn paths(&self) -> impl Iterator<Item = &Path> {
        self.candidates.iter().map(NspawnConfigSource::path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_sources_only_include_trusted_candidates() {
        let sources = NspawnConfigSources::from_test_roots(
            "arch",
            false,
            Path::new("/etc/systemd/nspawn"),
            Path::new("/run/systemd/nspawn"),
            Path::new("/var/lib/machines"),
        );
        assert_eq!(sources.candidates().len(), 2);
        assert!(sources.candidates().iter().all(|source| matches!(
            source.kind(),
            NspawnConfigSourceKind::Administrator | NspawnConfigSourceKind::Runtime
        )));
    }

    #[test]
    fn image_sources_preserve_systemd_search_order() {
        let sources = NspawnConfigSources::from_test_roots(
            "arch",
            true,
            Path::new("/etc/systemd/nspawn"),
            Path::new("/run/systemd/nspawn"),
            Path::new("/var/lib/machines"),
        );
        assert_eq!(
            sources.paths().collect::<Vec<_>>(),
            vec![
                Path::new("/etc/systemd/nspawn/arch.nspawn"),
                Path::new("/run/systemd/nspawn/arch.nspawn"),
                Path::new("/var/lib/machines/arch.nspawn"),
            ]
        );
    }
}

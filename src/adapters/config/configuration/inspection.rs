//! Bounded file discovery for Configure. This records what was actually read;
//! it does not resolve arbitrary nspawn command lines or infer an image from a
//! running machine's name. No inspection creates directories or lock files.

use std::fs::{Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::projection::project;
use crate::adapters::config::nspawn_file::NspawnConfig;
use crate::adapters::config::MAX_NSPAWN_CONTENT_BYTES;
use crate::adapters::error::{NspawnError, Result};
use crate::application::configuration::{
    ConfigurationCandidate, ConfigurationCandidateState, ConfigurationOrigin,
    ConfigurationRevision, ConfigurationSnapshot, ConfigurationTarget, ConfigurationWriteTarget,
};
use crate::domain::machine::MachineName;

pub(crate) async fn inspect(target: ConfigurationTarget) -> Result<ConfigurationSnapshot> {
    tokio::task::spawn_blocking(move || inspect_now(target))
        .await
        .map_err(|error| NspawnError::Runtime(format!("configuration inspection failed: {error}")))
}

pub(super) fn inspect_now(target: ConfigurationTarget) -> ConfigurationSnapshot {
    inspect_at(
        target,
        Path::new("/etc/systemd/nspawn"),
        Path::new("/run/systemd/nspawn"),
        &crate::paths::machines_dir(),
    )
}

pub(super) fn inspect_at(
    target: ConfigurationTarget,
    admin: &Path,
    runtime: &Path,
    images: &Path,
) -> ConfigurationSnapshot {
    let filename = format!("{}.nspawn", target.name());
    let mut locations = vec![
        (admin.join(&filename), ConfigurationOrigin::Administrator),
        (runtime.join(&filename), ConfigurationOrigin::Runtime),
    ];
    if matches!(target, ConfigurationTarget::Image(_)) {
        locations.push((images.join(&filename), ConfigurationOrigin::ImageAdjacent));
    }
    let writable_name = MachineName::new(target.name()).is_ok();
    let mut candidates = Vec::with_capacity(locations.len());
    let mut read_source = None;
    let mut write_target_revision = None;
    let mut write_target = None;
    let mut selected = None;
    let mut selected_origin = None;
    let mut failure = None;
    for (index, (path, origin)) in locations.into_iter().enumerate() {
        let state = if selected.is_some() || failure.is_some() {
            ConfigurationCandidateState::NotConsulted
        } else {
            match read_file(&path) {
                Ok(Some(file)) => {
                    if index == 0 && writable_name {
                        write_target_revision = Some(file.revision.clone());
                        write_target = Some(ConfigurationWriteTarget {
                            path: path.clone(),
                            exists: true,
                        });
                    }
                    read_source = Some(file.revision);
                    selected = Some(NspawnConfig {
                        path: path.clone(),
                        content: file.content,
                    });
                    selected_origin = Some(origin);
                    ConfigurationCandidateState::Selected
                }
                Ok(None) => {
                    if index == 0 && writable_name {
                        write_target_revision = Some(absence_revision(&path));
                        write_target = Some(ConfigurationWriteTarget {
                            path: path.clone(),
                            exists: false,
                        });
                    }
                    ConfigurationCandidateState::Absent
                }
                Err(error) => {
                    let reason = error.to_string();
                    failure = Some(format!("Cannot inspect {}: {reason}", path.display()));
                    ConfigurationCandidateState::Unavailable(reason)
                }
            }
        };
        candidates.push(ConfigurationCandidate {
            path,
            origin,
            state,
        });
    }
    let mut snapshot = project(target, selected);
    if let Some(document) = &mut snapshot.document {
        document.origin = selected_origin.expect("selected file has a source origin");
    }
    snapshot.write_target = write_target;
    if let Some(failure) = failure {
        snapshot.diagnostics.push(failure);
    } else {
        snapshot.revision = Some(ConfigurationRevision {
            discovery: discovery_revision(&snapshot.target, &candidates),
            read_source,
            write_target: write_target_revision,
        });
    }
    snapshot.candidates = candidates;
    snapshot
}

fn discovery_revision(
    target: &ConfigurationTarget,
    candidates: &[ConfigurationCandidate],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"lasper-nspawn-file-search-v1\0");
    digest.update([match target {
        ConfigurationTarget::Machine(_) => 0,
        ConfigurationTarget::Image(_) => 1,
    }]);
    hash_field(&mut digest, target.name().as_bytes());
    for candidate in candidates {
        hash_field(&mut digest, candidate.path.as_os_str().as_bytes());
        digest.update([match candidate.state {
            ConfigurationCandidateState::Absent => 0,
            ConfigurationCandidateState::Selected => 1,
            ConfigurationCandidateState::NotConsulted => 2,
            ConfigurationCandidateState::Unavailable(_) => 3,
        }]);
    }
    format!("{:x}", digest.finalize())
}

fn absence_revision(path: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(b"lasper-nspawn-absent-v1\0");
    hash_field(&mut digest, path.as_os_str().as_bytes());
    format!("{:x}", digest.finalize())
}

fn hash_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

struct FileSnapshot {
    content: String,
    revision: String,
}

// Exclude atime, which an ordinary inspection can itself change. Include the
// inode and ctime so replacement with identical bytes is still a new source.
fn metadata_revision(metadata: &Metadata) -> [u64; 10] {
    [
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.uid() as u64,
        metadata.gid() as u64,
        metadata.mode() as u64,
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
        metadata.ctime() as u64,
        metadata.ctime_nsec() as u64,
    ]
}

fn read_file(path: &Path) -> io::Result<Option<FileSnapshot>> {
    let mut file = match OpenOptions::new()
        .read(true)
        // O_NONBLOCK prevents a FIFO from hanging inspection before fstat.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration is not a regular file",
        ));
    }
    if before.len() > MAX_NSPAWN_CONTENT_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration exceeds the inspection size limit",
        ));
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_NSPAWN_CONTENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_NSPAWN_CONTENT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration exceeds the inspection size limit",
        ));
    }
    let after = file.metadata()?;
    let current = std::fs::symlink_metadata(path)?;
    let metadata = metadata_revision(&before);
    if metadata != metadata_revision(&after) || metadata != metadata_revision(&current) {
        return Err(io::Error::other(
            "configuration changed during inspection; refresh to retry",
        ));
    }
    let mut digest = Sha256::new();
    digest.update(b"lasper-nspawn-file-v1\0");
    hash_field(&mut digest, path.as_os_str().as_bytes());
    for value in metadata {
        digest.update(value.to_le_bytes());
    }
    hash_field(&mut digest, &bytes);
    Ok(Some(FileSnapshot {
        revision: format!("{:x}", digest.finalize()),
        content: String::from_utf8(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::ImageName;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
            }
        }

        fn path(&self, location: &str, name: &str) -> PathBuf {
            self.root
                .path()
                .join(location)
                .join(format!("{name}.nspawn"))
        }

        fn write(&self, location: &str, name: &str, content: &str) -> PathBuf {
            let path = self.path(location, name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            path
        }

        fn inspect(&self, target: ConfigurationTarget) -> ConfigurationSnapshot {
            inspect_at(
                target,
                &self.root.path().join("admin"),
                &self.root.path().join("runtime"),
                &self.root.path().join("images"),
            )
        }

        fn image(&self, name: &str) -> ConfigurationSnapshot {
            self.inspect(ConfigurationTarget::Image(ImageName::new(name).unwrap()))
        }
    }

    #[test]
    fn machine_search_includes_runtime_but_does_not_guess_the_source_image() {
        let fixture = Fixture::new();
        fixture.write("images", "renamed", "[Files]\nBind=/tmp/.X11-unix/X9\n");
        let target = ConfigurationTarget::Machine(MachineName::new("renamed").unwrap());
        let absent = fixture.inspect(target.clone());
        assert!(absent.document.is_none());
        assert_eq!(absent.candidates.len(), 2);
        let runtime = fixture.write("runtime", "renamed", "[Files]\nBind=/tmp/.X11-unix/X0\n");
        let selected = fixture.inspect(target);
        assert_eq!(selected.document.unwrap().path, runtime);
        assert_eq!(selected.x11_bindings.len(), 1);
        assert_eq!(
            selected.candidates[0].state,
            ConfigurationCandidateState::Absent
        );
        assert_eq!(
            selected.candidates[1].state,
            ConfigurationCandidateState::Selected
        );
    }

    #[test]
    fn source_and_write_target_revisions_change_independently_during_promotion() {
        let fixture = Fixture::new();
        fixture.write("images", "archlinux", "[Exec]\nBoot=yes\n");
        let first = fixture.image("archlinux");
        let before = first.revision.unwrap();
        assert!(!first.write_target.unwrap().exists);
        fixture.write("images", "archlinux", "[Exec]\nBoot=no\n");
        let second = fixture.image("archlinux").revision.unwrap();
        assert_eq!(before.discovery, second.discovery);
        assert_ne!(before.read_source, second.read_source);
        assert_eq!(before.write_target, second.write_target);
        fixture.write("runtime", "archlinux", "[Exec]\nBoot=no\n");
        let runtime = fixture.image("archlinux").revision.unwrap();
        assert_ne!(second.discovery, runtime.discovery);
        assert_eq!(second.write_target, runtime.write_target);
        fixture.write("admin", "archlinux", "[Exec]\nBoot=no\n");
        let final_snapshot = fixture.image("archlinux");
        assert!(final_snapshot.write_target.unwrap().exists);
        assert_eq!(
            final_snapshot.candidates[1].state,
            ConfigurationCandidateState::NotConsulted
        );
        let admin = final_snapshot.revision.unwrap();
        assert_ne!(admin.discovery, runtime.discovery);
        assert_ne!(admin.write_target, runtime.write_target);
        assert_eq!(admin.read_source, admin.write_target);
    }

    #[test]
    fn unchanged_bytes_do_not_hide_permission_changes_or_inode_replacement() {
        let fixture = Fixture::new();
        let path = fixture.write("admin", "archlinux", "[Exec]\nBoot=yes\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before = fixture.image("archlinux").revision.unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let chmod = fixture.image("archlinux").revision.unwrap();
        assert_ne!(before.read_source, chmod.read_source);
        let replacement = fixture.write("admin", "replacement", "[Exec]\nBoot=yes\n");
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(replacement, path).unwrap();
        let replaced = fixture.image("archlinux").revision.unwrap();
        assert_ne!(chmod.read_source, replaced.read_source);
        assert_eq!(before.discovery, replaced.discovery);
    }

    #[test]
    fn an_empty_configuration_is_present_and_masks_lower_priority_files() {
        let fixture = Fixture::new();
        fixture.write("runtime", "archlinux", "[Files]\nBind=/tmp/.X11-unix\n");
        let absent_target = fixture.image("archlinux").revision.unwrap().write_target;
        fixture.write("admin", "archlinux", "");
        let empty = fixture.image("archlinux");
        assert_eq!(empty.document.unwrap().content, "");
        assert!(empty.x11_bindings.is_empty());
        assert!(empty.write_target.unwrap().exists);
        assert_ne!(empty.revision.unwrap().write_target, absent_target);
    }

    #[test]
    fn unsupported_earlier_files_block_selection_instead_of_becoming_absence() {
        let fixture = Fixture::new();
        fixture.write("images", "archlinux", "[Exec]\nBoot=yes\n");
        std::fs::create_dir_all(fixture.root.path().join("admin")).unwrap();
        let path = fixture.path("admin", "archlinux");
        symlink(fixture.root.path().join("missing"), &path).unwrap();
        let blocked = fixture.image("archlinux");
        assert!(matches!(
            blocked.candidates[0].state,
            ConfigurationCandidateState::Unavailable(_)
        ));
        assert_eq!(
            blocked.candidates[2].state,
            ConfigurationCandidateState::NotConsulted
        );
        assert!(blocked.document.is_none());
        assert!(blocked.revision.is_none());
        std::fs::remove_file(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let blocked = fixture.image("archlinux");
        assert!(blocked.revision.is_none());
        assert!(blocked.diagnostics[0].contains("not a regular file"));
    }

    #[test]
    fn bounded_reader_rejects_oversized_and_non_utf8_sources() {
        let fixture = Fixture::new();
        let path = fixture.write("admin", "archlinux", "");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_NSPAWN_CONTENT_BYTES as u64 + 1)
            .unwrap();
        assert!(fixture.image("archlinux").revision.is_none());
        std::fs::write(path, [0xff]).unwrap();
        assert!(fixture.image("archlinux").revision.is_none());
    }

    #[test]
    fn inspection_does_not_create_paths_or_fabricate_a_writable_machine_name() {
        let fixture = Fixture::new();
        let absent = fixture.image("vendor image");
        assert!(absent.document.is_none());
        assert!(absent.write_target.is_none());
        assert!(absent.revision.unwrap().write_target.is_none());
        assert_eq!(std::fs::read_dir(fixture.root.path()).unwrap().count(), 0);
        fixture.write("images", "vendor image", "[Exec]\nBoot=yes\n");
        let present = fixture.image("vendor image");
        assert!(present.document.is_some());
        assert!(present.write_target.is_none());
    }

    #[test]
    fn snapshot_transport_keeps_source_evidence_and_redacts_debug_content() {
        let fixture = Fixture::new();
        fixture.write("runtime", "archlinux", "[Exec]\nEnvironment=secret-value\n");
        let snapshot = fixture.image("archlinux");
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let decoded: ConfigurationSnapshot = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.revision, snapshot.revision);
        assert_eq!(decoded.candidates, snapshot.candidates);
        assert_eq!(decoded.write_target, snapshot.write_target);
        assert_eq!(
            decoded.document.unwrap().origin,
            ConfigurationOrigin::Runtime
        );
        assert!(!format!("{snapshot:?}").contains("secret-value"));
    }
}

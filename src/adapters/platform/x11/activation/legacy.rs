//! Retire only runtime files bearing the previous Lasper unit marker. New
//! activations never create or change permissions on systemd's directories.

use crate::adapters::trusted_state::TrustedDirectory;

use super::super::transport::user_runtime_directory;
use super::UnitManager;

const PATH_UNIT: &str = "lasper-x11-machine.path";
const SERVICE_UNIT: &str = "lasper-x11-reconcile.service";
const MARKER: &[u8] = b"# Managed by Lasper: X11 system-machine reconcile v1\n";
const MAX_UNIT_BYTES: usize = 16 * 1024;

pub(super) struct LegacyUnits {
    directory: TrustedDirectory,
    owned: Vec<&'static str>,
}

impl LegacyUnits {
    pub(super) fn find() -> Result<Option<Self>, String> {
        let uid = uzers::get_effective_uid();
        let runtime = user_runtime_directory(uid)?;
        let runtime = TrustedDirectory::open_existing(&runtime, uid)
            .map_err(|error| format!("open user runtime directory: {error}"))?;
        let Some(systemd) = runtime
            .open_existing_child("systemd")
            .map_err(|error| format!("open systemd runtime directory: {error}"))?
        else {
            return Ok(None);
        };
        let Some(directory) = systemd
            .open_existing_child("user")
            .map_err(|error| format!("open user runtime unit directory: {error}"))?
        else {
            return Ok(None);
        };
        Self::in_directory(directory)
    }

    fn in_directory(directory: TrustedDirectory) -> Result<Option<Self>, String> {
        let mut owned = Vec::new();
        for name in [PATH_UNIT, SERVICE_UNIT] {
            if is_owned(&directory, name)? {
                owned.push(name);
            }
        }
        Ok((!owned.is_empty()).then_some(Self { directory, owned }))
    }

    pub(super) async fn remove(self, manager: &dyn UnitManager) -> Result<(), String> {
        if self.owned.contains(&PATH_UNIT) {
            if let Some(path) = manager.inspect(PATH_UNIT).await? {
                if path.transient
                    || path.description != "Wake Lasper X11 reconcile after system machine changes"
                {
                    return Err(format!(
                        "refusing to stop an external legacy user unit: {PATH_UNIT}"
                    ));
                }
                manager.stop(PATH_UNIT).await?;
            }
        }
        tokio::task::spawn_blocking(move || {
            for name in self.owned {
                if !is_owned(&self.directory, name)? {
                    return Err(format!("legacy X11 unit {name} changed during migration"));
                }
                self.directory
                    .remove_unlocked(name)
                    .map_err(|error| format!("remove legacy X11 unit {name}: {error}"))?;
            }
            Ok::<_, String>(())
        })
        .await
        .map_err(|error| format!("remove legacy X11 runtime files: {error}"))??;
        manager.reload().await
    }
}

fn is_owned(directory: &TrustedDirectory, name: &str) -> Result<bool, String> {
    let Some(file) = directory
        .read_bounded(name, MAX_UNIT_BYTES)
        .map_err(|error| format!("read legacy X11 unit {name}: {error}"))?
    else {
        return Ok(false);
    };
    Ok(file.uid == directory.expected_uid() && file.bytes.starts_with(MARKER))
}

#[cfg(test)]
#[path = "tests/legacy.rs"]
mod tests;

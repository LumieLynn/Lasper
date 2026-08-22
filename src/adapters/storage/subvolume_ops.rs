//! Low-level operations for Lasper-managed Btrfs subvolumes.

use crate::adapters::process::{log_output, CommandRunner};
use crate::adapters::storage::get_filesystem_type;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::MachineName;
use std::path::{Path, PathBuf};

pub async fn create_subvolume(
    machine: &MachineName,
    runner: &dyn CommandRunner,
) -> Result<PathBuf> {
    let parent = crate::paths::machines_dir();
    ensure_btrfs_parent(&parent).await?;
    create_subvolume_at(&parent, machine, runner).await
}

pub async fn remove_subvolume(machine: &MachineName, runner: &dyn CommandRunner) -> Result<()> {
    let parent = crate::paths::machines_dir();
    let target = machine_path(&parent, machine);

    match tokio::fs::symlink_metadata(&target).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(NspawnError::Io(target, error)),
    }

    ensure_btrfs_parent(&parent).await?;
    remove_subvolume_at(&parent, machine, runner).await
}

pub async fn is_subvolume(path: &Path) -> bool {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return false;
    }

    if !matches!(get_filesystem_type(path).await, Ok(fs_type) if fs_type == "btrfs") {
        return false;
    }

    if let Ok(metadata) = tokio::fs::metadata(path).await {
        use std::os::unix::fs::MetadataExt;
        if metadata.ino() == 256 {
            return true;
        }
    }

    let output = crate::adapters::process::new_command("btrfs")
        .args(["subvolume", "show", &path.to_string_lossy()])
        .output()
        .await;
    output
        .map(|result| result.status.success())
        .unwrap_or(false)
}

async fn ensure_btrfs_parent(parent: &Path) -> Result<()> {
    let metadata = tokio::fs::symlink_metadata(parent)
        .await
        .map_err(|error| NspawnError::Io(parent.to_path_buf(), error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(NspawnError::Validation(format!(
            "Btrfs machine storage is not a directory: {}",
            parent.display()
        )));
    }

    let fs_type = get_filesystem_type(parent).await?;
    if fs_type != "btrfs" {
        return Err(NspawnError::Validation(format!(
            "Btrfs subvolume storage requires /var/lib/machines on Btrfs, found {}",
            fs_type
        )));
    }
    Ok(())
}

async fn create_subvolume_at(
    parent: &Path,
    machine: &MachineName,
    runner: &dyn CommandRunner,
) -> Result<PathBuf> {
    let target = machine_path(parent, machine);
    reject_existing_target(&target).await?;
    reject_existing_target(&image_path(parent, machine, "raw")).await?;
    reject_existing_target(&image_path(parent, machine, "img")).await?;

    let output = runner
        .run(
            "btrfs",
            vec![
                "subvolume".into(),
                "create".into(),
                target.to_string_lossy().to_string(),
            ],
        )
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("btrfs"), error))?;
    log_output("btrfs subvolume create", &output);
    if !output.status.success() {
        return Err(NspawnError::cmd_failed(
            "btrfs subvolume create",
            format!("btrfs subvolume create {}", target.display()),
            &output,
        ));
    }
    Ok(target)
}

async fn remove_subvolume_at(
    parent: &Path,
    machine: &MachineName,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let target = machine_path(parent, machine);
    let metadata = tokio::fs::symlink_metadata(&target)
        .await
        .map_err(|error| NspawnError::Io(target.clone(), error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(NspawnError::Validation(format!(
            "Refusing to remove non-directory Btrfs storage path: {}",
            target.display()
        )));
    }

    let show = runner
        .run(
            "btrfs",
            vec![
                "subvolume".into(),
                "show".into(),
                target.to_string_lossy().to_string(),
            ],
        )
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("btrfs"), error))?;
    log_output("btrfs subvolume show", &show);
    if !show.status.success() {
        return Err(NspawnError::Validation(format!(
            "Refusing to remove ordinary directory as Btrfs subvolume: {}",
            target.display()
        )));
    }

    let output = runner
        .run(
            "btrfs",
            vec![
                "subvolume".into(),
                "delete".into(),
                target.to_string_lossy().to_string(),
            ],
        )
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("btrfs"), error))?;
    log_output("btrfs subvolume delete", &output);
    if !output.status.success() {
        return Err(NspawnError::cmd_failed(
            "btrfs subvolume delete",
            format!("btrfs subvolume delete {}", target.display()),
            &output,
        ));
    }
    Ok(())
}

async fn reject_existing_target(path: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => Err(NspawnError::Validation(format!(
            "Managed Btrfs storage already exists: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NspawnError::Io(path.to_path_buf(), error)),
    }
}

fn machine_path(parent: &Path, machine: &MachineName) -> PathBuf {
    parent.join(machine.as_str())
}

fn image_path(parent: &Path, machine: &MachineName, extension: &str) -> PathBuf {
    parent.join(format!("{}.{}", machine.as_str(), extension))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;

    fn output(success: bool, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn create_rejects_existing_directory_and_image_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let machine = MachineName::new("test").unwrap();
        let target = directory.path().join("test");
        tokio::fs::create_dir(&target).await.unwrap();

        let runner = MockCommandRunner::new();
        assert!(create_subvolume_at(directory.path(), &machine, &runner)
            .await
            .is_err());

        tokio::fs::remove_dir(&target).await.unwrap();
        tokio::fs::write(directory.path().join("test.raw"), b"image")
            .await
            .unwrap();
        assert!(create_subvolume_at(directory.path(), &machine, &runner)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn remove_rejects_an_ordinary_directory() {
        let directory = tempfile::tempdir().unwrap();
        let machine = MachineName::new("test").unwrap();
        tokio::fs::create_dir(directory.path().join("test"))
            .await
            .unwrap();

        let mut runner = MockCommandRunner::new();
        runner.expect_run().returning(|program, _| {
            assert_eq!(program, "btrfs");
            Ok(output(false, "", "Not a subvolume"))
        });

        let result = remove_subvolume_at(directory.path(), &machine, &runner).await;
        assert!(
            matches!(result, Err(NspawnError::Validation(message)) if message.contains("ordinary directory"))
        );
    }

    #[tokio::test]
    async fn remove_requires_successful_subvolume_probe_before_delete() {
        let directory = tempfile::tempdir().unwrap();
        let machine = MachineName::new("test").unwrap();
        tokio::fs::create_dir(directory.path().join("test"))
            .await
            .unwrap();

        let mut runner = MockCommandRunner::new();
        runner.expect_run().times(2).returning(|program, args| {
            assert_eq!(program, "btrfs");
            if args.get(1).map(String::as_str) == Some("show") {
                Ok(output(true, "Name: test", ""))
            } else {
                Err(std::io::Error::other("simulated delete failure"))
            }
        });

        let result = remove_subvolume_at(directory.path(), &machine, &runner).await;
        assert!(matches!(result, Err(NspawnError::Io(_, _))));
    }
}

use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::log_output;
use crate::adapters::rootfs::process::{nspawn_io_path, RootfsProcessRunner};
use crate::domain::secret::SecretBytes;
use std::collections::HashSet;
use std::path::{Component, Path};

const MAX_LD_CACHE_FOLDERS: usize = 512;
const MAX_ENVIRONMENT_ENTRIES: usize = 512;
const MAX_CLEANUP_PATHS: usize = 16_384;
const MAX_PATH_BYTES: usize = 4096;
const MAX_ENV_KEY_BYTES: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 8192;
const MAX_MUTATION_BYTES: usize = 1024 * 1024;
const CLEANUP_BATCH_SIZE: usize = 128;

pub(crate) async fn configure_nvidia_rootfs(
    rootfs: &Path,
    ld_cache_folders: &[String],
    environment: &[(String, String)],
    write_environment: bool,
    runner: &dyn RootfsProcessRunner,
) -> Result<Vec<String>> {
    validate_nvidia_config(ld_cache_folders, environment, write_environment)?;
    let mut warnings = Vec::new();

    if !ld_cache_folders.is_empty() {
        let mut seen = HashSet::new();
        let mut content = String::new();
        for folder in ld_cache_folders {
            if seen.insert(folder) {
                content.push_str(folder);
                content.push('\n');
            }
        }
        let output = write_system_file(
            rootfs,
            "/etc/ld.so.conf.d/lasper-nvidia.conf",
            true,
            content.into_bytes(),
            runner,
        )
        .await?;
        if output.status.success() {
            let output = runner
                .run(rootfs, vec!["ldconfig".into()], None)
                .await
                .map_err(|error| NspawnError::Io(nspawn_io_path(), error))?;
            log_output("ldconfig", &output);
            if !output.status.success() {
                warnings.push(format!(
                    "WARNING: ldconfig failed after NVIDIA configuration: {}",
                    command_error(&output)
                ));
            }
        } else {
            warnings.push(format!(
                "WARNING: Failed to write NVIDIA ld.so.conf: {}",
                command_error(&output)
            ));
        }
    }

    if write_environment {
        let existing = read_environment(rootfs, runner).await?;
        let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
        for (key, value) in environment {
            let prefix = format!("{key}=");
            lines.retain(|line| !line.starts_with(&prefix));
            lines.push(format!("{key}={value}"));
        }
        let content = if lines.is_empty() {
            Vec::new()
        } else {
            (lines.join("\n") + "\n").into_bytes()
        };
        if content.len() > MAX_MUTATION_BYTES {
            return Err(NspawnError::Validation(
                "NVIDIA environment output exceeds 1 MiB".into(),
            ));
        }
        let output = write_system_file(rootfs, "/etc/environment", false, content, runner).await?;
        if !output.status.success() {
            warnings.push(format!(
                "WARNING: Failed to write NVIDIA environment: {}",
                command_error(&output)
            ));
        }
    }

    Ok(warnings)
}

pub(crate) async fn cleanup_nvidia_files(
    rootfs: &Path,
    paths: &[String],
    runner: &dyn RootfsProcessRunner,
) -> Result<Vec<String>> {
    validate_cleanup_paths(paths)?;
    let mut warnings = Vec::new();
    for batch in paths.chunks(CLEANUP_BATCH_SIZE) {
        let mut command = vec![
            "sh".into(),
            "-c".into(),
            "for path do [ -f \"$path\" ] && [ ! -s \"$path\" ] && rm -f -- \"$path\"; done".into(),
            "_".into(),
        ];
        command.extend(batch.iter().cloned());
        let output = runner
            .run(rootfs, command, None)
            .await
            .map_err(|error| NspawnError::Io(nspawn_io_path(), error))?;
        log_output("NVIDIA cleanup", &output);
        if !output.status.success() {
            warnings.push(format!(
                "NVIDIA cleanup batch failed: {}",
                command_error(&output)
            ));
        }
    }
    Ok(warnings)
}

async fn read_environment(rootfs: &Path, runner: &dyn RootfsProcessRunner) -> Result<String> {
    let output = runner
        .run(rootfs, vec!["cat".into(), "/etc/environment".into()], None)
        .await
        .map_err(|error| NspawnError::Io(nspawn_io_path(), error))?;
    if !output.status.success() {
        return Ok(String::new());
    }
    if output.stdout.len() > MAX_MUTATION_BYTES {
        return Err(NspawnError::Validation(
            "Existing /etc/environment exceeds 1 MiB".into(),
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| NspawnError::Validation("Existing /etc/environment is not valid UTF-8".into()))
}

async fn write_system_file(
    rootfs: &Path,
    target: &str,
    create_parent: bool,
    content: Vec<u8>,
    runner: &dyn RootfsProcessRunner,
) -> Result<std::process::Output> {
    let script = if create_parent {
        "target=$1; install -d -m 0755 \"${target%/*}\"; [ ! -L \"$target\" ]; cat > \"$target\"; chmod 0644 \"$target\""
    } else {
        "target=$1; [ ! -L \"$target\" ]; cat > \"$target\"; chmod 0644 \"$target\""
    };
    let output = runner
        .run(
            rootfs,
            vec![
                "sh".into(),
                "-eu".into(),
                "-c".into(),
                script.into(),
                "_".into(),
                target.into(),
            ],
            Some(SecretBytes::new(content)),
        )
        .await
        .map_err(|error| NspawnError::Io(nspawn_io_path(), error))?;
    log_output("NVIDIA rootfs file", &output);
    Ok(output)
}

pub(crate) fn validate_nvidia_config(
    folders: &[String],
    environment: &[(String, String)],
    write_environment: bool,
) -> Result<()> {
    if folders.len() > MAX_LD_CACHE_FOLDERS {
        return Err(NspawnError::Validation(
            "Too many NVIDIA ld-cache folders".into(),
        ));
    }
    let mut total = 0usize;
    for folder in folders {
        validate_container_path("NVIDIA ld-cache folder", folder)?;
        total = total.saturating_add(folder.len());
    }
    if environment.len() > MAX_ENVIRONMENT_ENTRIES {
        return Err(NspawnError::Validation(
            "Too many NVIDIA environment entries".into(),
        ));
    }
    if !write_environment && !environment.is_empty() {
        return Err(NspawnError::Validation(
            "NVIDIA environment entries require environment injection to be enabled".into(),
        ));
    }
    for (key, value) in environment {
        validate_environment_entry(key, value)?;
        total = total.saturating_add(key.len()).saturating_add(value.len());
    }
    if total > MAX_MUTATION_BYTES {
        return Err(NspawnError::Validation(
            "NVIDIA rootfs configuration exceeds 1 MiB".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_cleanup_paths(paths: &[String]) -> Result<()> {
    if paths.len() > MAX_CLEANUP_PATHS {
        return Err(NspawnError::Validation(
            "Too many NVIDIA cleanup paths".into(),
        ));
    }
    let mut total = 0usize;
    for path in paths {
        validate_container_path("NVIDIA cleanup path", path)?;
        if path == "/" {
            return Err(NspawnError::Validation(
                "NVIDIA cleanup path must not be the container root".into(),
            ));
        }
        total = total.saturating_add(path.len());
    }
    if total > MAX_MUTATION_BYTES {
        return Err(NspawnError::Validation(
            "NVIDIA cleanup paths exceed 1 MiB".into(),
        ));
    }
    Ok(())
}

fn validate_container_path(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_PATH_BYTES
        || value.chars().any(char::is_control)
        || !Path::new(value).is_absolute()
    {
        return Err(NspawnError::Validation(format!(
            "Invalid {label}: {value:?}"
        )));
    }
    if Path::new(value)
        .components()
        .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(NspawnError::Validation(format!(
            "{label} contains relative path components: {value:?}"
        )));
    }
    Ok(())
}

fn validate_environment_entry(key: &str, value: &str) -> Result<()> {
    let valid_key = !key.is_empty()
        && key.len() <= MAX_ENV_KEY_BYTES
        && key.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        });
    if !valid_key || value.len() > MAX_ENV_VALUE_BYTES || value.chars().any(char::is_control) {
        return Err(NspawnError::Validation(format!(
            "Invalid NVIDIA environment entry: {key:?}"
        )));
    }
    Ok(())
}

fn command_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    if message.is_empty() {
        format!("process exited with {}", output.status)
    } else {
        message.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::rootfs::process::MockRootfsProcessRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;
    use std::sync::{Arc, Mutex};

    fn success_output(stdout: &[u8]) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn mutation_validation_rejects_path_and_environment_injection() {
        assert!(validate_nvidia_config(&["../../host".into()], &[], false).is_err());
        assert!(
            validate_nvidia_config(&[], &[("LD_PRELOAD\nEVIL".into(), "/tmp/x".into())], true,)
                .is_err()
        );
        assert!(validate_cleanup_paths(&["/usr/lib/../etc/shadow".into()]).is_err());
    }

    #[tokio::test]
    async fn environment_update_preserves_unmanaged_lines() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut runner = MockRootfsProcessRunner::new();
        let captured = calls.clone();
        runner
            .expect_run()
            .times(2)
            .returning(move |_, command, stdin| {
                captured.lock().unwrap().push((command.clone(), stdin));
                if command.first().is_some_and(|arg| arg == "cat") {
                    Ok(success_output(b"LANG=C\nNVIDIA_VISIBLE_DEVICES=old\n"))
                } else {
                    Ok(success_output(&[]))
                }
            });

        configure_nvidia_rootfs(
            Path::new("/tmp/rootfs"),
            &[],
            &[("NVIDIA_VISIBLE_DEVICES".into(), "void".into())],
            true,
            &runner,
        )
        .await
        .unwrap();

        let calls = calls.lock().unwrap();
        let content = std::str::from_utf8(calls[1].1.as_ref().unwrap().as_slice()).unwrap();
        assert_eq!(content, "LANG=C\nNVIDIA_VISIBLE_DEVICES=void\n");
    }

    #[tokio::test]
    async fn cleanup_paths_are_data_arguments_not_shell_source() {
        let mut runner = MockRootfsProcessRunner::new();
        runner.expect_run().once().returning(|_, command, stdin| {
            assert!(stdin.is_none());
            let script = command
                .iter()
                .find(|arg| arg.contains("for path do"))
                .unwrap();
            assert!(!script.contains("odd'path"));
            assert!(command.iter().any(|arg| arg == "/usr/lib/odd'path.so"));
            Ok(success_output(&[]))
        });

        cleanup_nvidia_files(
            Path::new("/tmp/rootfs"),
            &["/usr/lib/odd'path.so".into()],
            &runner,
        )
        .await
        .unwrap();
    }
}

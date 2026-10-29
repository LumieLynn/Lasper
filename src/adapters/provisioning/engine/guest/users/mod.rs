use crate::adapters::process::log_output;
use crate::adapters::process::{nspawn_io_path, RootfsProcessRunner};
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::domain::machine::{validate_login_username, CreateUser};
use crate::domain::secret::{validate_chpasswd_secret, SecretBytes};
use crate::domain::wayland::ContainerUserIdentity;
use std::path::Path;

pub(super) async fn create_user_in_container(
    rootfs: &Path,
    user: &CreateUser,
    password: Option<&str>,
    runner: &dyn RootfsProcessRunner,
) -> OperationResult<()> {
    user.validate()
        .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?;
    if let Some(password) = password {
        validate_chpasswd_secret(password)
            .map_err(|error| OperationFailure::InvalidInput(error.message("user password")))?;
    }
    let shell = user.login_shell();

    let mut command = vec!["useradd".into(), "-m".into()];
    if let Some(uid) = user.uid {
        command.extend(["--uid".into(), uid.to_string()]);
    }
    command.extend(["-s".into(), shell.into(), user.username.clone()]);

    let output = runner
        .run(rootfs, command, None)
        .await
        .map_err(|error| OperationFailure::Io(nspawn_io_path(), error))?;
    log_output("useradd", &output);
    if !output.status.success() {
        return Err(OperationFailure::command_failed(
            "useradd in container",
            format!("systemd-nspawn -D {:?} -- useradd ...", rootfs),
            &output,
        ));
    }

    if user.sudoer {
        configure_sudoer(rootfs, &user.username, runner).await?;
    }

    if let Some(password) = password.filter(|password| !password.is_empty()) {
        set_password(rootfs, &user.username, password, runner).await?;
    }
    Ok(())
}

pub(super) async fn resolve_user_identity(
    rootfs: &Path,
    username: &str,
    runner: &dyn RootfsProcessRunner,
) -> OperationResult<ContainerUserIdentity> {
    validate_login_username(username)
        .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?;
    let uid = query_numeric_identity(rootfs, username, "-u", "uid", runner).await?;
    let gid = query_numeric_identity(rootfs, username, "-g", "gid", runner).await?;
    Ok(ContainerUserIdentity {
        username: username.to_string(),
        uid,
        gid,
    })
}

async fn query_numeric_identity(
    rootfs: &Path,
    username: &str,
    flag: &str,
    label: &str,
    runner: &dyn RootfsProcessRunner,
) -> OperationResult<u32> {
    let output = runner
        .run(
            rootfs,
            vec!["id".into(), flag.into(), username.into()],
            None,
        )
        .await
        .map_err(|error| OperationFailure::Io(nspawn_io_path(), error))?;
    log_output("id", &output);
    if !output.status.success() {
        return Err(OperationFailure::command_failed(
            "resolve container user identity",
            format!("systemd-nspawn -D {:?} -- id {flag} {username}", rootfs),
            &output,
        ));
    }
    let value = std::str::from_utf8(&output.stdout)
        .map_err(|_| {
            OperationFailure::InconsistentState(format!("container {label} is not valid UTF-8"))
        })?
        .trim();
    value.parse::<u32>().map_err(|_| {
        OperationFailure::InconsistentState(format!(
            "container {label} lookup returned an invalid value: {value:?}"
        ))
    })
}

pub(super) async fn set_root_password(
    rootfs: &Path,
    password: &str,
    runner: &dyn RootfsProcessRunner,
) -> OperationResult<()> {
    if password.is_empty() {
        return Ok(());
    }
    set_password(rootfs, "root", password, runner).await
}

async fn set_password(
    rootfs: &Path,
    username: &str,
    password: &str,
    runner: &dyn RootfsProcessRunner,
) -> OperationResult<()> {
    let output = run_chpasswd(rootfs, username, password, runner).await?;
    if !output.status.success() {
        return Err(OperationFailure::command_failed(
            format!("set password for {username}"),
            format!("systemd-nspawn -D {:?} -- chpasswd", rootfs),
            &output,
        ));
    }
    Ok(())
}

async fn run_chpasswd(
    rootfs: &Path,
    username: &str,
    password: &str,
    runner: &dyn RootfsProcessRunner,
) -> OperationResult<std::process::Output> {
    validate_chpasswd_secret(password)
        .map_err(|error| OperationFailure::InvalidInput(error.message("password")))?;
    let mut input = Vec::with_capacity(username.len() + password.len() + 2);
    input.extend_from_slice(username.as_bytes());
    input.push(b':');
    input.extend_from_slice(password.as_bytes());
    input.push(b'\n');
    let output = runner
        .run(
            rootfs,
            vec!["chpasswd".into()],
            Some(SecretBytes::new(input)),
        )
        .await
        .map_err(|error| OperationFailure::Io(nspawn_io_path(), error))?;
    log_output("chpasswd", &output);
    Ok(output)
}

async fn configure_sudoer(
    rootfs: &Path,
    username: &str,
    runner: &dyn RootfsProcessRunner,
) -> OperationResult<()> {
    let mut group_failures = Vec::new();
    for group in ["sudo", "wheel"] {
        let output = runner
            .run(
                rootfs,
                vec![
                    "usermod".into(),
                    "-aG".into(),
                    group.into(),
                    username.into(),
                ],
                None,
            )
            .await;
        match output {
            Ok(output) if output.status.success() => {
                log_output("usermod", &output);
            }
            Ok(output) => {
                log_output("usermod", &output);
                group_failures.push(format!("{group}: {}", command_error(&output)));
                continue;
            }
            Err(error) => {
                group_failures.push(format!("{group}: {error}"));
                continue;
            }
        }

        let sudoers = format!(
            "# Managed by Lasper: sudo access for provisioned users\n%{group} ALL=(ALL:ALL) ALL\n"
        )
        .into_bytes();
        let output = runner
            .run(
                rootfs,
                vec![
                    "sh".into(),
                    "-eu".into(),
                    "-c".into(),
                    concat!(
                        "grep -Eq '^[[:space:]]*([@#]includedir)[[:space:]]+\"?(/etc/)?sudoers\\.d/?\"?([[:space:]]|$)' /etc/sudoers || ",
                        "{ echo '/etc/sudoers does not include /etc/sudoers.d' >&2; exit 1; }; ",
                        "command -v visudo >/dev/null || { echo 'visudo is unavailable' >&2; exit 1; }; ",
                        "LC_ALL=C visudo -cf /etc/sudoers >/dev/null; ",
                        "target=/etc/sudoers.d/90-lasper-$1; ",
                        "install -d -m 0750 /etc/sudoers.d; ",
                        "[ ! -L \"$target\" ]; ",
                        "if [ -e \"$target\" ]; then head -n 1 \"$target\" | ",
                        "grep -Fx '# Managed by Lasper: sudo access for provisioned users' >/dev/null || ",
                        "{ echo 'refusing to replace an unmanaged sudoers policy' >&2; exit 1; }; fi; ",
                        "tmp=$(mktemp /etc/sudoers.d/.lasper-sudoers.XXXXXX); ",
                        "trap 'rm -f \"$tmp\"' EXIT; cat > \"$tmp\"; chmod 0440 \"$tmp\"; ",
                        "LC_ALL=C visudo -cf \"$tmp\" >/dev/null; mv -f \"$tmp\" \"$target\"; ",
                        "trap - EXIT; LC_ALL=C visudo -cf /etc/sudoers >/dev/null"
                    )
                    .into(),
                    "_".into(),
                    group.into(),
                ],
                Some(SecretBytes::new(sudoers)),
            )
            .await
            .map_err(|error| OperationFailure::Io(nspawn_io_path(), error))?;
        log_output("sudoers", &output);
        if !output.status.success() {
            return Err(OperationFailure::command_failed(
                "write sudoers policy in container",
                format!("systemd-nspawn -D {:?} -- write sudoers", rootfs),
                &output,
            ));
        }
        return Ok(());
    }
    Err(OperationFailure::InconsistentState(format!(
        "Could not grant sudo access to {username:?}: neither sudo nor wheel group was usable ({})",
        group_failures.join("; ")
    )))
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
mod tests;

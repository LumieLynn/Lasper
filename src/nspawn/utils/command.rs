//! Command builder helpers.

/// Creates a new `tokio::process::Command` with `LC_ALL=C` set.
pub fn new_command(program: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.env("LC_ALL", "C");
    cmd
}

/// Creates a new `std::process::Command` with `LC_ALL=C` set.
pub fn new_sync_command(program: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.env("LC_ALL", "C");
    cmd
}

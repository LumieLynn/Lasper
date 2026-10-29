use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RootfsError {
    #[error("invalid rootfs target: {0}")]
    InvalidTarget(String),
    #[error("I/O error in {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("{operation} failed: {command}. {diagnostic}")]
    CommandFailed {
        operation: &'static str,
        command: String,
        diagnostic: String,
    },
}

impl RootfsError {
    pub(super) fn command_failed(
        operation: &'static str,
        command: impl Into<String>,
        output: &std::process::Output,
    ) -> Self {
        Self::CommandFailed {
            operation,
            command: command.into(),
            diagnostic: crate::adapters::process::command_diagnostic(output),
        }
    }
}

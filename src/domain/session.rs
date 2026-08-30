use std::fmt;
use std::num::{NonZeroU16, NonZeroU64};

/// Identity of one terminal or journal session within a Lasper process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(NonZeroU64);

impl SessionId {
    pub fn new(value: u64) -> Result<Self, SessionIdError> {
        NonZeroU64::new(value).map(Self).ok_or(SessionIdError)
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionIdError;

impl fmt::Display for SessionIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("session id must be non-zero")
    }
}

impl std::error::Error for SessionIdError {}

/// Valid terminal dimensions shared by direct and elevated session adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionSize {
    cols: NonZeroU16,
    rows: NonZeroU16,
}

impl SessionSize {
    pub fn new(cols: u16, rows: u16) -> Result<Self, SessionSizeError> {
        Ok(Self {
            cols: NonZeroU16::new(cols).ok_or(SessionSizeError)?,
            rows: NonZeroU16::new(rows).ok_or(SessionSizeError)?,
        })
    }

    pub(crate) fn from_nonzero(cols: NonZeroU16, rows: NonZeroU16) -> Self {
        Self { cols, rows }
    }

    pub fn cols(self) -> u16 {
        self.cols.get()
    }

    pub fn rows(self) -> u16 {
        self.rows.get()
    }

    pub(crate) fn into_nonzero(self) -> (NonZeroU16, NonZeroU16) {
        (self.cols, self.rows)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionSizeError;

impl fmt::Display for SessionSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("session dimensions must be non-zero u16 values")
    }
}

impl std::error::Error for SessionSizeError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalAttachmentKind {
    Login,
    Namespace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionLifecycle {
    Running,
    Exited { success: bool, code: Option<i32> },
    Failed(String),
    Closed,
}

impl SessionLifecycle {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_identifiers_and_sizes_reject_zero() {
        assert!(SessionId::new(0).is_err());
        assert!(SessionId::new(1).is_ok());
        assert!(SessionSize::new(80, 24).is_ok());
        assert!(SessionSize::new(0, 24).is_err());
        assert!(SessionSize::new(80, 0).is_err());
    }
}

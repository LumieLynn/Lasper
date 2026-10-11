/// Severity level for status messages shown in the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusLevel {
    Info,
    Success,
    Warn,
    Error,
}

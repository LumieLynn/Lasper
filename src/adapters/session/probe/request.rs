use std::path::{Component, Path};

const MAX_TARGET_PATH_BYTES: usize = 4096;

pub(crate) fn validate_absolute_path(path: &Path) -> Result<String, ProbePathError> {
    if !path.is_absolute() {
        return Err(ProbePathError::NotAbsolute);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(ProbePathError::RelativeComponent);
    }
    let value = path.to_str().ok_or(ProbePathError::NonUtf8)?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(ProbePathError::InvalidValue);
    }
    if value.len() > MAX_TARGET_PATH_BYTES {
        return Err(ProbePathError::TooLong {
            maximum: MAX_TARGET_PATH_BYTES,
        });
    }
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProbePathError {
    #[error("probe path must be absolute")]
    NotAbsolute,
    #[error("probe path must not contain relative path components")]
    RelativeComponent,
    #[error("probe path is not valid UTF-8")]
    NonUtf8,
    #[error("probe path contains an empty or control-character value")]
    InvalidValue,
    #[error("probe path exceeds the {maximum}-byte path limit")]
    TooLong { maximum: usize },
}

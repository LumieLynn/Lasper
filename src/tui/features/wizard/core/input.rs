//! Wizard path-entry normalization, separate from text-input behavior.

use std::path::{Path, PathBuf};

pub fn expand_user_path(value: &str) -> Result<PathBuf, String> {
    expand_user_path_with_home(value.trim(), dirs::home_dir().as_deref())
}

fn expand_user_path_with_home(value: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    if value == "~" || value.starts_with("~/") {
        let home = home.ok_or_else(|| "Home directory is unavailable".to_string())?;
        return Ok(if value == "~" {
            home.to_path_buf()
        } else {
            home.join(&value[2..])
        });
    }
    Ok(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::expand_user_path_with_home;
    use std::path::{Path, PathBuf};

    #[test]
    fn expands_current_users_home_shorthand() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_user_path_with_home("~", Some(home)).unwrap(),
            PathBuf::from("/home/tester")
        );
        assert_eq!(
            expand_user_path_with_home("~/images/test.raw", Some(home)).unwrap(),
            PathBuf::from("/home/tester/images/test.raw")
        );
    }

    #[test]
    fn leaves_absolute_and_relative_paths_unchanged() {
        assert_eq!(
            expand_user_path_with_home("/tmp/test.raw", None).unwrap(),
            PathBuf::from("/tmp/test.raw")
        );
        assert_eq!(
            expand_user_path_with_home("images/test.raw", None).unwrap(),
            PathBuf::from("images/test.raw")
        );
    }

    #[test]
    fn requires_home_only_for_supported_tilde_forms() {
        assert!(expand_user_path_with_home("~/test.raw", None).is_err());
        assert_eq!(
            expand_user_path_with_home("~other/test.raw", None).unwrap(),
            PathBuf::from("~other/test.raw")
        );
    }
}

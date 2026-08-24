use crate::tui::core::{Component, EventResult};
use crate::tui::widgets::inputs::text_input_base::TextInputBase;
use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};
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

#[allow(clippy::type_complexity)]
pub struct PathBox {
    base: TextInputBase,
    validator: Option<Box<dyn Fn(&str) -> Result<(), String>>>,
}

impl PathBox {
    pub fn new(label: impl Into<String>, initial_value: String) -> Self {
        Self {
            base: TextInputBase::new(label, initial_value),
            validator: None,
        }
    }

    #[allow(dead_code)]
    pub fn set_value(&mut self, value: String) {
        self.base.input = tui_input::Input::from(value);
    }

    pub fn with_validator<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) -> Result<(), String> + 'static,
    {
        self.validator = Some(Box::new(f));
        self
    }

    pub fn value(&self) -> &str {
        self.base.input.value()
    }
}

impl Component for PathBox {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        self.base.render_base(f, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        self.base.handle_key(key)
    }

    fn set_focus(&mut self, focused: bool) {
        self.base.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.base.focused
    }

    fn is_enabled(&self) -> bool {
        self.base.enabled
    }

    fn validate(&mut self) -> Result<(), String> {
        let val = self.base.input.value().to_string();
        if let Some(validator) = &self.validator {
            if let Err(e) = validator(&val) {
                self.base.error_msg = Some(e.clone());
                return Err(e);
            }
        }
        self.base.error_msg = None;
        Ok(())
    }
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

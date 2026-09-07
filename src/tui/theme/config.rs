use super::color_def::ColorDef;
use super::detect::is_light_background;
use super::Theme;

/// Deserialized theme config where every field is optional.
/// Users override only the colors they care about in `~/.config/lasper/lasper.toml`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct PartialTheme {
    pub text_primary: Option<ColorDef>,
    pub text_secondary: Option<ColorDef>,
    pub text_dim: Option<ColorDef>,
    pub accent: Option<ColorDef>,
    pub highlight: Option<ColorDef>,
    pub highlight_secondary: Option<ColorDef>,
    pub success: Option<ColorDef>,
    pub warning: Option<ColorDef>,
    pub error: Option<ColorDef>,
    pub state_running: Option<ColorDef>,
    pub state_stopped: Option<ColorDef>,
    pub state_transitional: Option<ColorDef>,
    pub border_focused: Option<ColorDef>,
    pub border_unfocused: Option<ColorDef>,
    pub border_disabled: Option<ColorDef>,
    pub border_panel_primary: Option<ColorDef>,
    pub border_panel_secondary: Option<ColorDef>,
    pub tab_active_focused: Option<ColorDef>,
    pub tab_active_unfocused: Option<ColorDef>,
    pub tab_inactive: Option<ColorDef>,
    pub resize_focused: Option<ColorDef>,
    pub resize_unfocused: Option<ColorDef>,
    pub badge_root: Option<ColorDef>,
    pub badge_readonly: Option<ColorDef>,
    #[serde(alias = "badge_cli")]
    pub badge_systemd_tools: Option<ColorDef>,
    pub status_info: Option<ColorDef>,
    pub status_success: Option<ColorDef>,
    pub status_warning: Option<ColorDef>,
    pub status_error: Option<ColorDef>,
    pub key_hint_fg: Option<ColorDef>,
    pub hint_fg: Option<ColorDef>,
    pub list_selected_focused: Option<ColorDef>,
    pub list_selected_unfocused: Option<ColorDef>,
    pub list_unselected: Option<ColorDef>,
    pub list_icon_alive: Option<ColorDef>,
    pub list_icon_dead: Option<ColorDef>,
    pub list_addr: Option<ColorDef>,
    pub list_empty: Option<ColorDef>,
    pub list_cursor_focused: Option<ColorDef>,
    pub list_cursor_unfocused: Option<ColorDef>,
    pub prop_enabled: Option<ColorDef>,
    pub prop_disabled: Option<ColorDef>,
    pub prop_unknown: Option<ColorDef>,
    pub prop_transitional: Option<ColorDef>,
    pub prop_pid: Option<ColorDef>,
    pub prop_memory: Option<ColorDef>,
    pub prop_default: Option<ColorDef>,
    pub prop_readonly_yes: Option<ColorDef>,
    pub prop_readonly_no: Option<ColorDef>,
    pub chart_cpu: Option<ColorDef>,
    pub chart_ram: Option<ColorDef>,
    pub chart_axis: Option<ColorDef>,
    pub button_focused_fg: Option<ColorDef>,
    pub button_focused_bg: Option<ColorDef>,
    pub button_unfocused_fg: Option<ColorDef>,
    pub button_border_focused: Option<ColorDef>,
    pub button_border_unfocused: Option<ColorDef>,
    pub dialog_border: Option<ColorDef>,
    pub dialog_border_warn: Option<ColorDef>,
    pub dialog_text: Option<ColorDef>,
    pub dialog_host_border: Option<ColorDef>,
    pub dialog_host_text: Option<ColorDef>,
    pub config_section: Option<ColorDef>,
    pub config_key: Option<ColorDef>,
    pub config_value: Option<ColorDef>,
    pub help_key: Option<ColorDef>,
    pub help_border: Option<ColorDef>,
    pub help_title: Option<ColorDef>,
    pub help_close_hint: Option<ColorDef>,
    pub wizard_border: Option<ColorDef>,
    pub wizard_footer: Option<ColorDef>,
    pub confirm_hint: Option<ColorDef>,
    pub cancel_hint: Option<ColorDef>,
    pub editor_error: Option<ColorDef>,
    pub terminal_insert_border: Option<ColorDef>,
}

/// Load the theme: config takes precedence, then auto-detection, then dark fallback.
pub fn load_theme(partial: Option<&PartialTheme>) -> Theme {
    if let Some(partial) = partial {
        return merge(partial);
    }

    // No config or theme section — auto-detect terminal background.
    if is_light_background() {
        Theme::light()
    } else {
        Theme::dark()
    }
}

/// Merge partial config over the auto-detected base theme.
fn merge(partial: &PartialTheme) -> Theme {
    let base = if is_light_background() {
        Theme::light()
    } else {
        Theme::dark()
    };

    let mut t = base;

    macro_rules! merge_field {
        ($field:ident) => {
            if let Some(ref v) = partial.$field {
                t.$field = v.clone().into();
            }
        };
    }

    merge_field!(text_primary);
    merge_field!(text_secondary);
    merge_field!(text_dim);
    merge_field!(accent);
    merge_field!(highlight);
    merge_field!(highlight_secondary);
    merge_field!(success);
    merge_field!(warning);
    merge_field!(error);
    merge_field!(state_running);
    merge_field!(state_stopped);
    merge_field!(state_transitional);
    merge_field!(border_focused);
    merge_field!(border_unfocused);
    merge_field!(border_disabled);
    merge_field!(border_panel_primary);
    merge_field!(border_panel_secondary);
    merge_field!(tab_active_focused);
    merge_field!(tab_active_unfocused);
    merge_field!(tab_inactive);
    merge_field!(resize_focused);
    merge_field!(resize_unfocused);
    merge_field!(badge_root);
    merge_field!(badge_readonly);
    merge_field!(badge_systemd_tools);
    merge_field!(status_info);
    merge_field!(status_success);
    merge_field!(status_warning);
    merge_field!(status_error);
    merge_field!(key_hint_fg);
    merge_field!(hint_fg);
    merge_field!(list_selected_focused);
    merge_field!(list_selected_unfocused);
    merge_field!(list_unselected);
    merge_field!(list_icon_alive);
    merge_field!(list_icon_dead);
    merge_field!(list_addr);
    merge_field!(list_empty);
    merge_field!(list_cursor_focused);
    merge_field!(list_cursor_unfocused);
    merge_field!(prop_enabled);
    merge_field!(prop_disabled);
    merge_field!(prop_unknown);
    merge_field!(prop_transitional);
    merge_field!(prop_pid);
    merge_field!(prop_memory);
    merge_field!(prop_default);
    merge_field!(prop_readonly_yes);
    merge_field!(prop_readonly_no);
    merge_field!(chart_cpu);
    merge_field!(chart_ram);
    merge_field!(chart_axis);
    merge_field!(button_focused_fg);
    merge_field!(button_focused_bg);
    merge_field!(button_unfocused_fg);
    merge_field!(button_border_focused);
    merge_field!(button_border_unfocused);
    merge_field!(dialog_border);
    merge_field!(dialog_border_warn);
    merge_field!(dialog_text);
    merge_field!(dialog_host_border);
    merge_field!(dialog_host_text);
    merge_field!(config_section);
    merge_field!(config_key);
    merge_field!(config_value);
    merge_field!(help_key);
    merge_field!(help_border);
    merge_field!(help_title);
    merge_field!(help_close_hint);
    merge_field!(wizard_border);
    merge_field!(wizard_footer);
    merge_field!(confirm_hint);
    merge_field!(cancel_hint);
    merge_field!(editor_error);
    merge_field!(terminal_insert_border);

    t
}

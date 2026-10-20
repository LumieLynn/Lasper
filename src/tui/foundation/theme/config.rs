use super::color_def::ColorDef;
use super::detect::is_light_background;
use super::Theme;
use ratatui::style::Color;

/// Deserialized theme config where every field is optional.
/// Users override only the colors they care about in `~/.config/lasper/lasper.toml`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PartialTheme {
    // Text and accents
    pub text_primary: Option<ColorDef>,
    pub text_secondary: Option<ColorDef>,
    pub text_dim: Option<ColorDef>,
    pub accent: Option<ColorDef>,
    pub highlight: Option<ColorDef>,

    // Semantic states
    pub success: Option<ColorDef>,
    pub warning: Option<ColorDef>,
    pub error: Option<ColorDef>,

    // Borders, tabs, and resize mode
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

    // Badges and status bar
    pub badge_root: Option<ColorDef>,
    pub badge_readonly: Option<ColorDef>,
    pub badge_systemd_tools: Option<ColorDef>,
    pub status_info: Option<ColorDef>,
    pub status_success: Option<ColorDef>,
    pub status_warning: Option<ColorDef>,
    pub status_error: Option<ColorDef>,
    pub key_hint_fg: Option<ColorDef>,
    pub hint_fg: Option<ColorDef>,

    // Lists
    pub list_selected_focused: Option<ColorDef>,
    pub list_selected_unfocused: Option<ColorDef>,
    pub list_unselected: Option<ColorDef>,
    pub list_icon_alive: Option<ColorDef>,
    pub list_icon_dead: Option<ColorDef>,
    pub list_addr: Option<ColorDef>,
    pub list_empty: Option<ColorDef>,
    pub list_cursor_focused: Option<ColorDef>,
    pub list_cursor_unfocused: Option<ColorDef>,
    pub list_highlight_symbol: Option<ColorDef>,
    pub list_disabled_item: Option<ColorDef>,

    // Properties and charts
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

    // Buttons and dialogs
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

    // Configuration, help, wizard, and terminal
    pub config_section: Option<ColorDef>,
    pub config_key: Option<ColorDef>,
    pub config_value: Option<ColorDef>,
    pub help_key: Option<ColorDef>,
    pub help_border: Option<ColorDef>,
    pub help_title: Option<ColorDef>,
    pub wizard_border: Option<ColorDef>,
    pub editor_error: Option<ColorDef>,
    pub terminal_insert_border: Option<ColorDef>,
}

/// Load the theme: config takes precedence, then auto-detection, then dark fallback.
pub fn load_theme(partial: Option<&PartialTheme>) -> Theme {
    let base = if is_light_background() {
        Theme::light()
    } else {
        Theme::dark()
    };
    match partial {
        Some(partial) => merge(partial, base),
        None => base,
    }
}

/// Merge partial config over the auto-detected base theme.
fn merge(partial: &PartialTheme, base: Theme) -> Theme {
    // Both sides are exhaustive: adding a config or runtime field requires
    // updating its resolution here, rather than silently keeping a default.
    let PartialTheme {
        text_primary,
        text_secondary,
        text_dim,
        accent,
        highlight,
        success,
        warning,
        error,
        border_focused,
        border_unfocused,
        border_disabled,
        border_panel_primary,
        border_panel_secondary,
        tab_active_focused,
        tab_active_unfocused,
        tab_inactive,
        resize_focused,
        resize_unfocused,
        badge_root,
        badge_readonly,
        badge_systemd_tools,
        status_info,
        status_success,
        status_warning,
        status_error,
        key_hint_fg,
        hint_fg,
        list_selected_focused,
        list_selected_unfocused,
        list_unselected,
        list_icon_alive,
        list_icon_dead,
        list_addr,
        list_empty,
        list_cursor_focused,
        list_cursor_unfocused,
        list_highlight_symbol,
        list_disabled_item,
        prop_enabled,
        prop_disabled,
        prop_unknown,
        prop_transitional,
        prop_pid,
        prop_memory,
        prop_default,
        prop_readonly_yes,
        prop_readonly_no,
        chart_cpu,
        chart_ram,
        chart_axis,
        button_focused_fg,
        button_focused_bg,
        button_unfocused_fg,
        button_border_focused,
        button_border_unfocused,
        dialog_border,
        dialog_border_warn,
        dialog_text,
        dialog_host_border,
        dialog_host_text,
        config_section,
        config_key,
        config_value,
        help_key,
        help_border,
        help_title,
        wizard_border,
        editor_error,
        terminal_insert_border,
    } = partial;

    Theme {
        text_primary: resolve(text_primary, base.text_primary),
        text_secondary: resolve(text_secondary, base.text_secondary),
        text_dim: resolve(text_dim, base.text_dim),
        accent: resolve(accent, base.accent),
        highlight: resolve(highlight, base.highlight),
        success: resolve(success, base.success),
        warning: resolve(warning, base.warning),
        error: resolve(error, base.error),
        border_focused: resolve(border_focused, base.border_focused),
        border_unfocused: resolve(border_unfocused, base.border_unfocused),
        border_disabled: resolve(border_disabled, base.border_disabled),
        border_panel_primary: resolve(border_panel_primary, base.border_panel_primary),
        border_panel_secondary: resolve(border_panel_secondary, base.border_panel_secondary),
        tab_active_focused: resolve(tab_active_focused, base.tab_active_focused),
        tab_active_unfocused: resolve(tab_active_unfocused, base.tab_active_unfocused),
        tab_inactive: resolve(tab_inactive, base.tab_inactive),
        resize_focused: resolve(resize_focused, base.resize_focused),
        resize_unfocused: resolve(resize_unfocused, base.resize_unfocused),
        badge_root: resolve(badge_root, base.badge_root),
        badge_readonly: resolve(badge_readonly, base.badge_readonly),
        badge_systemd_tools: resolve(badge_systemd_tools, base.badge_systemd_tools),
        status_info: resolve(status_info, base.status_info),
        status_success: resolve(status_success, base.status_success),
        status_warning: resolve(status_warning, base.status_warning),
        status_error: resolve(status_error, base.status_error),
        key_hint_fg: resolve(key_hint_fg, base.key_hint_fg),
        hint_fg: resolve(hint_fg, base.hint_fg),
        list_selected_focused: resolve(list_selected_focused, base.list_selected_focused),
        list_selected_unfocused: resolve(list_selected_unfocused, base.list_selected_unfocused),
        list_unselected: resolve(list_unselected, base.list_unselected),
        list_icon_alive: resolve(list_icon_alive, base.list_icon_alive),
        list_icon_dead: resolve(list_icon_dead, base.list_icon_dead),
        list_addr: resolve(list_addr, base.list_addr),
        list_empty: resolve(list_empty, base.list_empty),
        list_cursor_focused: resolve(list_cursor_focused, base.list_cursor_focused),
        list_cursor_unfocused: resolve(list_cursor_unfocused, base.list_cursor_unfocused),
        list_highlight_symbol: resolve(list_highlight_symbol, base.list_highlight_symbol),
        list_disabled_item: resolve(list_disabled_item, base.list_disabled_item),
        prop_enabled: resolve(prop_enabled, base.prop_enabled),
        prop_disabled: resolve(prop_disabled, base.prop_disabled),
        prop_unknown: resolve(prop_unknown, base.prop_unknown),
        prop_transitional: resolve(prop_transitional, base.prop_transitional),
        prop_pid: resolve(prop_pid, base.prop_pid),
        prop_memory: resolve(prop_memory, base.prop_memory),
        prop_default: resolve(prop_default, base.prop_default),
        prop_readonly_yes: resolve(prop_readonly_yes, base.prop_readonly_yes),
        prop_readonly_no: resolve(prop_readonly_no, base.prop_readonly_no),
        chart_cpu: resolve(chart_cpu, base.chart_cpu),
        chart_ram: resolve(chart_ram, base.chart_ram),
        chart_axis: resolve(chart_axis, base.chart_axis),
        button_focused_fg: resolve(button_focused_fg, base.button_focused_fg),
        button_focused_bg: resolve(button_focused_bg, base.button_focused_bg),
        button_unfocused_fg: resolve(button_unfocused_fg, base.button_unfocused_fg),
        button_border_focused: resolve(button_border_focused, base.button_border_focused),
        button_border_unfocused: resolve(button_border_unfocused, base.button_border_unfocused),
        dialog_border: resolve(dialog_border, base.dialog_border),
        dialog_border_warn: resolve(dialog_border_warn, base.dialog_border_warn),
        dialog_text: resolve(dialog_text, base.dialog_text),
        dialog_host_border: resolve(dialog_host_border, base.dialog_host_border),
        dialog_host_text: resolve(dialog_host_text, base.dialog_host_text),
        config_section: resolve(config_section, base.config_section),
        config_key: resolve(config_key, base.config_key),
        config_value: resolve(config_value, base.config_value),
        help_key: resolve(help_key, base.help_key),
        help_border: resolve(help_border, base.help_border),
        help_title: resolve(help_title, base.help_title),
        wizard_border: resolve(wizard_border, base.wizard_border),
        editor_error: resolve(editor_error, base.editor_error),
        terminal_insert_border: resolve(terminal_insert_border, base.terminal_insert_border),
    }
}

fn resolve(value: &Option<ColorDef>, fallback: Color) -> Color {
    value.clone().map(Color::from).unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_reach_list_and_border_styles_without_changing_other_colors() {
        let partial: PartialTheme = toml::from_str(
            r##"
                list_highlight_symbol = "#123456"
                list_disabled_item = 247
                border_focused = "magenta"
                help_title = { r = 42, g = 100, b = 200 }
            "##,
        )
        .unwrap();

        for base in [Theme::dark(), Theme::light()] {
            let theme = merge(&partial, base.clone());
            assert_eq!(theme.list_highlight_symbol, Color::Rgb(0x12, 0x34, 0x56));
            assert_eq!(theme.list_disabled_item, Color::Indexed(247));
            assert_eq!(theme.panel_border(false, true, true), Color::Magenta);
            assert_eq!(theme.panel_border(false, true, false), Color::Magenta);
            assert_eq!(theme.widget_border(true, true), Color::Magenta);
            assert_eq!(theme.help_title, Color::Rgb(42, 100, 200));
            assert_eq!(theme.accent, base.accent);
            assert_eq!(theme.key_hint_fg, base.key_hint_fg);
            assert_eq!(
                theme.panel_border(false, false, true),
                base.border_panel_primary
            );
            assert_eq!(theme.panel_border(true, true, true), base.resize_focused);
            assert_eq!(theme.widget_border(true, false), base.border_disabled);
        }
    }
}

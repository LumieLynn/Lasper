use std::sync::OnceLock;

use ratatui::style::Color;

use crate::ui::StatusLevel;

mod color_def;
mod config;
mod detect;

pub use config::load_theme;

// ---------------------------------------------------------------------------
// Global theme accessor
// ---------------------------------------------------------------------------

static THEME: OnceLock<Theme> = OnceLock::new();

/// Initialize the global theme. Must be called once before any render.
pub fn init_theme(theme: Theme) {
    let _ = THEME.set(theme);
}

/// Returns a reference to the active global theme.
pub fn theme() -> &'static Theme {
    THEME
        .get()
        .expect("Theme not initialized — call init_theme() before rendering")
}

// ---------------------------------------------------------------------------
// Theme struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Theme {
    // Text hierarchy
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_dim: Color,

    // Accents
    pub accent: Color,
    pub highlight: Color,
    pub highlight_secondary: Color,

    // Semantic states
    pub success: Color,
    pub warning: Color,
    pub error: Color,

    // Container states
    pub state_running: Color,
    pub state_stopped: Color,
    pub state_transitional: Color,

    // Borders
    pub border_focused: Color,
    pub border_unfocused: Color,
    pub border_disabled: Color,
    pub border_panel_primary: Color,
    pub border_panel_secondary: Color,

    // Tabs
    pub tab_active_focused: Color,
    pub tab_active_unfocused: Color,
    pub tab_inactive: Color,

    // Resize mode
    pub resize_focused: Color,
    pub resize_unfocused: Color,

    // Badges
    pub badge_root: Color,
    pub badge_readonly: Color,
    pub badge_cmd_mode: Color,

    // Status bar
    pub status_info: Color,
    pub status_success: Color,
    pub status_warning: Color,
    pub status_error: Color,
    pub key_hint_fg: Color,
    pub hint_fg: Color,

    // List
    pub list_selected_focused: Color,
    pub list_selected_unfocused: Color,
    pub list_unselected: Color,
    pub list_icon_alive: Color,
    pub list_icon_dead: Color,
    pub list_addr: Color,
    pub list_empty: Color,
    pub list_cursor_focused: Color,
    pub list_cursor_unfocused: Color,
    pub list_highlight_symbol: Color,
    pub list_disabled_item: Color,

    // Properties
    pub prop_enabled: Color,
    pub prop_disabled: Color,
    pub prop_unknown: Color,
    pub prop_transitional: Color,
    pub prop_pid: Color,
    pub prop_memory: Color,
    pub prop_default: Color,
    pub prop_readonly_yes: Color,
    pub prop_readonly_no: Color,

    // Charts
    pub chart_cpu: Color,
    pub chart_ram: Color,
    pub chart_axis: Color,

    // Buttons
    pub button_focused_fg: Color,
    pub button_focused_bg: Color,
    pub button_unfocused_fg: Color,
    pub button_border_focused: Color,
    pub button_border_unfocused: Color,

    // Dialogs
    pub dialog_border: Color,
    pub dialog_border_warn: Color,
    pub dialog_text: Color,
    pub dialog_host_border: Color,
    pub dialog_host_text: Color,

    // Config view
    pub config_section: Color,
    pub config_key: Color,
    pub config_value: Color,

    // Help
    pub help_key: Color,
    pub help_border: Color,
    pub help_title: Color,
    pub help_close_hint: Color,

    // Wizard
    pub wizard_border: Color,
    pub wizard_footer: Color,

    // Confirm / Cancel
    pub confirm_hint: Color,
    pub cancel_hint: Color,

    // Editor
    pub editor_error: Color,

    // Terminal
    pub terminal_insert_border: Color,
}

impl Theme {
    /// Dark theme: terminal-native ANSI palette. No custom RGB, no gray.
    pub fn dark() -> Self {
        Self {
            // --- Text ---
            text_primary: Color::White,
            text_secondary: Color::White,
            text_dim: Color::White,

            // --- Accents ---
            accent: Color::Cyan,
            highlight: Color::Yellow,
            highlight_secondary: Color::White,

            // --- Semantic ---
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,

            // --- Container states ---
            state_running: Color::Green,
            state_stopped: Color::White,
            state_transitional: Color::Cyan,

            // --- Borders ---
            border_focused: Color::Cyan,
            border_unfocused: Color::White,
            border_disabled: Color::White,
            border_panel_primary: Color::White,
            border_panel_secondary: Color::White,

            // --- Tabs ---
            tab_active_focused: Color::Yellow,
            tab_active_unfocused: Color::White,
            tab_inactive: Color::White,

            // --- Resize ---
            resize_focused: Color::Yellow,
            resize_unfocused: Color::Yellow,

            // --- Badges ---
            badge_root: Color::Green,
            badge_readonly: Color::Yellow,
            badge_cmd_mode: Color::Yellow,

            // --- Status bar ---
            status_info: Color::White,
            status_success: Color::Green,
            status_warning: Color::Yellow,
            status_error: Color::Red,
            key_hint_fg: Color::Cyan,
            hint_fg: Color::White,

            // --- List ---
            list_selected_focused: Color::Yellow,
            list_selected_unfocused: Color::Yellow,
            list_unselected: Color::White,
            list_icon_alive: Color::Green,
            list_icon_dead: Color::White,
            list_addr: Color::White,
            list_empty: Color::White,
            list_cursor_focused: Color::Yellow,
            list_cursor_unfocused: Color::White,
            list_highlight_symbol: Color::Yellow,
            list_disabled_item: Color::White,

            // --- Properties ---
            prop_enabled: Color::Green,
            prop_disabled: Color::Red,
            prop_unknown: Color::Yellow,
            prop_transitional: Color::Cyan,
            prop_pid: Color::Magenta,
            prop_memory: Color::Blue,
            prop_default: Color::White,
            prop_readonly_yes: Color::Yellow,
            prop_readonly_no: Color::White,

            // --- Charts ---
            chart_cpu: Color::Cyan,
            chart_ram: Color::Magenta,
            chart_axis: Color::White,

            // --- Buttons ---
            button_focused_fg: Color::Black,
            button_focused_bg: Color::Cyan,
            button_unfocused_fg: Color::White,
            button_border_focused: Color::Cyan,
            button_border_unfocused: Color::White,

            // --- Dialogs ---
            dialog_border: Color::Cyan,
            dialog_border_warn: Color::Yellow,
            dialog_text: Color::White,
            dialog_host_border: Color::White,
            dialog_host_text: Color::White,

            // --- Config view ---
            config_section: Color::Cyan,
            config_key: Color::Yellow,
            config_value: Color::White,

            // --- Help ---
            help_key: Color::Yellow,
            help_border: Color::Cyan,
            help_title: Color::Cyan,
            help_close_hint: Color::White,

            // --- Wizard ---
            wizard_border: Color::Cyan,
            wizard_footer: Color::White,

            // --- Confirm / Cancel ---
            confirm_hint: Color::Green,
            cancel_hint: Color::Red,

            // --- Editor ---
            editor_error: Color::Red,

            // --- Terminal ---
            terminal_insert_border: Color::Green,
        }
    }

    /// Light theme: dark-on-light. Same terminal-native palette, inverted text.
    pub fn light() -> Self {
        Self {
            // --- Text ---
            text_primary: Color::Black,
            text_secondary: Color::Black,
            text_dim: Color::Black,

            // --- Accents: blue reads better than cyan on white ---
            accent: Color::Blue,
            highlight: Color::Yellow,
            highlight_secondary: Color::Black,

            // --- Semantic ---
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,

            // --- Container states ---
            state_running: Color::Green,
            state_stopped: Color::Black,
            state_transitional: Color::Blue,

            // --- Borders ---
            border_focused: Color::Blue,
            border_unfocused: Color::Black,
            border_disabled: Color::Black,
            border_panel_primary: Color::Black,
            border_panel_secondary: Color::Black,

            // --- Tabs ---
            tab_active_focused: Color::Yellow,
            tab_active_unfocused: Color::Black,
            tab_inactive: Color::Black,

            // --- Resize ---
            resize_focused: Color::Yellow,
            resize_unfocused: Color::Yellow,

            // --- Badges ---
            badge_root: Color::Green,
            badge_readonly: Color::Yellow,
            badge_cmd_mode: Color::Yellow,

            // --- Status bar ---
            status_info: Color::Black,
            status_success: Color::Green,
            status_warning: Color::Yellow,
            status_error: Color::Red,
            key_hint_fg: Color::Blue,
            hint_fg: Color::Black,

            // --- List ---
            list_selected_focused: Color::Yellow,
            list_selected_unfocused: Color::Yellow,
            list_unselected: Color::Black,
            list_icon_alive: Color::Green,
            list_icon_dead: Color::Black,
            list_addr: Color::Black,
            list_empty: Color::Black,
            list_cursor_focused: Color::Yellow,
            list_cursor_unfocused: Color::Black,
            list_highlight_symbol: Color::Yellow,
            list_disabled_item: Color::Black,

            // --- Properties ---
            prop_enabled: Color::Green,
            prop_disabled: Color::Red,
            prop_unknown: Color::Yellow,
            prop_transitional: Color::Blue,
            prop_pid: Color::Magenta,
            prop_memory: Color::Blue,
            prop_default: Color::Black,
            prop_readonly_yes: Color::Yellow,
            prop_readonly_no: Color::Black,

            // --- Charts ---
            chart_cpu: Color::Blue,
            chart_ram: Color::Magenta,
            chart_axis: Color::Black,

            // --- Buttons ---
            button_focused_fg: Color::White,
            button_focused_bg: Color::Blue,
            button_unfocused_fg: Color::Black,
            button_border_focused: Color::Blue,
            button_border_unfocused: Color::Black,

            // --- Dialogs ---
            dialog_border: Color::Blue,
            dialog_border_warn: Color::Yellow,
            dialog_text: Color::Black,
            dialog_host_border: Color::Black,
            dialog_host_text: Color::Black,

            // --- Config view ---
            config_section: Color::Blue,
            config_key: Color::Yellow,
            config_value: Color::Black,

            // --- Help ---
            help_key: Color::Yellow,
            help_border: Color::Blue,
            help_title: Color::Blue,
            help_close_hint: Color::Black,

            // --- Wizard ---
            wizard_border: Color::Blue,
            wizard_footer: Color::Black,

            // --- Confirm / Cancel ---
            confirm_hint: Color::Green,
            cancel_hint: Color::Red,

            // --- Editor ---
            editor_error: Color::Red,

            // --- Terminal ---
            terminal_insert_border: Color::Green,
        }
    }

    // -----------------------------------------------------------------------
    // Helper methods (replace the free functions in ui/mod.rs)
    // -----------------------------------------------------------------------

    /// Border color for a top-level panel given resize/focus state.
    /// `is_primary` = true for the container list, false for detail/terminal.
    pub fn panel_border(&self, resize_mode: bool, focused: bool, is_primary: bool) -> Color {
        if resize_mode {
            if focused {
                self.resize_focused
            } else {
                self.resize_unfocused
            }
        } else if focused {
            self.accent
        } else if is_primary {
            self.border_panel_primary
        } else {
            self.border_panel_secondary
        }
    }

    /// Border color for an inner widget given focus/enabled state.
    pub fn widget_border(&self, focused: bool, enabled: bool) -> Color {
        if !enabled {
            self.border_disabled
        } else if focused {
            self.accent
        } else {
            self.border_unfocused
        }
    }

    /// Color for a status bar message level.
    pub fn status_color(&self, level: &StatusLevel) -> Color {
        match level {
            StatusLevel::Info => self.status_info,
            StatusLevel::Success => self.status_success,
            StatusLevel::Warn => self.status_warning,
            StatusLevel::Error => self.status_error,
        }
    }
}

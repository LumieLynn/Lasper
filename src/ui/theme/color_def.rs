use serde::Deserialize;

/// Serializable color representation for theme config files.
///
/// Supports three forms in TOML:
/// ```toml
/// accent = "cyan"                      # named
/// accent = { r = 80, g = 200, b = 220 } # RGB
/// accent = 51                           # ANSI 256-color index
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ColorDef {
    Named(String),
    Rgb { r: u8, g: u8, b: u8 },
    Indexed(u8),
}

impl From<ColorDef> for ratatui::style::Color {
    fn from(def: ColorDef) -> Self {
        match def {
            ColorDef::Named(s) => parse_named_color(&s),
            ColorDef::Rgb { r, g, b } => ratatui::style::Color::Rgb(r, g, b),
            ColorDef::Indexed(i) => ratatui::style::Color::Indexed(i),
        }
    }
}

impl From<ratatui::style::Color> for ColorDef {
    fn from(c: ratatui::style::Color) -> Self {
        match c {
            ratatui::style::Color::Rgb(r, g, b) => ColorDef::Rgb { r, g, b },
            ratatui::style::Color::Indexed(i) => ColorDef::Indexed(i),
            _ => ColorDef::Named(format!("{:?}", c).to_lowercase()),
        }
    }
}

fn parse_named_color(s: &str) -> ratatui::style::Color {
    match s.to_lowercase().as_str() {
        "black" => ratatui::style::Color::Black,
        "red" => ratatui::style::Color::Red,
        "green" => ratatui::style::Color::Green,
        "yellow" => ratatui::style::Color::Yellow,
        "blue" => ratatui::style::Color::Blue,
        "magenta" => ratatui::style::Color::Magenta,
        "cyan" => ratatui::style::Color::Cyan,
        "gray" => ratatui::style::Color::Gray,
        "dark_gray" | "darkgray" => ratatui::style::Color::DarkGray,
        "light_red" | "lightred" => ratatui::style::Color::LightRed,
        "light_green" | "lightgreen" => ratatui::style::Color::LightGreen,
        "light_yellow" | "lightyellow" => ratatui::style::Color::LightYellow,
        "light_blue" | "lightblue" => ratatui::style::Color::LightBlue,
        "light_magenta" | "lightmagenta" => ratatui::style::Color::LightMagenta,
        "light_cyan" | "lightcyan" => ratatui::style::Color::LightCyan,
        "white" => ratatui::style::Color::White,
        // Extended aliases
        "orange" => ratatui::style::Color::Rgb(255, 140, 0),
        "reset" | "default" => ratatui::style::Color::Reset,
        // Catch-all: try to parse as Rgb from hex-like string or fallback to White
        other => {
            if let Some(hex) = other.strip_prefix('#') {
                if hex.len() == 6 {
                    if let (Ok(r), Ok(g), Ok(b)) = (
                        u8::from_str_radix(&hex[0..2], 16),
                        u8::from_str_radix(&hex[2..4], 16),
                        u8::from_str_radix(&hex[4..6], 16),
                    ) {
                        return ratatui::style::Color::Rgb(r, g, b);
                    }
                }
            }
            ratatui::style::Color::White
        }
    }
}

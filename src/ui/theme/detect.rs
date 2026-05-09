/// Detect whether the terminal has a light background.
///
/// Checks in order:
/// 1. `COLORFGBG` env var (set by tmux, rxvt, screen, etc.)
///    Format: `<fg>;<bg>` where values are ANSI color indices (0-15).
///    bg == 7 (white) or bg == 15 (bright white) suggests a light terminal.
/// 2. `TERM_BG` env var (explicit "light" or "dark")
/// 3. Falls back to false (dark) — the overwhelming default.
pub fn is_light_background() -> bool {
    if let Ok(val) = std::env::var("COLORFGBG") {
        if let Some(bg) = val
            .split(';')
            .nth(1)
            .and_then(|s| s.parse::<u8>().ok())
        {
            if bg == 7 || bg == 15 {
                return true;
            }
        }
    }

    if let Ok(term_bg) = std::env::var("TERM_BG") {
        return term_bg.eq_ignore_ascii_case("light");
    }

    false
}

use rio_vt::config::colors::ColorRgb;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use super::cursor::{CursorStyle, HostCursorStyle};

const HOST_APPEARANCE_QUERY: &[u8] = b"\x1b[>0q\x1b]11;?\x07\x1bP$q q\x1b\\";
const QUERY_TIMEOUT_MS: libc::c_int = 100;

#[derive(Default)]
pub(crate) struct HostAppearance {
    pub background: Option<ColorRgb>,
    pub cursor_style: Option<HostCursorStyle>,
}

pub(crate) fn default_query_colors() -> [ColorRgb; 3] {
    if environment_background_is_light() {
        [rgb(0, 0, 0), rgb(255, 255, 255), rgb(0, 0, 0)]
    } else {
        [rgb(255, 255, 255), rgb(0, 0, 0), rgb(255, 255, 255)]
    }
}

pub(crate) fn query_colors_with_background(background: ColorRgb) -> [ColorRgb; 3] {
    let mut colors = default_query_colors();
    colors[1] = background;
    colors
}

/// Query background color and cursor style before taking over the terminal.
/// Terminal identity resolves differences in cursor-query semantics. All
/// unsupported queries remain unset within the same bounded wait.
pub(crate) fn probe_host_appearance(output: &mut impl Write) -> HostAppearance {
    read_host_appearance(output).unwrap_or_default()
}

fn read_host_appearance(output: &mut impl Write) -> Option<HostAppearance> {
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1 {
        return None;
    }

    let input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY)
        .open("/dev/tty")
        .ok()?;

    output.write_all(HOST_APPEARANCE_QUERY).ok()?;
    output.flush().ok()?;

    let deadline = Instant::now() + Duration::from_millis(QUERY_TIMEOUT_MS as u64);
    let mut response = [0_u8; 512];
    let mut length = 0;
    while length < response.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut poll_fd = libc::pollfd {
            fd: input.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, remaining.as_millis().max(1) as i32) };
        if ready <= 0 || poll_fd.revents & libc::POLLIN == 0 {
            break;
        }
        match (&input).read(&mut response[length..]) {
            Ok(0) => break,
            Ok(read) => length += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        if parse_background_response(&response[..length]).is_some()
            && cursor_response(&response[..length]).is_some()
            && terminal_identity_response(&response[..length]).is_some()
        {
            break;
        }
    }

    Some(HostAppearance {
        background: parse_background_response(&response[..length]),
        cursor_style: cursor_response(&response[..length]).flatten(),
    })
}

fn dcs_responses(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.iter().enumerate().filter_map(move |(offset, byte)| {
        let start = match byte {
            0x90 => offset + 1,
            0x1b if bytes.get(offset + 1) == Some(&b'P') => offset + 2,
            _ => return None,
        };
        let rest = &bytes[start..];
        let end = rest.iter().enumerate().position(|(offset, byte)| {
            *byte == 0x9c || (*byte == 0x1b && rest.get(offset + 1) == Some(&b'\\'))
        })?;
        Some(&rest[..end])
    })
}

fn terminal_identity_response(bytes: &[u8]) -> Option<&[u8]> {
    dcs_responses(bytes).find_map(|body| body.strip_prefix(b">|"))
}

/// A complete DECRQSS response, including an explicit unsupported response.
fn cursor_response(bytes: &[u8]) -> Option<Option<HostCursorStyle>> {
    let is_kitty = terminal_identity_response(bytes)
        .is_some_and(|identity| identity.starts_with(b"kitty(") && identity.ends_with(b")"));
    dcs_responses(bytes).find_map(|body| {
        if body == b"0$r" {
            return Some(None);
        }
        let parameter = body
            .strip_prefix(b"1$r")?
            .strip_suffix(b" q")
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.parse::<u8>().ok())?;
        // Kitty reports its unoverridden configuration as 1, while an explicit
        // blinking block is reported as 0. Replaying 1 forces a block instead
        // of restoring the configured shape (for example, a beam).
        if is_kitty && parameter == 1 {
            Some(Some(HostCursorStyle::TerminalDefault))
        } else {
            Some(CursorStyle::from_parameter(parameter.max(1)).map(HostCursorStyle::Explicit))
        }
    })
}

fn parse_background_response(bytes: &[u8]) -> Option<ColorRgb> {
    const PREFIX: &[u8] = b"\x1b]11;rgb:";
    let start = bytes
        .windows(PREFIX.len())
        .position(|window| window == PREFIX)?
        + PREFIX.len();
    let end = bytes[start..]
        .iter()
        .position(|byte| *byte == b'\x07' || *byte == 0x9c)
        .map(|offset| start + offset)
        .or_else(|| {
            bytes[start..]
                .windows(2)
                .position(|window| window == b"\x1b\\")
                .map(|offset| start + offset)
        })?;
    let components = bytes[start..end]
        .split(|byte| *byte == b'/')
        .collect::<Vec<_>>();
    if components.len() != 3 || components.iter().any(|component| component.is_empty()) {
        return None;
    }
    let values = components
        .into_iter()
        .map(parse_color_component)
        .collect::<Option<Vec<_>>>()?;
    Some(rgb(values[0], values[1], values[2]))
}

fn parse_color_component(component: &[u8]) -> Option<u8> {
    if component.len() > 4 {
        return None;
    }
    let value = u16::from_str_radix(std::str::from_utf8(component).ok()?, 16).ok()?;
    let maximum = (1_u32 << (component.len() * 4)) - 1;
    Some((u32::from(value) * 255 / maximum) as u8)
}

fn environment_background_is_light() -> bool {
    if let Ok(value) = std::env::var("COLORFGBG") {
        if value
            .split(';')
            .nth(1)
            .and_then(|background| background.parse::<u8>().ok())
            .is_some_and(|background| background == 7 || background == 15)
        {
            return true;
        }
    }
    std::env::var("TERM_BG")
        .map(|value| value.eq_ignore_ascii_case("light"))
        .unwrap_or(false)
}

const fn rgb(r: u8, g: u8, b: u8) -> ColorRgb {
    ColorRgb { r, g, b }
}

#[cfg(test)]
mod tests {
    use super::{cursor_response, parse_background_response, rgb};
    use crate::tui::foundation::term::{CursorStyle, HostCursor, HostCursorStyle, TerminalCore};

    #[test]
    fn cursor_query_requires_a_complete_successful_status_response() {
        assert_eq!(
            cursor_response(b"noise\x1b]11;rgb:00/00/00\x07\x1bP1$r6 q\x1b\\"),
            Some(Some(HostCursorStyle::Explicit(CursorStyle::SteadyBar)))
        );
        assert_eq!(cursor_response(b"\x1bP1$r5 q"), None);
        assert_eq!(cursor_response(b"\x1bP0$r\x1b\\"), Some(None));
        assert_eq!(cursor_response(b"\x1bP1$r99 q\x1b\\"), Some(None));
        assert_eq!(
            cursor_response(b"\x901$r4 q\x9c"),
            Some(Some(HostCursorStyle::Explicit(
                CursorStyle::SteadyUnderline
            )))
        );
    }

    #[test]
    fn kitty_default_cursor_is_restored_after_an_embedded_alternate_screen() {
        // These are Kitty's replies with no explicit cursor override. A
        // successful 1 q report represents its configured default, not a block.
        let response = b"\x1bP>|kitty(0.45.0)\x1b\\\x1bP1$r1 q\x1b\\";
        let original = cursor_response(response).unwrap();
        assert_eq!(original, Some(HostCursorStyle::TerminalDefault));
        let mut cursor = HostCursor::default();
        cursor.set_original(original);
        let mut terminal = TerminalCore::new(24, 80, 100);
        let mut output = Vec::new();
        terminal.process(b"\x1b[?1049h\x1b[6 q", &mut Vec::new());
        cursor
            .apply(&mut output, terminal.screen().cursor_style())
            .unwrap();
        terminal.process(b"\x1b[2 q\x1b[?1049l", &mut Vec::new());
        cursor
            .apply(&mut output, terminal.screen().cursor_style())
            .unwrap();
        cursor.restore(&mut output).unwrap();
        assert_eq!(output, b"\x1b[6 q\x1b[0 q");

        // A multiplexer's own response must take precedence over any inherited
        // Kitty environment; its explicit blinking block remains a block.
        assert_eq!(
            cursor_response(b"\x1bP>|tmux 3.5\x1b\\\x1bP1$r1 q\x1b\\"),
            Some(Some(HostCursorStyle::Explicit(CursorStyle::BlinkingBlock)))
        );
        assert_eq!(
            cursor_response(b"\x1bP1$r0 q\x1b\\\x90>|kitty(0.45.0)\x9c"),
            Some(Some(HostCursorStyle::Explicit(CursorStyle::BlinkingBlock)))
        );
    }

    #[test]
    fn parses_bel_terminated_background_response() {
        assert_eq!(
            parse_background_response(b"\x1b]11;rgb:4e4e/5a5a/7b7b\x07"),
            Some(rgb(78, 90, 123))
        );
    }

    #[test]
    fn parses_st_terminated_background_response() {
        assert_eq!(
            parse_background_response(b"noise\x1b]11;rgb:ff/00/80\x1b\\"),
            Some(rgb(255, 0, 128))
        );
    }

    #[test]
    fn rejects_malformed_background_response() {
        assert_eq!(parse_background_response(b"\x1b]11;rgb:ff/00\x07"), None);
        assert_eq!(
            parse_background_response(b"\x1b]11;rgb:fffff/00/00\x07"),
            None
        );
    }
}

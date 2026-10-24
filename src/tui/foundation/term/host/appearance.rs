//! Capture the outer terminal's colors and cursor before starting the event loop.

use rio_vt::config::colors::ColorRgb;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use super::HostCursorStyle;
use crate::tui::foundation::term::cursor::{decode_query_response, has_terminal_identity};

const HOST_APPEARANCE_QUERY: &[u8] = b"\x1b[>0q\x1b]10;?\x07\x1b]11;?\x07\x1bP$q q\x1b\\";
const QUERY_TIMEOUT_MS: libc::c_int = 100;

#[derive(Default)]
pub(crate) struct HostAppearance {
    pub foreground: Option<ColorRgb>,
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

pub(crate) fn query_colors_with_defaults(
    foreground: Option<ColorRgb>,
    background: Option<ColorRgb>,
) -> [ColorRgb; 3] {
    let mut colors = default_query_colors();
    if let Some(foreground) = foreground {
        colors[0] = foreground;
        colors[2] = foreground;
    }
    if let Some(background) = background {
        colors[1] = background;
    }
    colors
}

/// Query default colors and cursor style before taking over the terminal.
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
        if parse_color_response(&response[..length], b"\x1b]10;rgb:").is_some()
            && parse_color_response(&response[..length], b"\x1b]11;rgb:").is_some()
            && decode_query_response(&response[..length]).is_some()
            && has_terminal_identity(&response[..length])
        {
            break;
        }
    }

    Some(HostAppearance {
        foreground: parse_color_response(&response[..length], b"\x1b]10;rgb:"),
        background: parse_color_response(&response[..length], b"\x1b]11;rgb:"),
        cursor_style: decode_query_response(&response[..length]).flatten(),
    })
}

fn parse_color_response(bytes: &[u8], prefix: &[u8]) -> Option<ColorRgb> {
    let start = bytes
        .windows(prefix.len())
        .position(|window| window == prefix)?
        + prefix.len();
    let end = bytes[start..]
        .iter()
        .enumerate()
        .position(|(offset, byte)| {
            matches!(byte, 0x07 | 0x9c)
                || (*byte == 0x1b && bytes.get(start + offset + 1) == Some(&b'\\'))
        })?
        + start;
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
    use super::{parse_color_response, query_colors_with_defaults, rgb};

    #[test]
    fn parses_bel_terminated_background_response() {
        assert_eq!(
            parse_color_response(b"\x1b]11;rgb:4e4e/5a5a/7b7b\x07", b"\x1b]11;rgb:"),
            Some(rgb(78, 90, 123))
        );
    }

    #[test]
    fn parses_st_terminated_background_response() {
        assert_eq!(
            parse_color_response(b"noise\x1b]11;rgb:ff/00/80\x1b\\", b"\x1b]11;rgb:"),
            Some(rgb(255, 0, 128))
        );
    }

    #[test]
    fn rejects_malformed_background_response() {
        assert_eq!(
            parse_color_response(b"\x1b]11;rgb:ff/00\x07", b"\x1b]11;rgb:"),
            None
        );
        assert_eq!(
            parse_color_response(b"\x1b]11;rgb:fffff/00/00\x07", b"\x1b]11;rgb:"),
            None
        );
    }

    #[test]
    fn foreground_and_background_queries_preserve_the_hosts_actual_colors() {
        let response = b"\x1b]11;rgb:ee/ee/ee\x07\x1b]10;rgb:33/44/55\x1b\\";
        let foreground = parse_color_response(response, b"\x1b]10;rgb:");
        let background = parse_color_response(response, b"\x1b]11;rgb:");
        assert_eq!(foreground, Some(rgb(51, 68, 85)));
        assert_eq!(background, Some(rgb(238, 238, 238)));
        assert_eq!(
            query_colors_with_defaults(foreground, background),
            [rgb(51, 68, 85), rgb(238, 238, 238), rgb(51, 68, 85)]
        );
    }

    #[test]
    fn color_responses_stop_at_the_first_terminator_in_a_batch() {
        for response in [
            b"\x1b]10;rgb:33/44/55\x1b\\\x1b]11;rgb:ee/ee/ee\x07".as_slice(),
            b"\x1b]10;rgb:33/44/55\x07\x1b]11;rgb:ee/ee/ee\x1b\\",
        ] {
            assert_eq!(
                parse_color_response(response, b"\x1b]10;rgb:"),
                Some(rgb(51, 68, 85))
            );
            assert_eq!(
                parse_color_response(response, b"\x1b]11;rgb:"),
                Some(rgb(238, 238, 238))
            );
        }
    }
}

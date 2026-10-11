//! Concise command inspection, without changing the complete property display.
//!
//! systemctl joins argv without escaping, so tool text cannot recover argument
//! boundaries. Keep that text as display only, and fall back to the complete
//! property whenever its known ExecStatus envelope is ambiguous or unfamiliar.

use std::borrow::Cow;

use crate::domain::inspection::{PropertyEntry, PropertyValue};

pub(crate) fn command_summary(entry: &PropertyEntry) -> Cow<'_, str> {
    let summary = match &entry.value {
        PropertyValue::CanonicalText(text) => tool_summary(text),
        PropertyValue::Array(commands) => commands
            .iter()
            .map(typed_command)
            .collect::<Option<Vec<_>>>()
            .map(|commands| commands.join("\n")),
        _ => None,
    };
    summary.map_or(Cow::Borrowed(&entry.display), Cow::Owned)
}

fn typed_command(value: &PropertyValue) -> Option<String> {
    let PropertyValue::Structure(fields) = value else {
        return None;
    };
    if fields.len() != 10 {
        return None;
    }
    let PropertyValue::String(path) = &fields[0] else {
        return None;
    };
    let arguments = string_array(&fields[1])?;
    let flags = match &fields[2] {
        PropertyValue::Bool(true) => "ignore_errors=yes".into(),
        PropertyValue::Bool(false) => String::new(),
        value => flags_label(&string_array(value)?),
    };
    Some(command_label(path, &arguments, &flags))
}

fn string_array(value: &PropertyValue) -> Option<String> {
    let PropertyValue::Array(array) = value else {
        return None;
    };
    if array.element_signature() != "s" {
        return None;
    }
    array
        .iter()
        .map(|value| match value {
            PropertyValue::String(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(|values| values.join(" "))
}

fn flags_label(flags: &str) -> String {
    if flags.is_empty() {
        String::new()
    } else {
        format!("flags={flags}")
    }
}

fn command_label(path: &str, arguments: &str, flags: &str) -> String {
    let mut command = if arguments.is_empty() {
        path.to_owned()
    } else {
        arguments.to_owned()
    };
    if !arguments.is_empty()
        && !arguments
            .strip_prefix(path)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    {
        command.push_str(&format!(" [executable={path}]"));
    }
    if !flags.is_empty() {
        command.push_str(&format!(" [{flags}]"));
    }
    command
}

fn tool_summary(text: &str) -> Option<String> {
    text.trim()
        .strip_prefix("{ path=")?
        .strip_suffix(" }")?
        .split(" } { path=")
        .map(tool_command)
        .collect::<Option<Vec<_>>>()
        .map(|commands| commands.join("\n"))
}

fn tool_command(text: &str) -> Option<String> {
    let (command, status) = text.rsplit_once(" ; start_time=[")?;
    let (_, status) = status.split_once("] ; stop_time=[")?;
    let (_, status) = status.split_once("] ; pid=")?;
    let (pid, status) = status.split_once(" ; code=")?;
    pid.parse::<u32>().ok()?;
    let (code, status) = status.split_once(" ; status=")?;
    if code.is_empty()
        || !code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '(' | ')' | '-'))
    {
        return None;
    }
    let (exit_status, signal) = status.split_once('/').unwrap_or((status, ""));
    exit_status.parse::<i32>().ok()?;
    if !signal.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }

    let (command, flags) = if let Some((command, ignore)) = command.rsplit_once(" ; ignore_errors=")
    {
        let flags = match ignore {
            "yes" => "ignore_errors=yes".into(),
            "no" => String::new(),
            _ => return None,
        };
        (command, flags)
    } else {
        let (command, flags) = command.rsplit_once(" ; flags=")?;
        if !flags
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | ' '))
        {
            return None;
        }
        (command, flags_label(flags))
    };
    let (path, arguments) = command.split_once(" ; argv[]=")?;
    // Reserved envelope markers inside a path/argument cannot be interpreted
    // reliably after systemctl's unescaped join. Do not shorten such a value.
    if path.is_empty()
        || [path, arguments].iter().any(|value| {
            [
                "{ path=",
                " ; argv[]=",
                " ; ignore_errors=",
                " ; flags=",
                " ; start_time=[",
            ]
            .iter()
            .any(|marker| value.contains(marker))
        })
    {
        return None;
    }
    Some(command_label(path, arguments, &flags))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inspection::PropertyArray;

    fn command(path: &str, arguments: &[&str], flags: PropertyValue) -> PropertyValue {
        PropertyValue::Structure(vec![
            PropertyValue::String(path.into()),
            PropertyValue::Array(PropertyArray {
                element_signature: "s".into(),
                values: arguments
                    .iter()
                    .map(|text| PropertyValue::String((*text).into()))
                    .collect(),
            }),
            flags,
            PropertyValue::U64(0),
            PropertyValue::U64(0),
            PropertyValue::U64(0),
            PropertyValue::U64(0),
            PropertyValue::U32(42),
            PropertyValue::I32(1),
            PropertyValue::I32(0),
        ])
    }

    fn tool_command(path: &str, arguments: &str, flags: &str) -> String {
        format!("{{ path={path} ; argv[]={arguments} ; {flags} ; start_time=[n/a] ; stop_time=[n/a] ; pid=42 ; code=exited ; status=0 }}")
    }

    #[test]
    fn typed_and_tool_commands_share_a_concise_read_only_display() {
        let path = "/usr/bin/systemd-nspawn";
        let arguments = [path, "--boot", "--machine=archlinux"];
        let canonical = tool_command(path, &arguments.join(" "), "ignore_errors=no");
        let typed = PropertyEntry::typed(
            PropertyValue::Array(PropertyArray {
                element_signature: "(sasbttttuii)".into(),
                values: vec![command(path, &arguments, PropertyValue::Bool(false))],
            }),
            &canonical,
        );
        let tools = PropertyEntry::text(&canonical);
        assert_eq!(command_summary(&typed), arguments.join(" "));
        assert_eq!(command_summary(&typed), command_summary(&tools));
        assert_eq!(typed.display, canonical);
        assert_eq!(tools.display, canonical);
    }

    #[test]
    fn multiple_commands_keep_executable_overrides_and_execution_flags() {
        let canonical = format!(
            "{} {}",
            tool_command(
                "/usr/bin/first",
                "custom-name argument",
                "flags=ignore-failure no-env-expand"
            ),
            tool_command("/usr/bin/second", "", "ignore_errors=yes"),
        );
        assert_eq!(command_summary(&PropertyEntry::text(canonical)),
            "custom-name argument [executable=/usr/bin/first] [flags=ignore-failure no-env-expand]\n/usr/bin/second [ignore_errors=yes]");
    }

    #[test]
    fn unfamiliar_or_ambiguous_text_keeps_the_full_property() {
        for text in [
            "/usr/bin/plain --argument".to_owned(),
            "{ path=/usr/bin/plain ; argv[]=/usr/bin/plain ; unknown=yes }".into(),
            tool_command(
                "/usr/bin/plain",
                "plain ; flags=payload",
                "ignore_errors=no",
            ),
            tool_command(
                "/usr/bin/plain",
                "plain } { path=payload",
                "ignore_errors=no",
            ),
        ] {
            assert_eq!(command_summary(&PropertyEntry::text(&text)), text);
        }
        let entry = PropertyEntry::typed(PropertyValue::Structure(Vec::new()), "opaque");
        assert_eq!(command_summary(&entry), "opaque");
    }
}

//! Help pages share their layout, while each command lists only its own options.

use std::fmt;

use super::BUILD_VERSION;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HelpTopic {
    Application,
    Shell,
    Launch,
}

pub(super) fn text(topic: HelpTopic) -> String {
    match topic {
        HelpTopic::Application => application_help(),
        HelpTopic::Shell => shell_help(),
        HelpTopic::Launch => launch_help(),
    }
    .to_string()
}

fn application_help() -> HelpPage<'static> {
    HelpPage::new(format!("lasper {BUILD_VERSION} - A TUI for systemd-nspawn containers"))
        .usage("lasper [OPTIONS]")
        .usage("lasper [OPTIONS] COMMAND [OPTIONS] ...")
        .section(
            HelpSection::new("COMMANDS")
                .entry("shell USER@MACHINE [COMMAND...]", "Open a shell or run a guest command in your terminal")
                .entry("launch USER@MACHINE COMMAND...", "Start a container if needed and run a guest command"),
        )
        .section(
            execution_options(true)
                .entry("-h, --help", "Show help")
                .entry("-v, --version", "Show version information")
                .paragraph("Execution options may precede shell or launch, or follow the command name. launch does not accept elevation options. Run lasper shell --help or lasper launch --help for session options."),
        )
        .section(
            HelpSection::new("CONFIGURATION")
                .paragraph("Defaults are read from ~/.config/lasper/config.toml. -e and -s enable their settings for this invocation; --elevate=METHOD also selects the launcher. See CONFIGURATION.md for all settings."),
        )
}

fn shell_help() -> HelpPage<'static> {
    HelpPage::new(format!("lasper {BUILD_VERSION} - Interactive container shell"))
        .usage("lasper [OPTIONS] shell [OPTIONS] USER@MACHINE [COMMAND [ARG...]]")
        .section(
            display_options(execution_options(true))
                .entry("--quiet", "Suppress Wayland fallback and detach notices")
                .entry("-h, --help", "Show shell help"),
        )
        .section(
            guest_arguments()
                .paragraph("With no executable, open the selected user's default shell. A trailing -- after USER@MACHINE also opens the default shell.")
                .paragraph("The shell uses your terminal. Press Ctrl+] three times within one second to detach. Automatic Wayland selection falls back to a terminal-only shell if validation fails; explicit --wayland=DISPLAY does not."),
        )
        .section(
            HelpSection::new("EXAMPLES")
                .paragraph("lasper shell user@machine")
                .paragraph("lasper -e shell --with-x11 user@machine /usr/bin/kitty")
                .paragraph("lasper shell -- user@machine /bin/echo --help"),
        )
}

fn launch_help() -> HelpPage<'static> {
    HelpPage::new(format!("lasper {BUILD_VERSION} - Launch a container application"))
        .usage("lasper [OPTIONS] launch [OPTIONS] USER@MACHINE COMMAND [ARG...]")
        .section(
            display_options(execution_options(false))
                .entry("-h, --help", "Show launch help"),
        )
        .section(
            guest_arguments()
                .paragraph("launch is intended for Terminal=false desktop entries. It starts a stopped machine before preparing display access and executing the guest command. It uses the caller's systemd/polkit authority, without the elevated daemon.")
                .paragraph("Guest output goes to inherited stdout; Lasper waits for the command to finish and returns its exit status. Launch failures also request a desktop notification. --elevate and --quiet are not accepted."),
        )
        .section(
            HelpSection::new("EXAMPLES")
                .paragraph("lasper launch user@machine /usr/bin/kitty --single-instance")
                .paragraph("lasper launch --with-x11=:0 user@machine /usr/bin/kitty"),
        )
}

fn execution_options(elevation: bool) -> HelpSection<'static> {
    let mut options = HelpSection::new("OPTIONS");
    if elevation {
        options = options
            .entry(
                "-e, --elevate",
                "Use the elevated daemon with the configured launcher",
            )
            .entry(
                "--elevate=METHOD",
                "Use the elevated daemon via sudo or run0",
            );
    }
    options.entry(
        "-s, --systemd-tools",
        "Use systemd command-line tools instead of Lasper's D-Bus client",
    )
}

fn display_options(options: HelpSection<'static>) -> HelpSection<'static> {
    options
        .entry(
            "--wayland[=DISPLAY]",
            "Use automatic Wayland selection, or select a socket",
        )
        .entry("--no-wayland", "Do not prepare Wayland access")
        .entry("--with-x11[=:N]", "Explicitly prepare Host X11 access")
}

fn guest_arguments() -> HelpSection<'static> {
    HelpSection::new("ARGUMENTS")
        .entry("USER@MACHINE", "The guest user and container name")
        .entry("COMMAND [ARG...]", "An absolute guest executable path and its arguments")
        .entry("--", "End Lasper option parsing (optional)")
        .paragraph("Session options may appear before or after USER@MACHINE, until the executable. All arguments after the executable are passed unchanged to the guest, including --help and --.")
        .paragraph("Wayland is automatic by default when the current WAYLAND_DISPLAY has a configured bind. Host X11 is never enabled implicitly; --with-x11 uses the current DISPLAY, or the specified :N.")
}

struct HelpPage<'a> {
    heading: String,
    usage: Vec<&'a str>,
    sections: Vec<HelpSection<'a>>,
}

impl<'a> HelpPage<'a> {
    fn new(heading: String) -> Self {
        Self {
            heading,
            usage: Vec::new(),
            sections: Vec::new(),
        }
    }

    fn usage(mut self, syntax: &'a str) -> Self {
        self.usage.push(syntax);
        self
    }

    fn section(mut self, section: HelpSection<'a>) -> Self {
        self.sections.push(section);
        self
    }
}

impl fmt::Display for HelpPage<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "{}", self.heading)?;
        writeln!(formatter)?;
        writeln!(formatter, "USAGE:")?;
        for syntax in &self.usage {
            writeln!(formatter, "    {syntax}")?;
        }
        for section in &self.sections {
            writeln!(formatter)?;
            section.fmt(formatter)?;
        }
        Ok(())
    }
}

struct HelpSection<'a> {
    heading: &'a str,
    items: Vec<HelpItem<'a>>,
}

enum HelpItem<'a> {
    Entry(&'a str, &'a str),
    Paragraph(&'a str),
}

impl<'a> HelpSection<'a> {
    fn new(heading: &'a str) -> Self {
        Self {
            heading,
            items: Vec::new(),
        }
    }

    fn entry(mut self, syntax: &'a str, description: &'a str) -> Self {
        self.items.push(HelpItem::Entry(syntax, description));
        self
    }

    fn paragraph(mut self, text: &'a str) -> Self {
        self.items.push(HelpItem::Paragraph(text));
        self
    }
}

impl fmt::Display for HelpSection<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "{}:", self.heading)?;
        let syntax_width = self
            .items
            .iter()
            .filter_map(|item| match item {
                HelpItem::Entry(syntax, _) => Some(syntax.len()),
                HelpItem::Paragraph(_) => None,
            })
            .max()
            .unwrap_or(0);
        for (index, item) in self.items.iter().enumerate() {
            match item {
                HelpItem::Entry(syntax, description) => writeln!(
                    formatter,
                    "    {syntax:<syntax_width$}  {description}",
                    syntax_width = syntax_width
                )?,
                HelpItem::Paragraph(paragraph) => {
                    if index > 0 {
                        writeln!(formatter)?;
                    }
                    write_wrapped(formatter, paragraph, 4, 80)?;
                }
            }
        }
        Ok(())
    }
}

fn write_wrapped(
    formatter: &mut fmt::Formatter<'_>,
    text: &str,
    indentation: usize,
    width: usize,
) -> fmt::Result {
    let prefix = " ".repeat(indentation);
    let line_width = width.saturating_sub(indentation).max(1);
    let mut line = String::new();
    for word in text.split_whitespace() {
        let separator = usize::from(!line.is_empty());
        if !line.is_empty() && line.len() + separator + word.len() > line_width {
            writeln!(formatter, "{prefix}{line}")?;
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        writeln!(formatter, "{prefix}{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(topic: HelpTopic) -> String {
        text(topic).split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn root_help_lists_commands_and_only_public_options() {
        let help = text(HelpTopic::Application);
        for heading in ["USAGE:", "COMMANDS:", "OPTIONS:", "CONFIGURATION:"] {
            assert!(help.contains(heading), "{heading}");
        }
        for hidden in [
            "--daemon",
            "--fd-sock",
            "--internal-x11-reconcile",
            "--cli-mode",
            "--quiet",
        ] {
            assert!(!help.contains(hidden), "{hidden}");
        }
        assert!(help.contains("-s, --systemd-tools"));
        let help = normalized(HelpTopic::Application);
        assert!(help.contains("lasper shell --help"));
        assert!(help.contains("lasper launch --help"));
    }

    #[test]
    fn shell_help_explains_default_shell_option_boundary_and_notices() {
        let help = normalized(HelpTopic::Shell);
        assert!(help.contains("--quiet Suppress Wayland fallback and detach notices"));
        assert!(help.contains("Press Ctrl+] three times within one second to detach."));
        assert!(help.contains("Automatic Wayland selection falls back"));
        assert!(help.contains("A trailing -- after USER@MACHINE also opens the default shell."));
        assert!(help.contains("All arguments after the executable are passed unchanged"));
        assert!(help.contains("--elevate=METHOD"));
    }

    #[test]
    fn launch_help_describes_desktop_execution_without_shell_only_switches() {
        let help = text(HelpTopic::Launch);
        let options = help
            .split("OPTIONS:\n")
            .nth(1)
            .unwrap()
            .split("\nARGUMENTS:")
            .next()
            .unwrap();
        assert!(!options.contains("--elevate"));
        assert!(!options.contains("--quiet"));
        assert!(options.contains("-s, --systemd-tools"));
        let help = normalized(HelpTopic::Launch);
        assert!(help.contains("Terminal=false desktop entries"));
        assert!(help.contains("systemd/polkit authority"));
    }

    #[test]
    fn both_session_help_pages_describe_the_same_display_options() {
        for topic in [HelpTopic::Shell, HelpTopic::Launch] {
            let help = normalized(topic);
            assert!(help.contains("--with-x11[=:N] Explicitly prepare Host X11 access"));
            assert!(help.contains("--wayland[=DISPLAY]"));
            assert!(help.contains("--no-wayland"));
            assert!(help.contains("Host X11 is never enabled implicitly"));
        }
    }
}

//! Present local account observations without treating them as proof of login access.

use crate::domain::machine::accounts::LocalAccountObservation;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShellGroup {
    LoginCandidate,
    OtherProgram,
    NonLogin,
}

impl ShellGroup {
    fn from_shell(shell: &str) -> Self {
        // passwd(5) specifies /bin/sh when the interpreter field is empty.
        if shell.is_empty() {
            return Self::LoginCandidate;
        }
        let path = Path::new(shell);
        if !path.is_absolute() {
            return Self::OtherProgram;
        }
        // Recognize names rather than fixed paths, including /usr merges and Nix store paths.
        // Other programs remain visible without guessing whether they offer a shell.
        match path.file_name().and_then(|name| name.to_str()) {
            Some("nologin" | "false" | "true") => Self::NonLogin,
            Some(
                "sh" | "bash" | "dash" | "ash" | "zsh" | "fish" | "ksh" | "ksh93" | "mksh" | "csh"
                | "tcsh" | "yash" | "nu" | "elvish" | "xonsh" | "osh",
            ) => Self::LoginCandidate,
            _ => Self::OtherProgram,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::LoginCandidate => "Login shell candidates",
            Self::OtherProgram => "Other shell programs",
            Self::NonLogin => "Non-login shells",
        }
    }
}

pub(super) fn content(observation: &LocalAccountObservation) -> String {
    match observation {
        LocalAccountObservation::Read {
            accounts,
            rejected_lines,
        } => {
            let mut lines = Vec::new();
            if !rejected_lines.is_empty() {
                lines.push(format!(
                    "Malformed passwd records skipped at line(s): {}",
                    rejected_lines
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if accounts.is_empty() {
                lines.push("No valid local account entries were found.".into());
                return lines.join("\n");
            }
            lines.push("Grouped by shell; login authentication has not been checked.".into());
            for group in [
                ShellGroup::LoginCandidate,
                ShellGroup::OtherProgram,
                ShellGroup::NonLogin,
            ] {
                let mut entries = accounts
                    .iter()
                    .filter(|account| ShellGroup::from_shell(&account.shell) == group)
                    .peekable();
                if entries.peek().is_none() {
                    continue;
                }
                lines.push(String::new());
                lines.push(format!("{}:", group.title()));
                for account in entries {
                    lines.push(format!(
                        "{}  UID {}  GID {}",
                        account.username, account.uid, account.gid
                    ));
                    lines.push(format!("  Home: {}", account.home));
                    lines.push(format!(
                        "  Shell: {}",
                        if account.shell.is_empty() {
                            "/bin/sh (empty shell field)"
                        } else {
                            &account.shell
                        }
                    ));
                }
            }
            lines.join("\n")
        }
        LocalAccountObservation::Missing => "/etc/passwd is missing.".into(),
        LocalAccountObservation::Invalid(reason) => format!("Invalid /etc/passwd: {reason}"),
        LocalAccountObservation::Unavailable(reason) => {
            format!("Cannot read /etc/passwd: {reason}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::machine::accounts::LocalAccount;

    #[test]
    fn shell_groups_keep_custom_programs_and_do_not_assume_a_human_uid_range() {
        let observation = LocalAccountObservation::Read {
            accounts: [
                ("service", 1000, "/usr/sbin/nologin"),
                ("custom", 1001, "/opt/company/login"),
                ("alice", 501, "/nix/store/example-zsh/bin/zsh"),
                ("root", 0, "/bin/bash"),
                ("default", 500, ""),
                ("sync", 4, "/bin/sync"),
            ]
            .into_iter()
            .map(|(username, uid, shell)| LocalAccount {
                username: username.into(),
                uid,
                gid: uid,
                home: format!("/home/{username}"),
                shell: shell.into(),
            })
            .collect(),
            rejected_lines: vec![],
        };
        let text = content(&observation);
        let (candidates, other) = text.split_once("Other shell programs:").unwrap();
        let (other, non_login) = other.split_once("Non-login shells:").unwrap();
        assert!(candidates.contains("alice  UID 501"));
        assert!(candidates.contains("root  UID 0"));
        assert!(candidates.contains("/bin/sh (empty shell field)"));
        assert!(other.contains("/opt/company/login"));
        assert!(other.contains("/bin/sync"));
        assert!(non_login.contains("service  UID 1000"));
        assert!(non_login.contains("/usr/sbin/nologin"));
        assert!(text.contains("login authentication has not been checked"));
    }

    #[test]
    fn non_login_commands_are_recognized_across_guest_layouts() {
        for shell in [
            "/sbin/nologin",
            "/bin/false",
            "/usr/bin/true",
            "/nix/store/example/bin/nologin",
        ] {
            assert!(ShellGroup::from_shell(shell) == ShellGroup::NonLogin);
        }
        assert!(ShellGroup::from_shell("bash") == ShellGroup::OtherProgram);
        assert!(ShellGroup::from_shell("/bin/company-shell") == ShellGroup::OtherProgram);
    }
}

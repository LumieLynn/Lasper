use std::path::{Path, PathBuf};

fn rust_sources(relative_root: &str) -> Vec<(PathBuf, String)> {
    fn collect(path: &Path, sources: &mut Vec<(PathBuf, String)>) {
        let mut entries = std::fs::read_dir(path)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
            .map(|entry| entry.expect("source directory entry is readable"))
            .collect::<Vec<_>>();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, sources);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
                sources.push((path, source));
            }
        }
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_root);
    let mut sources = Vec::new();
    collect(&root, &mut sources);
    sources
}

fn assert_absent(layer: &str, sources: &[(PathBuf, String)], forbidden: &[&str]) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();
    for (path, source) in sources {
        for needle in forbidden {
            if source.contains(needle) {
                violations.push(format!(
                    "{} contains {needle:?}",
                    path.strip_prefix(repository).unwrap_or(path).display()
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "{layer} boundary violations:\n{}",
        violations.join("\n")
    );
}

/// Remove test-only items before applying a production boundary rule.
///
/// This is deliberately a small source scanner rather than a Rust parser: the
/// boundary test only needs to skip complete `#[cfg(test)]` items, while the
/// compiler remains the authoritative dependency checker. Unlike a prefix
/// split, it keeps scanning production items which follow a test-only item and
/// handles local cfg(test) helpers without hiding the rest of the file.
fn production_source(source: &str) -> String {
    const CFG_TEST: &str = "#[cfg(test)]";

    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    while let Some(relative) = source[cursor..]
        .match_indices(CFG_TEST)
        .next()
        .map(|(i, _)| i)
    {
        let start = cursor + relative;
        let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
        if !source[line_start..start].trim().is_empty() {
            cursor = start + CFG_TEST.len();
            continue;
        }

        output.push_str(&source[cursor..start]);
        cursor = cfg_test_item_end(source, start + CFG_TEST.len());
    }
    output.push_str(&source[cursor..]);
    output
}

fn cfg_test_item_end(source: &str, mut cursor: usize) -> usize {
    let bytes = source.as_bytes();
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut block_comment_depth = 0usize;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    let mut saw_body = false;

    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if block_comment_depth > 0 {
            if bytes.get(cursor..cursor + 2) == Some(b"/*") {
                block_comment_depth += 1;
                cursor += 2;
            } else if bytes.get(cursor..cursor + 2) == Some(b"*/") {
                block_comment_depth -= 1;
                cursor += 2;
            } else {
                cursor += 1;
            }
            continue;
        }
        if in_string || in_char {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if (in_string && byte == b'"') || (in_char && byte == b'\'') {
                in_string = false;
                in_char = false;
            }
            cursor += 1;
            continue;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"//") {
            cursor += 2;
            while cursor < bytes.len() && bytes[cursor] != b'\n' {
                cursor += 1;
            }
            continue;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"/*") {
            block_comment_depth = 1;
            cursor += 2;
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'\'' => in_char = true,
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => {
                saw_body = true;
                brace_depth += 1;
            }
            b'}' if brace_depth > 0 => {
                brace_depth -= 1;
                if saw_body && brace_depth == 0 {
                    return cursor + 1;
                }
            }
            b';' if !saw_body && paren_depth == 0 && bracket_depth == 0 => {
                return cursor + 1;
            }
            _ => {}
        }
        cursor += 1;
    }
    source.len()
}

#[test]
fn domain_and_application_do_not_import_outer_layers_or_transport_types() {
    let mut sources = rust_sources("src/domain");
    sources.extend(rust_sources("src/application"));

    assert_absent(
        "domain/application",
        &sources,
        &[
            "crate::tui::",
            "crate::adapters::",
            "crate::daemon::",
            "crate::composition::",
            "ElevatedDaemon",
            "serde_json::Value",
            "std::os::fd::RawFd",
            "std::os::fd::OwnedFd",
            "std::os::unix::io::RawFd",
            "ratatui::",
            "crossterm::",
        ],
    );
}

#[test]
fn tui_production_does_not_hold_host_transport_or_composition_mode() {
    let sources = rust_sources("src/tui")
        .into_iter()
        .map(|(path, source)| {
            let production = production_source(&source);
            (path, production)
        })
        .collect::<Vec<_>>();

    assert_absent(
        "TUI production",
        &sources,
        &[
            "crate::adapters::",
            "crate::daemon::",
            "ElevatedDaemon",
            "ExecutionContext",
            "CompositionMode",
            "serde_json::Value",
            "std::os::fd::RawFd",
            "std::os::fd::OwnedFd",
        ],
    );
}

#[test]
fn production_source_keeps_code_after_test_only_items() {
    let source = r#"
#[cfg(test)]
mod tests {
    use crate::adapters::test_only;
}

fn production_after_tests() {
    crate::adapters::required();
}
"#;

    let production = production_source(source);
    assert!(!production.contains("test_only"));
    assert!(production.contains("production_after_tests"));
    assert!(production.contains("adapters::required"));
}

#[test]
fn production_source_removes_local_cfg_test_items_without_truncating_file() {
    let source = r#"
fn production_before() {}

#[cfg(test)]
fn test_helper() {
    crate::daemon::test_only();
}

fn production_after() {}
"#;

    let production = production_source(source);
    assert!(production.contains("production_before"));
    assert!(!production.contains("test_helper"));
    assert!(!production.contains("daemon::test_only"));
    assert!(production.contains("production_after"));
}

#[test]
fn adapters_do_not_reselect_process_authority_or_optional_daemon_routes() {
    let sources = rust_sources("src/adapters");

    assert_absent(
        "adapter composition",
        &sources,
        &[
            "PermissionLevel",
            "ExecutionContext",
            "Option<Arc<ElevatedDaemon>>",
            "Option<std::sync::Arc<ElevatedDaemon>>",
            "Option<Arc<crate::adapters::elevated::ElevatedDaemon>>",
        ],
    );
}

#[test]
fn adapters_do_not_depend_on_daemon_server_implementation() {
    let sources = rust_sources("src/adapters");

    assert_absent(
        "adapter/server ownership",
        &sources,
        &[
            "crate::daemon::server",
            "super::server::transport",
            "server::transport",
        ],
    );
}

#[test]
fn ipc_transport_does_not_depend_on_runtime_layers() {
    let sources = rust_sources("src/ipc/transport");

    assert_absent(
        "IPC transport",
        &sources,
        &[
            "crate::adapters::",
            "crate::application::",
            "crate::daemon::",
            "crate::tui::",
            "crate::composition::",
            "ratatui::",
            "crossterm::",
        ],
    );
}

#[test]
fn ipc_protocol_does_not_depend_on_host_implementations() {
    let sources = rust_sources("src/ipc/protocol");

    assert_absent(
        "IPC protocol",
        &sources,
        &[
            "crate::adapters::",
            "crate::daemon::",
            "crate::tui::",
            "crate::composition::",
            "ratatui::",
            "crossterm::",
            "std::os::fd::RawFd",
            "std::os::fd::OwnedFd",
        ],
    );
}

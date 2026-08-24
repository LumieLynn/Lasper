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
            let production = source
                .split("#[cfg(test)]")
                .next()
                .expect("every source has a production prefix")
                .to_string();
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

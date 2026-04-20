use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{new_command, CommandLogged};
use std::path::Path;

pub(crate) async fn get_ldconfig_cache() -> Option<String> {
    let out = new_command("ldconfig")
        .arg("-p")
        .logged_output("ldconfig")
        .await
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        None
    }
}

/// For a given .so path (which might be a versioned file), find it and all its aliases.
/// We first try `ldconfig -p`, then fallback to directory scanning.
pub(crate) async fn resolve_so_aliases(
    path: &str,
    ldconfig_cache: Option<&str>,
) -> Result<Vec<String>> {
    let p = Path::new(path);
    let dir = p
        .parent()
        .ok_or_else(|| NspawnError::Runtime("Invalid lib path".into()))?;
    let file_name = p
        .file_name()
        .ok_or_else(|| NspawnError::Runtime("Invalid lib path".into()))?
        .to_string_lossy();

    // Extract base name, e.g. "libcuda.so" from "libcuda.so.595.58.03"
    let base_name = if let Some(pos) = file_name.find(".so") {
        &file_name[..pos + 3]
    } else {
        &file_name
    };

    let mut aliases = Vec::new();

    // 1. Try ldconfig cache
    if let Some(cache) = ldconfig_cache {
        for line in cache.lines() {
            if line.contains(base_name) {
                if let Some(right) = line.split("=>").nth(1) {
                    let extracted = right.trim();
                    if !extracted.is_empty() {
                        aliases.push(extracted.to_string());
                    }
                }
            }
        }
    }

    // 2. Fallback to directory scan if ldconfig found nothing
    if aliases.is_empty() {
        let mut entries = tokio::fs::read_dir(dir)
            .await
            .map_err(|e| NspawnError::Io(dir.to_path_buf(), e))?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| NspawnError::Io(dir.to_path_buf(), e))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(base_name) {
                aliases.push(entry.path().to_string_lossy().into_owned());
            }
        }
    }

    Ok(aliases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_resolve_ldconfig_extracts_paths() {
        let cache = "libcuda.so.1 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcuda.so.1\n\
                     libcuda.so (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcuda.so";

        let result = resolve_so_aliases("/some/path/libcuda.so.550.1", Some(cache))
            .await
            .unwrap();
        assert!(result.contains(&"/usr/lib/x86_64-linux-gnu/libcuda.so.1".to_string()));
        assert!(result.contains(&"/usr/lib/x86_64-linux-gnu/libcuda.so".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_ldconfig_no_match() {
        let cache = "libother.so => /usr/lib/libother.so";
        // Use a parent directory that exists but a filename that doesn't match anything in cache.
        // We use /usr/bin as a likely existing directory.
        let result = resolve_so_aliases("/usr/bin/libnonexistent.so", Some(cache))
            .await
            .unwrap();
        assert!(result.is_empty());
    }
}

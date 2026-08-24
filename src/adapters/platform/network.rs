/// Autodetect bridges available on the host.
pub async fn detect_bridges() -> Vec<String> {
    let mut bridges = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir("/sys/class/net").await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if tokio::fs::metadata(path.join("bridge"))
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                if let Some(name) = path.file_name() {
                    bridges.push(name.to_string_lossy().into_owned());
                }
            }
        }
    }
    bridges.sort();
    bridges
}

/// Autodetect physical (non-virtual) network interfaces on the host.
pub async fn detect_physical_interfaces() -> Vec<String> {
    let mut interfaces = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir("/sys/class/net").await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "lo" {
                continue;
            }

            let path = entry.path();

            if let Ok(real_path) = tokio::fs::canonicalize(&path).await {
                if !real_path.to_string_lossy().contains("/devices/virtual/") {
                    interfaces.push(name);
                }
            }
        }
    }
    interfaces.sort();
    interfaces
}

/// Identify the type of a network interface for diagnostic purposes.
pub async fn identify_interface(name: &str) -> String {
    if name == "lo" {
        return "loopback interface".into();
    }
    let path = std::path::PathBuf::from(format!("/sys/class/net/{}", name));
    if tokio::fs::metadata(path.join("bridge")).await.is_ok() {
        return "bridge".into();
    }
    if let Ok(real_path) = tokio::fs::canonicalize(&path).await {
        if !real_path.to_string_lossy().contains("/devices/virtual/") {
            return "physical interface".into();
        }
    }
    "virtual interface".into()
}

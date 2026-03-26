use std::path::PathBuf;

/// Raw content of a `.nspawn` config file from `/etc/systemd/nspawn/`.
pub struct NspawnConfig {
    #[allow(dead_code)]
    pub path: PathBuf,
    pub content: String,
}

impl NspawnConfig {
    /// Load the `.nspawn` config for a container by name.
    /// Returns `None` if the file doesn't exist or cannot be read (e.g. no root).
    pub fn load(name: &str) -> Option<NspawnConfig> {
        let path = PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name));
        match std::fs::read_to_string(&path) {
            Ok(content) => Some(NspawnConfig { path, content }),
            Err(e) => {
                log::debug!("Could not read .nspawn config for {}: {}", name, e);
                Option::None
            }
        }
    }
}

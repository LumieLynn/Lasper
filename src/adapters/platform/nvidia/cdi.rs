use serde::Deserialize;

pub(crate) const NVIDIA_CDI_KIND: &str = "nvidia.com/gpu";
pub(crate) const MAX_CDI_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

/// Metadata and payload of one CDI document.
///
/// CDI files are an external registry, so the metadata is kept separate from
/// the projection-oriented `CdiSpec` used by the state builder.  This lets the
/// generated JSON and an administrator-provided YAML document share exactly
/// the same downstream representation.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CdiDocument {
    pub(crate) cdi_version: Option<String>,
    pub(crate) kind: Option<String>,
    pub(crate) container_edits: Option<CdiEdits>,
    pub(crate) devices: Option<Vec<CdiDevice>>,
}

impl CdiDocument {
    pub(crate) fn into_spec(self) -> CdiSpec {
        CdiSpec {
            container_edits: self.container_edits,
            devices: self.devices,
        }
    }
}

// CDI Parsing Structs for industry-standard discovery (ISO/IEC 20248 compliant)
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CdiSpec {
    pub(crate) container_edits: Option<CdiEdits>,
    pub(crate) devices: Option<Vec<CdiDevice>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CdiDevice {
    pub(crate) name: String,
    pub(crate) container_edits: Option<CdiEdits>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CdiEdits {
    pub(crate) device_nodes: Option<Vec<CdiDeviceNode>>,
    pub(crate) mounts: Option<Vec<CdiMount>>,
    pub(crate) hooks: Option<Vec<CdiHook>>,
    pub(crate) env: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct CdiHook {
    pub(crate) hook_name: String,
    pub(crate) path: String,
    pub(crate) args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct CdiDeviceNode {
    pub(crate) path: String,
    pub(crate) host_path: Option<String>,
    pub(crate) major: Option<u32>,
    pub(crate) minor: Option<u32>,
    pub(crate) permissions: Option<String>,
    pub(crate) gid: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct CdiMount {
    pub(crate) host_path: String,
    pub(crate) container_path: String,
    pub(crate) options: Option<Vec<String>>,
}

impl CdiMount {
    pub(crate) fn readonly(&self) -> bool {
        let mut readonly = false;
        for option in self
            .options
            .iter()
            .flatten()
            .flat_map(|option| option.split(','))
            .map(str::trim)
        {
            match option {
                "ro" => readonly = true,
                "rw" => readonly = false,
                _ => {}
            }
        }
        readonly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_real_world_cdi_json() {
        let json = r#"{"cdiVersion":"0.5.0","kind":"nvidia.com/gpu","devices":[{"name":"0","containerEdits":{"deviceNodes":[{"path":"/dev/nvidia0"}]}}],"containerEdits":{"env":["NVIDIA_VISIBLE_DEVICES=void"],"deviceNodes":[{"path":"/dev/nvidiactl"}]}}"#;
        let spec: CdiSpec = serde_json::from_str(json).unwrap();

        let mut nodes = Vec::new();
        if let Some(edits) = spec.container_edits {
            for node in edits.device_nodes.unwrap() {
                nodes.push(node.path);
            }
        }
        for dev in spec.devices.unwrap() {
            for node in dev.container_edits.unwrap().device_nodes.unwrap() {
                nodes.push(node.path);
            }
        }

        assert!(nodes.contains(&"/dev/nvidiactl".to_string()));
        assert!(nodes.contains(&"/dev/nvidia0".to_string()));
    }

    #[test]
    fn mount_options_apply_oci_readonly_order() {
        let mount = |options: Option<Vec<&str>>| CdiMount {
            host_path: "/host".into(),
            container_path: "/container".into(),
            options: options.map(|options| options.into_iter().map(str::to_string).collect()),
        };

        assert!(!mount(None).readonly());
        assert!(mount(Some(vec!["rbind", "ro"])).readonly());
        assert!(!mount(Some(vec!["ro", "rw"])).readonly());
        assert!(mount(Some(vec!["rw,ro"])).readonly());
    }
}

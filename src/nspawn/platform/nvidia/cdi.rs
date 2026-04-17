use serde::Deserialize;

// CDI Parsing Structs for industry-standard discovery (ISO/IEC 20248 compliant)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CdiSpec {
    pub(crate) container_edits: Option<CdiEdits>,
    pub(crate) devices: Option<Vec<CdiDevice>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CdiDevice {
    #[allow(dead_code)]
    pub(crate) name: String,
    pub(crate) container_edits: Option<CdiEdits>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CdiEdits {
    pub(crate) device_nodes: Option<Vec<CdiDeviceNode>>,
    pub(crate) mounts: Option<Vec<CdiMount>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CdiDeviceNode {
    pub(crate) path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CdiMount {
    pub(crate) host_path: String,
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
}

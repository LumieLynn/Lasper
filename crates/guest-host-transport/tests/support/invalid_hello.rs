use lasper_guest_protocol::{BrokerId, ControlLimits, FeatureSet, HelloResponse};

/// Constructs wire-valid responses that violate negotiation, for rejection tests.
pub fn response(features: FeatureSet, broker: BrokerId, limits: ControlLimits) -> HelloResponse {
    let payload = format!(
        r#"{{"offer":{{"protocols":{{"major":1,"min_minor":0,"max_minor":0}},"supported_features":{},"limits":{{"max_control_frame_bytes":{},"max_descriptors":{}}}}},"selected":{{"version":{{"major":1,"minor":0}},"features":{},"limits":{{"max_control_frame_bytes":{},"max_descriptors":{}}}}},"broker_id":{:?}}}"#,
        features.bits(),
        limits.max_control_frame_bytes(),
        limits.max_descriptors(),
        features.bits(),
        limits.max_control_frame_bytes(),
        limits.max_descriptors(),
        broker.as_bytes(),
    );
    HelloResponse::decode_payload(payload.as_bytes()).unwrap()
}

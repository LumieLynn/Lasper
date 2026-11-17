extern crate std;
use super::*;
use crate::{BrokerId, ControlLimits, FeatureSet, HelloRequest, HelloResponse};

#[test]
fn borrowed_json_strings_reject_raw_controls_and_invalid_escapes() {
    #[derive(serde::Deserialize)]
    struct Borrowed<'a> {
        #[serde(borrow)]
        value: EscapedStr<'a>,
    }
    for byte in 0..0x20 {
        let mut bytes = std::vec::Vec::from(&br#"{"value":"a"#[..]);
        bytes.push(byte);
        bytes.extend_from_slice(br#"b"}"#);
        assert!(validate_json_strings(&bytes).is_err());
    }
    for bytes in [
        &br#"{"value":"\q"}"#[..],
        &br#"{"value":"\uD800"}"#[..],
        &br#"{"value":"\uDC00"}"#[..],
        &br#"{"value":"\uXYZW"}"#[..],
        &b"{\"value\":\"\xff\"}"[..],
    ] {
        assert!(validate_json_strings(bytes).is_err(), "{bytes:?}");
    }
    let valid = br#"{"value":"\n\t\r\b\f\u0001\"\\\/\uD83D\uDE00"}"#;
    validate_json_strings(valid).unwrap();
    let (decoded, _) = serde_json_core::from_slice::<Borrowed<'_>>(valid).unwrap();
    assert!(fragments(decoded.value).all(|fragment| fragment.is_ok()));
}

#[test]
fn codec_enforces_payload_and_output_bounds() {
    let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
    let (response, _) = HelloResponse::accept(
        request,
        crate::HelloOffer::local(ControlLimits::CURRENT),
        BrokerId::new([0; 16]),
    )
    .unwrap();
    let mut short = [0; 8];
    assert_eq!(
        encode_json_payload(&response, &mut short),
        Err(PayloadCodecError::OutputTooSmall)
    );

    let oversized_value = std::string::String::from_iter(std::iter::repeat_n(
        'x',
        crate::MAX_CONTROL_PAYLOAD_BYTES as usize,
    ));
    let mut maximum_output = std::vec![0; crate::MAX_CONTROL_PAYLOAD_BYTES as usize + 1024];
    assert_eq!(
        encode_json_payload(oversized_value.as_str(), &mut maximum_output),
        Err(PayloadCodecError::PayloadTooLarge)
    );

    let oversized = [b' '; crate::MAX_CONTROL_PAYLOAD_BYTES as usize + 1];
    assert_eq!(
        HelloRequest::decode_payload(&oversized),
        Err(PayloadCodecError::PayloadTooLarge)
    );
}

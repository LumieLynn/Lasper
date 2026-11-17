use super::*;

fn stat(start: &str) -> Vec<u8> {
    let fields = std::iter::repeat_n("0", 18).collect::<Vec<_>>().join(" ");
    format!("42 (a process ) name) S {fields} {start} 0\n").into_bytes()
}

#[test]
fn start_time_ignores_parentheses_spaces_and_non_utf8_in_comm() {
    let mut bytes = stat("123");
    assert_eq!(parse_start_time(&bytes).unwrap().get(), 123);
    bytes[5] = 0xff;
    assert_eq!(parse_start_time(&bytes).unwrap().get(), 123);
}

#[test]
fn missing_zero_or_invalid_start_time_is_not_instance_evidence() {
    for bytes in [
        b"".to_vec(),
        b"42 (process) S".to_vec(),
        stat("0"),
        stat("-1"),
        stat("18446744073709551616"),
        stat("invalid"),
    ] {
        assert_eq!(
            parse_start_time(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

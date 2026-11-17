use lasper_guest_protocol::*;

#[test]
fn encoder_orders_entries_independently_of_insertion_order() {
    let ordered = [
        ("A", "first"),
        ("AA", "prefix"),
        ("B", "next"),
        ("a", "case"),
    ];
    let mut expected = Vec::new();
    for entries in [
        ordered,
        [ordered[3], ordered[2], ordered[1], ordered[0]],
        [ordered[2], ordered[0], ordered[3], ordered[1]],
    ] {
        let request = PrepareRequest::new(
            "root",
            ExecutionKind::Piped,
            GuestProcess::LoginShell,
            ProcessEnvironment::new(entries).unwrap(),
            None,
        )
        .unwrap();
        let mut bytes = [0; 4096];
        let length = protocol().encode_prepare(&request, &mut bytes).unwrap();
        if expected.is_empty() {
            expected.extend_from_slice(&bytes[..length]);
        }
        assert_eq!(&bytes[..length], expected);
    }
    assert!(ProcessEnvironment::new([("A", "same"), ("A", "same")]).is_err());
    assert!(ProcessEnvironment::new([("A", "first"), ("A", "second")]).is_err());
    assert!(ProcessEnvironment::new([("B", "x"), ("A", "y"), ("B", "z")]).is_err());
}

#[test]
fn surrogate_pairs_count_as_decoded_utf8_at_the_environment_limit() {
    let value = "\\uD83D\\uDE00".repeat(4095) + "x";
    let bytes = payload(&format!(r#"[{{"name":"V","value":"{value}"}}]"#));
    let mut scratch = [0; MAX_PREPARE_UNESCAPE_BYTES];
    let decoded = protocol()
        .decode_prepare(bytes.as_bytes(), &mut scratch)
        .unwrap();
    let (name, actual) = decoded.environment().entries()[0]
        .decode_into(&mut scratch)
        .unwrap();
    assert_eq!(name, "V");
    assert_eq!(actual, "😀".repeat(4095) + "x");
    assert_eq!(name.len() + actual.len() + 2, MAX_PROCESS_ENVIRONMENT_BYTES);
    let mut output = vec![0; 64 * 1024];
    let length = protocol().encode_prepare(&decoded, &mut output).unwrap();
    let round_trip = protocol()
        .decode_prepare(&output[..length], &mut scratch)
        .unwrap();
    assert_eq!(
        round_trip.environment().entries()[0]
            .decode_into(&mut scratch)
            .unwrap()
            .1,
        "😀".repeat(4095) + "x"
    );
    let oversized = payload(&format!(r#"[{{"name":"V","value":"{value}x"}}]"#));
    assert!(protocol()
        .decode_prepare(oversized.as_bytes(), &mut scratch)
        .is_err());
}

fn protocol() -> NegotiatedProtocol {
    negotiate_hello(
        HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap(),
        HelloOffer::local(ControlLimits::CURRENT),
    )
    .unwrap()
}
fn payload(environment: &str) -> String {
    format!(
        r#"{{"user":"root","execution":"piped","process":"login_shell","environment":{environment}}}"#
    )
}
#[test]
fn maximum_environment_value_with_escapes_decodes_without_an_argv_sized_buffer() {
    let value = "a\n\\\"".repeat((MAX_PROCESS_ENVIRONMENT_BYTES - 4) / 4);
    let env = ProcessEnvironment::new([("X", value.as_str())]).unwrap();
    let request = PrepareRequest::new(
        "root",
        ExecutionKind::Piped,
        GuestProcess::LoginShell,
        env,
        None,
    )
    .unwrap();
    let mut bytes = vec![0; 64 * 1024];
    let n = protocol().encode_prepare(&request, &mut bytes).unwrap();
    let mut scratch = vec![0; MAX_PREPARE_UNESCAPE_BYTES];
    let decoded = protocol()
        .decode_prepare(&bytes[..n], &mut scratch)
        .unwrap();
    let (name, actual) = decoded.environment().entries()[0]
        .decode_into(&mut scratch)
        .unwrap();
    assert_eq!(name, "X");
    assert_eq!(actual, value);
}
#[test]
fn decoded_names_are_ordered_unique_and_values_preserve_empty_and_newlines() {
    for env in [
        r#"[{"name":"B","value":""},{"name":"A","value":""}]"#,
        r#"[{"name":"A","value":""},{"name":"\u0041","value":""}]"#,
        r#"[{"name":"9BAD","value":""}]"#,
        r#"[{"name":"A","value":"\u0000"}]"#,
    ] {
        assert!(
            protocol()
                .decode_prepare(
                    payload(env).as_bytes(),
                    &mut [0; MAX_PREPARE_UNESCAPE_BYTES]
                )
                .is_err(),
            "accepted {env}"
        );
    }
    let bytes = payload(r#"[{"name":"\u0041","value":"line\nnext"},{"name":"TERM","value":""}]"#);
    let mut scratch = [0; MAX_PREPARE_UNESCAPE_BYTES];
    let decoded = protocol()
        .decode_prepare(bytes.as_bytes(), &mut scratch)
        .unwrap();
    assert_eq!(
        decoded.environment().entries()[0]
            .decode_into(&mut scratch)
            .unwrap(),
        ("A", "line\nnext")
    );
    assert_eq!(
        decoded.environment().entries()[1]
            .decode_into(&mut scratch)
            .unwrap(),
        ("TERM", "")
    );
    assert!(protocol().decode_prepare(br#"{"user":"root","execution":"piped","process":"login_shell","environment":[],"initial_size":null}"#, &mut scratch).is_err());
}
#[test]
fn environment_budget_includes_name_equals_and_nul() {
    let value = "x".repeat(MAX_PROCESS_ENVIRONMENT_BYTES - 3);
    assert!(ProcessEnvironment::new([("X", value.as_str())]).is_ok());
    let value = value + "x";
    assert!(ProcessEnvironment::new([("X", value.as_str())]).is_err());
}

#[test]
fn maximum_argument_and_environment_metadata_fit_the_prepare_workspace() {
    let mut arguments = vec!["/bin/true".to_owned()];
    arguments.extend((0..MAX_COMMAND_ARGUMENTS).map(|_| "x".to_owned()));
    let mut remaining = MAX_COMMAND_TOTAL_BYTES
        - arguments.iter().map(String::len).sum::<usize>()
        - arguments.len()
        + 1;
    for argument in &mut arguments[1..] {
        let extra = remaining.min(MAX_COMMAND_ARGUMENT_BYTES - argument.len());
        argument.extend(std::iter::repeat_n('x', extra));
        remaining -= extra;
    }
    assert_eq!(remaining, 0);
    let names: Vec<_> = (0..MAX_PROCESS_ENVIRONMENT_ENTRIES)
        .map(|index| format!("V{index:02}"))
        .collect();
    let mut values = vec![String::new(); names.len()];
    let overhead = names.iter().map(|name| name.len() + 2).sum::<usize>();
    values[0] = "\n".repeat(MAX_PROCESS_ENVIRONMENT_BYTES - overhead);
    let environment = ProcessEnvironment::new(
        names
            .iter()
            .zip(&values)
            .map(|(name, value)| (name.as_str(), value.as_str())),
    )
    .unwrap();
    let argv: Vec<_> = arguments.iter().map(String::as_str).collect();
    let request = PrepareRequest::new(
        "root",
        ExecutionKind::Piped,
        GuestProcess::Command(CommandArgv::new(&argv).unwrap()),
        environment,
        None,
    )
    .unwrap();
    let mut bytes = vec![0; 256 * 1024];
    let length = protocol().encode_prepare(&request, &mut bytes).unwrap();
    let mut scratch = vec![0; MAX_PREPARE_UNESCAPE_BYTES];
    let decoded = protocol()
        .decode_prepare(&bytes[..length], &mut scratch)
        .unwrap();
    assert_eq!(
        decoded.environment().entries().len(),
        MAX_PROCESS_ENVIRONMENT_ENTRIES
    );
    let GuestProcess::Command(argv) = decoded.process() else {
        panic!("expected argv")
    };
    assert_eq!(argv.argument_count(), MAX_COMMAND_ARGUMENTS);
    assert!(core::mem::size_of_val(&decoded) <= 48 * 1024);
    assert_eq!(
        decoded.environment().entries()[0]
            .decode_into(&mut scratch)
            .unwrap()
            .1,
        values[0]
    );
}

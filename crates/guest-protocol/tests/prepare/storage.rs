extern crate std;
use super::*;
use std::{
    borrow::ToOwned, boxed::Box, format, string::String as OwnedString, vec, vec::Vec as OwnedVec,
};

#[repr(C, align(16))]
struct Guarded<'a> {
    before: [u8; 64],
    scratch: [u8; 16 * 1024],
    metadata: PrepareDecodeStorage<'a>,
    after: [u8; 64],
}

#[test]
fn maximum_prepare_stays_in_its_slots_and_borrows_the_payload() {
    let mut arguments = vec!["/bin/true".to_owned()];
    arguments.extend((0..MAX_COMMAND_ARGUMENTS).map(|_| "x".to_owned()));
    let mut remaining = MAX_COMMAND_TOTAL_BYTES
        - arguments.iter().map(OwnedString::len).sum::<usize>()
        - arguments.len()
        + 1;
    for argument in &mut arguments[1..] {
        let extra = remaining.min(MAX_COMMAND_ARGUMENT_BYTES - argument.len());
        argument.extend(std::iter::repeat_n('\n', extra));
        remaining -= extra;
    }
    assert_eq!(remaining, 0);
    let names: OwnedVec<_> = (0..MAX_PROCESS_ENVIRONMENT_ENTRIES)
        .map(|n| format!("V{n:02}"))
        .collect();
    let mut values = vec![OwnedString::new(); names.len()];
    let overhead = names.iter().map(|name| name.len() + 2).sum::<usize>();
    values[0] = "\n".repeat(MAX_PROCESS_ENVIRONMENT_BYTES - overhead);
    let argv: OwnedVec<_> = arguments.iter().map(OwnedString::as_str).collect();
    let request = PrepareRequest::new(
        "u".repeat(MAX_GUEST_USER_NAME_BYTES).as_str(),
        ExecutionKind::Terminal,
        GuestProcess::Command(CommandArgv::new(&argv).unwrap()),
        ProcessEnvironment::new(
            names
                .iter()
                .zip(&values)
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .unwrap(),
        Some(TerminalSize::new(120, 40).unwrap()),
    )
    .unwrap();
    let profile = &crate::protocol_profiles()[0];
    let mut payload = vec![0; 256 * 1024];
    let length = profile.encode_prepare(&request, &mut payload).unwrap();
    let mut guarded = Box::new(Guarded {
        before: [0xa5; 64],
        scratch: [0; 16 * 1024],
        metadata: PrepareDecodeStorage::new(),
        after: [0xa5; 64],
    });
    guarded.metadata.wire.padding.fill(0xa5);
    guarded.metadata.output.padding.fill(0xa5);
    guarded.metadata.workspace.padding.fill(0xa5);
    guarded.metadata.reserve.fill(0xa5);
    let base = &guarded.metadata as *const _ as usize;
    assert_eq!(base - guarded.scratch.as_ptr() as usize, 16 * 1024);
    let header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        7,
        length as u32,
        0,
    )
    .unwrap();
    let Guarded {
        scratch, metadata, ..
    } = &mut *guarded;
    let decoded = profile
        .decode_prepare_into(
            metadata,
            header,
            &payload[..length],
            0,
            crate::BrokerId::new([1; 16]),
            scratch,
        )
        .unwrap();
    assert_eq!(decoded as *const _ as usize, base + REQUEST_SLOT_BYTES);
    assert_eq!(
        decoded.request().user(),
        "u".repeat(MAX_GUEST_USER_NAME_BYTES)
    );
    assert_eq!(decoded.request().initial_size(), request.initial_size());
    let GuestProcess::Command(argv) = decoded.request().process() else {
        panic!("expected command")
    };
    assert_eq!(argv.argument_count(), MAX_COMMAND_ARGUMENTS);
    let entries = argv.values.as_ptr() as usize;
    assert!((base + REQUEST_SLOT_BYTES..base + 2 * REQUEST_SLOT_BYTES).contains(&entries));
    for value in &argv.values {
        let CommandArgument::Escaped(value) = value else {
            panic!("expected a borrowed view")
        };
        for fragment in value.fragments() {
            if let EscapedStringFragment::NotEscaped(text) = fragment.unwrap() {
                let pointer = text.as_ptr() as usize;
                assert!(pointer >= payload.as_ptr() as usize);
                assert!(pointer + text.len() <= payload.as_ptr() as usize + length);
            }
        }
    }
    let mut output = vec![0; MAX_COMMAND_TOTAL_BYTES + 1];
    let written = argv.write_nul_terminated_into(&mut output).unwrap();
    assert_eq!(written, MAX_COMMAND_TOTAL_BYTES + 1);
    for (actual, expected) in output[..written].split(|byte| *byte == 0).zip(&arguments) {
        assert_eq!(actual, expected.as_bytes());
    }
    assert_eq!(
        decoded.request().environment().entries().len(),
        MAX_PROCESS_ENVIRONMENT_ENTRIES
    );
    for (actual, (name, value)) in decoded
        .request()
        .environment()
        .entries()
        .iter()
        .zip(names.iter().zip(&values))
    {
        let (actual_name, actual_value) = actual.decode_into(scratch).unwrap();
        assert_eq!((actual_name, actual_value), (name.as_str(), value.as_str()));
    }
    for region in [
        &guarded.before[..],
        &guarded.after[..],
        &guarded.metadata.wire.padding,
        &guarded.metadata.output.padding,
        &guarded.metadata.workspace.padding,
        &guarded.metadata.reserve,
    ] {
        assert!(region.iter().all(|&byte| byte == 0xa5));
    }
}

#[test]
fn failed_decode_invalidates_the_result_and_storage_can_be_reused() {
    let profile = &crate::protocol_profiles()[0];
    let broker = crate::BrokerId::new([1; 16]);
    let mut storage = PrepareDecodeStorage::new();
    let mut scratch = [0; MAX_PREPARE_UNESCAPE_BYTES];
    for (index, payload) in [
        &br#"{"user":"root","execution":"piped","process":{"command":["/bin/true"]},"environment":[]}"#[..],
        &br#"{"user":"root","execution":"piped","process":{"command":["/bin/false"]},"environment":[],"extra":1}"#[..],
        &br#"{"user":"root","execution":"piped","process":"login_shell","environment":[]}"#[..],
    ].into_iter().enumerate() {
        let header = ControlFrameHeader::new_with_version(crate::ProtocolVersion { major: 1, minor: 0 }, MessageKind::PrepareRequest, index as u64 + 1, payload.len() as u32, 0).unwrap();
        let result = profile.decode_prepare_into(&mut storage, header, payload, 0, broker, &mut scratch);
        assert_eq!(result.is_ok(), index != 1);
        assert_eq!(storage.request().is_some(), index != 1);
    }
    assert!(matches!(
        storage.request().unwrap().request().process(),
        GuestProcess::LoginShell
    ));
    let payload = br#"{}"#;
    let header = ControlFrameHeader::new_with_version(
        crate::ProtocolVersion { major: 1, minor: 0 },
        MessageKind::PrepareRequest,
        4,
        2,
        0,
    )
    .unwrap();
    assert!(profile
        .decode_prepare_into(&mut storage, header, payload, 0, broker, &mut [])
        .is_err());
    assert!(storage.request().is_none());
}

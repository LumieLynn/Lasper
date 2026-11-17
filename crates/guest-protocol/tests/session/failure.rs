use super::*;

#[test]
fn failure_stage_numbers_are_fixed_in_both_directions() {
    let cases = [
        (GuestFailureStage::Descriptors, 1),
        (GuestFailureStage::Session, 2),
        (GuestFailureStage::StandardIo, 3),
        (GuestFailureStage::Account, 4),
        (GuestFailureStage::Start, 5),
        (GuestFailureStage::Exec, 6),
        (GuestFailureStage::Signals, 7),
        (GuestFailureStage::Identity, 8),
        (GuestFailureStage::GroupDatabase, 9),
        (GuestFailureStage::SupplementaryGroups, 10),
        (GuestFailureStage::GroupIdentity, 11),
        (GuestFailureStage::UserIdentity, 12),
        (GuestFailureStage::HomeDirectory, 13),
        (GuestFailureStage::LoginShell, 14),
        (GuestFailureStage::LoginDisabled, 15),
        (GuestFailureStage::TerminalOwnership, 16),
        (GuestFailureStage::Environment, 17),
        (GuestFailureStage::Memory, 18),
    ];
    for (stage, number) in cases {
        assert_eq!(stage as u32, number);
        assert_eq!(GuestFailureStage::try_from(number), Ok(stage));
    }
}

#[test]
fn failure_stage_rejects_reserved_and_unknown_numbers() {
    for number in [0, 19, 0x1_0001, u32::MAX] {
        assert_eq!(
            GuestFailureStage::try_from(number),
            Err(SessionPayloadError::InvalidResponse)
        );
    }
}

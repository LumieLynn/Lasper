use super::*;

#[test]
fn selected_capacities_reject_build_valid_fields_at_the_application_boundary() {
    let spec = ProcessSpec {
        user: "alice".into(),
        program: Program::Argv(vec!["/bin/true".into(), "arg".into()]),
        stdio: Stdio::Piped,
        environment: [("EXTRA".into(), "value".into())].into(),
    };
    let request = spec.wire().unwrap();
    let baseline = *lasper_guest_protocol::protocol_profiles()[0].capacities();
    spec.validate_profile(&request, &baseline).unwrap();
    for (capacities, field) in [
        (
            ProfileCapacities {
                user_name_bytes: 4,
                ..baseline
            },
            "user",
        ),
        (
            ProfileCapacities {
                command_arguments: 0,
                ..baseline
            },
            "program.argv",
        ),
        (
            ProfileCapacities {
                command_argument_bytes: 8,
                ..baseline
            },
            "program.argv",
        ),
        (
            ProfileCapacities {
                command_total_bytes: 12,
                ..baseline
            },
            "program.argv",
        ),
        (
            ProfileCapacities {
                environment_entries: 0,
                ..baseline
            },
            "environment",
        ),
        (
            ProfileCapacities {
                environment_bytes: 11,
                ..baseline
            },
            "environment",
        ),
    ] {
        assert_eq!(
            spec.validate_profile(&request, &capacities)
                .unwrap_err()
                .field,
            field
        );
    }
    spec.validate_profile(
        &request,
        &ProfileCapacities {
            user_name_bytes: 5,
            command_arguments: 1,
            command_argument_bytes: 9,
            command_total_bytes: 13,
            environment_entries: 1,
            environment_bytes: 12,
            ..baseline
        },
    )
    .unwrap();
}

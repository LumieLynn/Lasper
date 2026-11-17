use super::*;

#[test]
fn profile_field_lists_reference_the_selected_typed_codec() {
    for profile in protocol_profiles() {
        for schema in profile.messages() {
            let fields = schema.codec.fields();
            assert_eq!(schema.required_fields, fields.required);
            assert_eq!(schema.optional_fields, fields.optional);
            assert_eq!(schema.gated_fields, fields.gated);
            for field in schema.gated_fields {
                assert!(
                    schema.required_fields.contains(&field.path)
                        || schema.optional_fields.contains(&field.path)
                );
                assert!(profile.features().contains(field.requires));
            }
        }
    }
    assert_eq!(
        BUILD_MERGED_ENVIRONMENT_BYTES,
        merged_environment_ceiling(protocol_profiles())
    );
}

#[test]
fn build_storage_combines_independent_profile_maxima() {
    let first = protocol_profiles()[0];
    let second = ProtocolProfile {
        capacities: ProfileCapacities {
            environment_bytes: first.capacities.environment_bytes * 2,
            max_prepared_lease_ms: first.capacities.max_prepared_lease_ms / 2,
            ..first.capacities
        },
        ..first
    };
    let third = ProtocolProfile {
        capacities: ProfileCapacities {
            command_argument_bytes: second.capacities.environment_bytes * 2,
            default_environment_bytes: first.capacities.default_environment_bytes * 3,
            ..first.capacities
        },
        ..first
    };
    let expected = ProfileCapacities {
        environment_bytes: second.capacities.environment_bytes,
        command_argument_bytes: third.capacities.command_argument_bytes,
        default_environment_bytes: third.capacities.default_environment_bytes,
        ..first.capacities
    };
    for profiles in [[first, second, third], [third, second, first]] {
        let capacities = build_capacities(&profiles);
        assert_eq!(capacities, expected);
        assert_eq!(
            capacities.prepare_unescape_bytes(),
            third.capacities.command_argument_bytes
        );
        assert_eq!(
            merged_environment_ceiling(&profiles),
            third.capacities.merged_environment_bytes()
        );
        assert!(merged_environment_ceiling(&profiles) < capacities.merged_environment_bytes());
    }
}

#[test]
fn each_profile_advertises_exactly_the_features_introduced_by_its_version() {
    for profile in protocol_profiles() {
        assert_eq!(profile.features(), features_for(profile.version()));
        assert!(profile.features().contains(profile.required_features()));
    }
    for definition in FEATURE_MANIFEST {
        assert!(features_for(definition.introduced).contains(definition.feature));
        if definition.introduced.minor > 0 {
            let previous = ProtocolVersion {
                minor: definition.introduced.minor - 1,
                ..definition.introduced
            };
            assert!(!features_for(previous).contains(definition.feature));
        }
    }
}

use super::*;
use lasper_guest_protocol::FeatureSet;

#[test]
fn profile_requires_pidfd_without_requiring_an_explicit_host_bit() {
    let readiness = GuestWorkerReadiness { _private: () };
    let request = HelloRequest::new(FeatureSet::EMPTY, ControlLimits::CURRENT).unwrap();
    assert!(readiness
        .accept_hello(
            ProtocolVersion::BOOTSTRAP,
            request,
            BrokerId::new([1; 16]),
            ControlLimits::CURRENT
        )
        .is_ok());
}

#[test]
fn accepted_worker_hello_keeps_negotiation_checks_and_limits() {
    let readiness = GuestWorkerReadiness { _private: () };
    let request = HelloRequest::new(
        FeatureSet::WORKER_PIDFD,
        ControlLimits::new(4096, 2).unwrap(),
    )
    .unwrap();
    let accepted = readiness
        .accept_hello(
            ProtocolVersion::BOOTSTRAP,
            request,
            BrokerId::new([1; 16]),
            ControlLimits::CURRENT,
        )
        .unwrap();
    assert_eq!(
        accepted.response().offer(),
        readiness.offer(ControlLimits::CURRENT)
    );
    assert_eq!(
        accepted.response().features(),
        FeatureSet::WORKER_PIDFD.union(FeatureSet::EXECUTION_TIMEOUT)
    );
    assert_eq!(accepted.limits().get(), request.limits());
    assert_eq!(
        accepted.limits(),
        request
            .negotiate_protocol(ProtocolVersion::BOOTSTRAP, accepted.response())
            .unwrap()
            .limits()
    );
    assert!(matches!(
        readiness.accept_hello(
            ProtocolVersion {
                major: ProtocolVersion::BOOTSTRAP.major + 1,
                ..ProtocolVersion::BOOTSTRAP
            },
            request,
            BrokerId::new([1; 16]),
            ControlLimits::CURRENT
        ),
        Err(GuestHelloError::Negotiation(
            CompatibilityError::InvalidOffer
        )),
    ));
    assert!(HelloRequest::new(FeatureSet::from_bits(1 << 8), ControlLimits::CURRENT).is_err());
}

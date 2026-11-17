use super::*;

#[test]
fn execution_budgets_round_up_without_wrapping() {
    for (duration, expected) in [
        (Duration::from_nanos(1), 1),
        (Duration::from_millis(1), 1),
        (Duration::from_nanos(1_000_001), 2),
        (Duration::from_secs(120), 120_000),
        (
            Duration::from_millis(MAX_EXECUTION_TIMEOUT_MS) - Duration::from_nanos(1),
            MAX_EXECUTION_TIMEOUT_MS,
        ),
        (
            Duration::from_millis(MAX_EXECUTION_TIMEOUT_MS),
            MAX_EXECUTION_TIMEOUT_MS,
        ),
    ] {
        assert_eq!(execution_timeout_ms(duration).unwrap(), expected);
    }
    for duration in [
        Duration::ZERO,
        Duration::from_millis(MAX_EXECUTION_TIMEOUT_MS) + Duration::from_nanos(1),
        Duration::from_millis(MAX_EXECUTION_TIMEOUT_MS + 1),
        Duration::from_millis(u64::MAX),
        Duration::from_millis(u64::MAX) + Duration::from_nanos(1),
        Duration::MAX,
    ] {
        assert!(matches!(
            execution_timeout_ms(duration),
            Err(BrokerSessionError::InvalidRequest(_))
        ));
    }
}

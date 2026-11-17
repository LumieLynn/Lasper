// The fixture codecs enter the same dispatch methods as production schemas.
// Their implementations and registry exist only in the unit-test build.
macro_rules! fixture_encode {
    (StartV1, $profile:expr, $frame:expr, $output:expr) => {
        crate::profile::schema_fixture::encode_start($profile, $frame, $output)
    };
    (ExitV1, $profile:expr, $frame:expr, $output:expr) => {
        crate::profile::schema_fixture::encode_exit($profile, $frame, $output)
    };
    ($codec:ident, $profile:expr, $frame:expr, $output:expr) => {
        None
    };
}

macro_rules! fixture_decode {
    (StartV1, $profile:expr, ($($arg:expr),*), $header:expr, $payload:expr, $fds:expr) => {
        crate::profile::schema_fixture::decode_start($profile, $($arg,)* $header, $payload, $fds)
    };
    (ExitV1, $profile:expr, ($($arg:expr),*), $header:expr, $payload:expr, $fds:expr) => {
        crate::profile::schema_fixture::decode_exit($profile, $($arg,)* $header, $payload, $fds)
    };
    ($codec:ident, $profile:expr, ($($arg:expr),*), $header:expr, $payload:expr, $fds:expr) => { None };
}

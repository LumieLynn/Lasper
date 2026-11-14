//! CLI names and help generated from the build-time command definition.

include!(concat!(env!("OUT_DIR"), "/cli_names.rs"));

pub const HELP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/help.txt"));
pub const WRAP_HELP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wrap_help.txt"));
pub const ERROR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/error.txt"));
pub const WRAP_ERROR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wrap_error.txt"));

use serde::{Deserialize, Serialize};

use super::SessionPayloadError;

/// The worker operation that failed, before an application could report its own status.
/// Discriminants are carried by the worker status pipe.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum GuestFailureStage {
    Descriptors = 1,
    Session = 2,
    StandardIo = 3,
    Account = 4,
    Start = 5,
    Exec = 6,
    Signals = 7,
    Identity = 8,
    GroupDatabase = 9,
    SupplementaryGroups = 10,
    GroupIdentity = 11,
    UserIdentity = 12,
    HomeDirectory = 13,
    LoginShell = 14,
    LoginDisabled = 15,
    TerminalOwnership = 16,
    Environment = 17,
    Memory = 18,
}

impl TryFrom<u32> for GuestFailureStage {
    type Error = SessionPayloadError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Descriptors),
            2 => Ok(Self::Session),
            3 => Ok(Self::StandardIo),
            4 => Ok(Self::Account),
            5 => Ok(Self::Start),
            6 => Ok(Self::Exec),
            7 => Ok(Self::Signals),
            8 => Ok(Self::Identity),
            9 => Ok(Self::GroupDatabase),
            10 => Ok(Self::SupplementaryGroups),
            11 => Ok(Self::GroupIdentity),
            12 => Ok(Self::UserIdentity),
            13 => Ok(Self::HomeDirectory),
            14 => Ok(Self::LoginShell),
            15 => Ok(Self::LoginDisabled),
            16 => Ok(Self::TerminalOwnership),
            17 => Ok(Self::Environment),
            18 => Ok(Self::Memory),
            _ => Err(SessionPayloadError::InvalidResponse),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestFailure {
    #[serde(deserialize_with = "crate::json::deserialize_string_enum")]
    stage: GuestFailureStage,
    errno: i32,
}

impl GuestFailure {
    pub fn new(stage: GuestFailureStage, errno: i32) -> Result<Self, SessionPayloadError> {
        let failure = Self { stage, errno };
        failure.validate()?;
        Ok(failure)
    }

    pub const fn stage(self) -> GuestFailureStage {
        self.stage
    }

    pub const fn errno(self) -> i32 {
        self.errno
    }

    pub(super) fn validate(self) -> Result<(), SessionPayloadError> {
        if !(1..=4095).contains(&self.errno) {
            return Err(SessionPayloadError::InvalidResponse);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/session/failure.rs"]
mod tests;

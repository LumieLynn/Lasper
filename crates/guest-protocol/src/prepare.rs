use core::fmt;
use heapless::{String, Vec};
use serde::de::{Error, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json_core::str::{EscapedStr, EscapedStringFragment};

use crate::json::{encode_json_payload, fragments};
use crate::{
    ControlFrameError, ControlFrameHeader, ControlFrameHeaderError, MessageKind, PayloadCodecError,
    CONTROL_FRAME_HEADER_BYTES,
};

mod environment;
mod storage;
#[cfg(test)]
#[path = "../tests/prepare/codec.rs"]
mod tests;
mod wire;

pub use environment::*;
pub use storage::PrepareDecodeStorage;

pub const MAX_GUEST_USER_NAME_BYTES: usize = crate::BUILD_CAPACITIES.user_name_bytes;
pub const MAX_COMMAND_ARGUMENTS: usize = crate::BUILD_CAPACITIES.command_arguments;
pub const MAX_COMMAND_ARGUMENT_BYTES: usize = crate::BUILD_CAPACITIES.command_argument_bytes;
/// Budget for argv bytes plus NUL separators between entries; excludes the final NUL.
/// The execution buffer therefore needs one byte more than this limit.
pub const MAX_COMMAND_TOTAL_BYTES: usize = crate::BUILD_CAPACITIES.command_total_bytes;
const MAX_ARGV_ITEMS: usize = MAX_COMMAND_ARGUMENTS + 1;
const PREPARATION_ID_BYTES: usize = 16;

pub const MAX_PREPARED_LEASE_MS: u32 = crate::BUILD_CAPACITIES.max_prepared_lease_ms;

/// How the prepared process will receive input and output.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    Terminal,
    Piped,
}

/// The process to create after the guest account has been resolved.
// The fixed argv view lets the no-allocator guest validate every argument.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestProcess<'a> {
    LoginShell,
    Command(#[serde(borrow)] CommandArgv<'a>),
}

#[derive(Clone, Copy, Debug)]
enum CommandArgument<'a> {
    Plain(&'a str),
    Escaped(EscapedStr<'a>),
}

impl CommandArgument<'_> {
    fn decoded_len(self) -> Result<usize, PayloadCodecError> {
        match self {
            Self::Plain(value) => {
                if value.contains('\0') {
                    return Err(PayloadCodecError::InvalidRequest);
                }
                Ok(value.len())
            }
            Self::Escaped(value) => {
                let mut length = 0usize;
                for fragment in fragments(value) {
                    let fragment_length =
                        match fragment.map_err(|_| PayloadCodecError::InvalidRequest)? {
                            EscapedStringFragment::NotEscaped(value) => {
                                if value.contains('\0') {
                                    return Err(PayloadCodecError::InvalidRequest);
                                }
                                value.len()
                            }
                            EscapedStringFragment::Escaped('\0') => {
                                return Err(PayloadCodecError::InvalidRequest);
                            }
                            EscapedStringFragment::Escaped(value) => value.len_utf8(),
                        };
                    length = length
                        .checked_add(fragment_length)
                        .ok_or(PayloadCodecError::InvalidRequest)?;
                }
                Ok(length)
            }
        }
    }

    fn starts_with_slash(self) -> Result<bool, PayloadCodecError> {
        match self {
            Self::Plain(value) => Ok(value.starts_with('/')),
            Self::Escaped(value) => {
                for fragment in fragments(value) {
                    match fragment.map_err(|_| PayloadCodecError::InvalidRequest)? {
                        EscapedStringFragment::NotEscaped(value) => {
                            if let Some(character) = value.chars().next() {
                                return Ok(character == '/');
                            }
                        }
                        EscapedStringFragment::Escaped(value) => return Ok(value == '/'),
                    }
                }
                Ok(false)
            }
        }
    }

    fn write_decoded(self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        let length = self.decoded_len()?;
        if output.len() < length {
            return Err(PayloadCodecError::OutputTooSmall);
        }
        match self {
            Self::Plain(value) => output[..length].copy_from_slice(value.as_bytes()),
            Self::Escaped(value) => {
                let mut offset = 0;
                for fragment in fragments(value) {
                    let bytes = match fragment.map_err(|_| PayloadCodecError::InvalidRequest)? {
                        EscapedStringFragment::NotEscaped(value) => value.as_bytes(),
                        EscapedStringFragment::Escaped(value) => {
                            let mut encoded_buffer = [0; 4];
                            let encoded = value.encode_utf8(&mut encoded_buffer);
                            let end = offset + encoded.len();
                            output[offset..end].copy_from_slice(encoded.as_bytes());
                            offset = end;
                            continue;
                        }
                    };
                    let end = offset + bytes.len();
                    output[offset..end].copy_from_slice(bytes);
                    offset = end;
                }
            }
        }
        Ok(length)
    }
}

impl Serialize for CommandArgument<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Plain(value) => serializer.serialize_str(value),
            Self::Escaped(value) => value.serialize(serializer),
        }
    }
}

/// A bounded argv view. Decoded arguments borrow the control payload; callers
/// provide the execution buffer when they need NUL-terminated strings.
#[derive(Clone, Debug)]
pub struct CommandArgv<'a> {
    values: Vec<CommandArgument<'a>, MAX_ARGV_ITEMS>,
    total_bytes: usize,
    invalid_wire_value: bool,
}

impl<'a> CommandArgv<'a> {
    const fn empty() -> Self {
        Self {
            values: Vec::new(),
            total_bytes: 0,
            invalid_wire_value: false,
        }
    }

    fn clear(&mut self) {
        self.values.clear();
        self.total_bytes = 0;
        self.invalid_wire_value = false;
    }

    pub fn new(argv: &[&'a str]) -> Result<Self, PayloadCodecError> {
        let mut result = Self {
            values: Vec::new(),
            total_bytes: 0,
            invalid_wire_value: false,
        };
        for value in argv {
            result.push(CommandArgument::Plain(value))?;
        }
        result.validate()?;
        Ok(result)
    }

    pub fn argument_count(&self) -> usize {
        self.values.len().saturating_sub(1)
    }

    /// Copies decoded arguments into NUL-terminated slots for `execve`.
    pub fn write_nul_terminated_into(&self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        self.validate()?;
        let mut required = self.values.len();
        for value in &self.values {
            required = required
                .checked_add(value.decoded_len()?)
                .ok_or(PayloadCodecError::InvalidRequest)?;
        }
        if output.len() < required {
            return Err(PayloadCodecError::OutputTooSmall);
        }

        let mut offset = 0;
        for value in &self.values {
            let written = value.write_decoded(&mut output[offset..])?;
            offset += written;
            output[offset] = 0;
            offset += 1;
        }
        Ok(offset)
    }

    fn push(&mut self, value: CommandArgument<'a>) -> Result<(), PayloadCodecError> {
        if self.values.len() == MAX_ARGV_ITEMS {
            return Err(PayloadCodecError::InvalidRequest);
        }
        let value_length = value.decoded_len()?;
        if value_length > MAX_COMMAND_ARGUMENT_BYTES {
            return Err(PayloadCodecError::InvalidRequest);
        }
        let separator_length = usize::from(!self.values.is_empty());
        let next_total = self
            .total_bytes
            .checked_add(separator_length)
            .and_then(|length| length.checked_add(value_length))
            .ok_or(PayloadCodecError::InvalidRequest)?;
        if next_total > MAX_COMMAND_TOTAL_BYTES {
            return Err(PayloadCodecError::InvalidRequest);
        }
        self.values
            .push(value)
            .map_err(|_| PayloadCodecError::InvalidRequest)?;
        self.total_bytes = next_total;
        Ok(())
    }

    fn validate(&self) -> Result<(), PayloadCodecError> {
        self.validate_with(&crate::profile::BUILD_CAPACITIES)
    }

    fn validate_with(
        &self,
        capacities: &crate::ProfileCapacities,
    ) -> Result<(), PayloadCodecError> {
        if self.invalid_wire_value {
            return Err(PayloadCodecError::InvalidRequest);
        }
        let Some(program) = self.values.first().copied() else {
            return Err(PayloadCodecError::InvalidRequest);
        };
        if program.decoded_len()? == 0 || !program.starts_with_slash()? {
            return Err(PayloadCodecError::InvalidRequest);
        }
        if self.argument_count() > capacities.command_arguments
            || self.total_bytes > capacities.command_total_bytes
            || self.values.iter().any(|value| {
                value
                    .decoded_len()
                    .map_or(true, |length| length > capacities.command_argument_bytes)
            })
        {
            return Err(PayloadCodecError::InvalidRequest);
        }
        Ok(())
    }
}

impl<'a> CommandArgv<'a> {
    fn push_wire_value(&mut self, value: CommandArgument<'a>) {
        if self.values.len() == MAX_ARGV_ITEMS {
            self.invalid_wire_value = true;
            return;
        }

        if !self.invalid_wire_value {
            match value.decoded_len() {
                Ok(value_length) if value_length <= MAX_COMMAND_ARGUMENT_BYTES => {
                    let separator_length = usize::from(!self.values.is_empty());
                    match self
                        .total_bytes
                        .checked_add(separator_length)
                        .and_then(|length| length.checked_add(value_length))
                    {
                        Some(total) if total <= MAX_COMMAND_TOTAL_BYTES => {
                            self.total_bytes = total;
                        }
                        _ => self.invalid_wire_value = true,
                    }
                }
                _ => self.invalid_wire_value = true,
            }
        }

        if self.values.push(value).is_err() {
            self.invalid_wire_value = true;
        }
    }
}

impl Serialize for CommandArgv<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.values.len()))?;
        for value in &self.values {
            sequence.serialize_element(value)?;
        }
        sequence.end()
    }
}

/// Terminal dimensions carried by Prepare and Start.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TerminalSize {
    cols: u16,
    rows: u16,
}

impl TerminalSize {
    pub fn new(cols: u16, rows: u16) -> Result<Self, PayloadCodecError> {
        let size = Self { cols, rows };
        size.validate()?;
        Ok(size)
    }

    fn validate(self) -> Result<(), PayloadCodecError> {
        if self.cols == 0 || self.rows == 0 {
            return Err(PayloadCodecError::InvalidRequest);
        }
        Ok(())
    }

    pub const fn cols(self) -> u16 {
        self.cols
    }

    pub const fn rows(self) -> u16 {
        self.rows
    }
}

impl<'de> Deserialize<'de> for TerminalSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireSize {
            cols: u16,
            rows: u16,
        }

        let size = WireSize::deserialize(deserializer)?;
        Self::new(size.cols, size.rows)
            .map_err(|_| D::Error::custom("terminal dimensions must be nonzero"))
    }
}

/// A process request whose I/O shape is fixed before worker creation.
#[derive(Clone, Debug, Serialize)]
pub struct PrepareRequest<'a> {
    user: String<MAX_GUEST_USER_NAME_BYTES>,
    execution: ExecutionKind,
    process: GuestProcess<'a>,
    environment: ProcessEnvironment<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_size: Option<TerminalSize>,
}

impl<'a> PrepareRequest<'a> {
    pub const KIND: MessageKind = MessageKind::PrepareRequest;

    pub fn new(
        user: &str,
        execution: ExecutionKind,
        process: GuestProcess<'a>,
        environment: ProcessEnvironment<'a>,
        initial_size: Option<TerminalSize>,
    ) -> Result<Self, PayloadCodecError> {
        let request = Self {
            user: bounded_string(user)?,
            execution,
            process,
            environment,
            initial_size,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn environment(&self) -> &ProcessEnvironment<'a> {
        &self.environment
    }

    pub fn user(&self) -> &str {
        self.user.as_str()
    }

    pub const fn execution(&self) -> ExecutionKind {
        self.execution
    }

    pub fn process(&self) -> &GuestProcess<'a> {
        &self.process
    }

    pub const fn initial_size(&self) -> Option<TerminalSize> {
        self.initial_size
    }

    pub(crate) fn encode_payload(&self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        self.validate()?;
        encode_json_payload(self, output)
    }

    fn validate(&self) -> Result<(), PayloadCodecError> {
        self.validate_with(&crate::profile::BUILD_CAPACITIES)
    }

    pub(crate) fn validate_with(
        &self,
        capacities: &crate::ProfileCapacities,
    ) -> Result<(), PayloadCodecError> {
        validate_user_name(self.user.as_str())?;
        if self.user.len() > capacities.user_name_bytes {
            return Err(PayloadCodecError::InvalidRequest);
        }
        self.environment.validate(capacities)?;
        if let GuestProcess::Command(argv) = &self.process {
            argv.validate_with(capacities)?;
        }
        match self.execution {
            ExecutionKind::Terminal => {
                self.initial_size
                    .ok_or(PayloadCodecError::InvalidRequest)?
                    .validate()?;
            }
            ExecutionKind::Piped => {
                if self.initial_size.is_some() {
                    return Err(PayloadCodecError::InvalidRequest);
                }
            }
        }
        Ok(())
    }
}

/// Identifies a preparation on this connection. It is not a cross-connection
/// lookup key and cannot be used to reattach to a worker.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PreparationId([u8; PREPARATION_ID_BYTES]);

impl PreparationId {
    pub const fn new(bytes: [u8; PREPARATION_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; PREPARATION_ID_BYTES] {
        &self.0
    }
}

/// Acknowledges prepared resources. The worker has not yet been authorized to exec.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PrepareReady {
    preparation_id: PreparationId,
    broker_id: crate::BrokerId,
    remaining_lease_ms: u32,
}

impl PrepareReady {
    pub const KIND: crate::MessageKind = crate::MessageKind::PrepareReady;

    pub fn new(
        preparation_id: PreparationId,
        broker_id: crate::BrokerId,
        remaining_lease_ms: u32,
    ) -> Result<Self, PayloadCodecError> {
        let response = Self {
            preparation_id,
            broker_id,
            remaining_lease_ms,
        };
        response.validate()?;
        Ok(response)
    }

    pub const fn preparation_id(self) -> PreparationId {
        self.preparation_id
    }

    pub const fn broker_id(self) -> crate::BrokerId {
        self.broker_id
    }

    pub const fn remaining_lease_ms(self) -> u32 {
        self.remaining_lease_ms
    }

    /// Descriptor count attached to PrepareReady for this request type.
    pub const fn descriptor_count_for(execution: ExecutionKind) -> u16 {
        match execution {
            ExecutionKind::Terminal => 3,
            ExecutionKind::Piped => 4,
        }
    }

    /// FD order for this I/O form. The guardian must acquire the worker pidfd
    /// before reaping can release the child's PID; the host must not reconstruct
    /// it from a PID reported in the payload.
    pub const fn descriptor_role_for(
        execution: ExecutionKind,
        index: u16,
    ) -> Option<PrepareReadyDescriptorRole> {
        match (execution, index) {
            (ExecutionKind::Terminal, 0) => Some(PrepareReadyDescriptorRole::PtyMaster),
            (ExecutionKind::Terminal, 1) | (ExecutionKind::Piped, 0) => {
                Some(PrepareReadyDescriptorRole::IdentityChannel)
            }
            (ExecutionKind::Terminal, 2) | (ExecutionKind::Piped, 1) => {
                Some(PrepareReadyDescriptorRole::WorkerPidfd)
            }
            (ExecutionKind::Piped, 2) => Some(PrepareReadyDescriptorRole::Stdout),
            (ExecutionKind::Piped, 3) => Some(PrepareReadyDescriptorRole::Stderr),
            _ => None,
        }
    }

    fn decode_payload(bytes: &[u8]) -> Result<Self, PayloadCodecError> {
        let wire: WirePrepareReady = crate::json::decode_json_payload(bytes)?;
        Self::new(
            PreparationId::new(wire.preparation_id),
            wire.broker_id,
            wire.remaining_lease_ms,
        )
    }

    fn validate(self) -> Result<(), PayloadCodecError> {
        if self.remaining_lease_ms == 0 || self.remaining_lease_ms > MAX_PREPARED_LEASE_MS {
            return Err(PayloadCodecError::InvalidRequest);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePrepareReady {
    preparation_id: [u8; PREPARATION_ID_BYTES],
    broker_id: crate::BrokerId,
    remaining_lease_ms: u32,
}

/// Meaning of one descriptor attached to a `PrepareReady` frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareReadyDescriptorRole {
    Stdout,
    Stderr,
    PtyMaster,
    IdentityChannel,
    WorkerPidfd,
}

/// A Prepare request with its connection-local correlation ID and no attached FDs.
/// The broker ID is retained from Hello for checking subsequent responses.
#[derive(Clone, Debug)]
pub struct PrepareRequestFrame<'a> {
    request_id: u64,
    broker_id: crate::BrokerId,
    request: PrepareRequest<'a>,
}

impl<'a> PrepareRequestFrame<'a> {
    pub const fn new(
        request_id: u64,
        broker_id: crate::BrokerId,
        request: PrepareRequest<'a>,
    ) -> Self {
        Self {
            request_id,
            broker_id,
            request,
        }
    }

    pub const fn request_id(&self) -> u64 {
        self.request_id
    }

    pub const fn broker_id(&self) -> crate::BrokerId {
        self.broker_id
    }

    pub const fn request(&self) -> &PrepareRequest<'a> {
        &self.request
    }

    /// Encodes the header and JSON payload. The caller sends no descriptors.
    pub(crate) fn encode_into_with_version(
        &self,
        version: crate::ProtocolVersion,
        output: &mut [u8],
    ) -> Result<usize, PrepareFrameError> {
        encode_prepare_frame(
            version,
            PrepareRequest::KIND,
            self.request_id,
            0,
            &self.request,
            output,
        )
    }

    /// Checks the Prepare message kind and declared and received FD counts.
    /// Version and capacity checks belong to the connection's header validator.
    pub fn validate_header(
        header: ControlFrameHeader,
        received_descriptors: usize,
    ) -> Result<(), PrepareFrameError> {
        validate_prepare_header(header, received_descriptors, PrepareRequest::KIND, 0)
    }

    pub fn ready_frame(
        &self,
        response: PrepareReady,
    ) -> Result<PrepareReadyFrame, PrepareFrameError> {
        if response.broker_id() != self.broker_id {
            return Err(PrepareFrameError::MismatchedBrokerId {
                expected: self.broker_id,
                actual: response.broker_id(),
            });
        }
        Ok(PrepareReadyFrame {
            request_id: self.request_id,
            execution: self.request.execution(),
            response,
        })
    }

    /// Checks the split Ready envelope and payload against this request. The
    /// transport retains FD ownership; only its received count is checked here.
    pub(crate) fn decode_ready_parts(
        &self,
        header: ControlFrameHeader,
        payload: &[u8],
        received_descriptors: usize,
    ) -> Result<PrepareReadyFrame, PrepareFrameError> {
        validate_prepare_parts(
            header,
            payload,
            received_descriptors,
            PrepareReady::KIND,
            PrepareReady::descriptor_count_for(self.request.execution()),
        )?;
        PrepareReadyFrame::from_validated_parts(self, header, payload)
    }
}

/// A complete `PrepareReady` response paired with the request that defines its FD contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrepareReadyFrame {
    request_id: u64,
    execution: ExecutionKind,
    response: PrepareReady,
}

impl PrepareReadyFrame {
    pub const fn request_id(self) -> u64 {
        self.request_id
    }

    pub const fn response(self) -> PrepareReady {
        self.response
    }

    pub const fn descriptor_count(self) -> u16 {
        PrepareReady::descriptor_count_for(self.execution)
    }

    pub const fn descriptor_role(self, index: u16) -> Option<PrepareReadyDescriptorRole> {
        PrepareReady::descriptor_role_for(self.execution, index)
    }

    /// The host supplies a fresh random nonce on every check, including Recheck,
    /// so an earlier reply from the same prepared worker cannot satisfy it.
    pub const fn identity_challenge(
        self,
        nonce: [u8; crate::IDENTITY_CHALLENGE_BYTES],
    ) -> crate::IdentityChallenge {
        crate::IdentityChallenge::new(self.response.preparation_id(), nonce)
    }

    /// Encodes the response metadata. The caller attaches FDs in `descriptor_role` order.
    pub(crate) fn encode_into_with_version(
        self,
        version: crate::ProtocolVersion,
        output: &mut [u8],
    ) -> Result<usize, PrepareFrameError> {
        encode_prepare_frame(
            version,
            PrepareReady::KIND,
            self.request_id,
            self.descriptor_count(),
            &self.response,
            output,
        )
    }

    fn from_validated_parts(
        request: &PrepareRequestFrame<'_>,
        header: ControlFrameHeader,
        payload: &[u8],
    ) -> Result<Self, PrepareFrameError> {
        if header.request_id() != request.request_id {
            return Err(PrepareFrameError::MismatchedRequestId {
                expected: request.request_id,
                actual: header.request_id(),
            });
        }
        let response = PrepareReady::decode_payload(payload).map_err(PrepareFrameError::Payload)?;
        if response.broker_id() != request.broker_id {
            return Err(PrepareFrameError::MismatchedBrokerId {
                expected: request.broker_id,
                actual: response.broker_id(),
            });
        }
        Ok(PrepareReadyFrame {
            request_id: request.request_id,
            execution: request.request.execution(),
            response,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareFrameError {
    Frame(ControlFrameError),
    Header(ControlFrameHeaderError),
    ProtocolVersion(crate::ProtocolVersionMismatch),
    Limit(crate::ControlLimitError),
    Payload(PayloadCodecError),
    UnexpectedMessageKind {
        expected: MessageKind,
        actual: MessageKind,
    },
    UnexpectedDescriptorCount {
        expected: u16,
        actual: u16,
    },
    MismatchedRequestId {
        expected: u64,
        actual: u64,
    },
    MismatchedBrokerId {
        expected: crate::BrokerId,
        actual: crate::BrokerId,
    },
}

fn encode_prepare_frame<T: Serialize + ?Sized>(
    version: crate::ProtocolVersion,
    kind: MessageKind,
    request_id: u64,
    descriptor_count: u16,
    payload: &T,
    output: &mut [u8],
) -> Result<usize, PrepareFrameError> {
    if output.len() < CONTROL_FRAME_HEADER_BYTES {
        return Err(PrepareFrameError::Frame(
            ControlFrameError::OutputTooSmall {
                required: CONTROL_FRAME_HEADER_BYTES,
                available: output.len(),
            },
        ));
    }
    let payload_bytes = encode_json_payload(payload, &mut output[CONTROL_FRAME_HEADER_BYTES..])
        .map_err(PrepareFrameError::Payload)?;
    let header = ControlFrameHeader::new_with_version(
        version,
        kind,
        request_id,
        payload_bytes as u32,
        descriptor_count,
    )
    .map_err(PrepareFrameError::Header)?;
    output[..CONTROL_FRAME_HEADER_BYTES].copy_from_slice(&header.encode());
    Ok(CONTROL_FRAME_HEADER_BYTES + payload_bytes)
}

fn validate_prepare_parts(
    header: ControlFrameHeader,
    payload: &[u8],
    received_descriptors: usize,
    expected_kind: MessageKind,
    expected_descriptors: u16,
) -> Result<(), PrepareFrameError> {
    validate_prepare_header(
        header,
        received_descriptors,
        expected_kind,
        expected_descriptors,
    )?;
    header
        .validate_payload_length(payload.len())
        .map_err(PrepareFrameError::Frame)
}

fn validate_prepare_header(
    header: ControlFrameHeader,
    received_descriptors: usize,
    expected_kind: MessageKind,
    expected_descriptors: u16,
) -> Result<(), PrepareFrameError> {
    if header.kind() != expected_kind {
        return Err(PrepareFrameError::UnexpectedMessageKind {
            expected: expected_kind,
            actual: header.kind(),
        });
    }
    if header.descriptor_count() != expected_descriptors {
        return Err(PrepareFrameError::UnexpectedDescriptorCount {
            expected: expected_descriptors,
            actual: header.descriptor_count(),
        });
    }
    header
        .validate_descriptor_count(received_descriptors)
        .map_err(PrepareFrameError::Frame)?;
    Ok(())
}

fn bounded_string<const N: usize>(value: &str) -> Result<String<N>, PayloadCodecError> {
    let mut result = String::new();
    result
        .push_str(value)
        .map_err(|_| PayloadCodecError::InvalidRequest)?;
    Ok(result)
}

fn validate_user_name(value: &str) -> Result<(), PayloadCodecError> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_GUEST_USER_NAME_BYTES
        || !value.is_ascii()
        || bytes[0] == b'-'
        || bytes.iter().any(|byte| {
            byte.is_ascii_whitespace()
                || byte.is_ascii_control()
                || matches!(*byte, b'/' | b'\\' | b'@')
        })
    {
        return Err(PayloadCodecError::InvalidRequest);
    }
    Ok(())
}

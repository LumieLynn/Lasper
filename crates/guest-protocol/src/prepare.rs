use core::{fmt, marker::PhantomData};
use heapless::{String, Vec};
use serde::de::{Error, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json_core::str::{EscapedStr, EscapedStringFragment};

use crate::messages::{decode_json_payload_escaped, encode_json_payload};
use crate::{
    ControlFrame, ControlFrameError, ControlFrameHeader, ControlFrameHeaderError, MessageKind,
    PayloadCodecError, CONTROL_FRAME_HEADER_BYTES,
};

pub const MAX_GUEST_USER_NAME_BYTES: usize = 32;
pub const MAX_PROJECTION_PATH_BYTES: usize = 4096;
pub const MAX_COMMAND_ARGUMENTS: usize = 256;
pub const MAX_COMMAND_ARGUMENT_BYTES: usize = 4096;
/// Budget for argv bytes plus NUL separators between entries; excludes the final NUL.
/// The execution buffer therefore needs one byte more than this limit.
pub const MAX_COMMAND_TOTAL_BYTES: usize = 64 * 1024;
const MAX_ARGV_ITEMS: usize = MAX_COMMAND_ARGUMENTS + 1;
const MAX_TERM_BYTES: usize = 256;
const MAX_TERMINAL_ENVIRONMENT_VALUE_BYTES: usize = 4096;
const PREPARATION_ID_BYTES: usize = 16;

pub const MAX_PREPARED_LEASE_MS: u32 = 45_000;

/// How the prepared process will receive input and output.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    Interactive,
    Bounded,
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
                for fragment in value.fragments() {
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
                for fragment in value.fragments() {
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
                for fragment in value.fragments() {
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
        if self.invalid_wire_value {
            return Err(PayloadCodecError::InvalidRequest);
        }
        let Some(program) = self.values.first().copied() else {
            return Err(PayloadCodecError::InvalidRequest);
        };
        if program.decoded_len()? == 0 || !program.starts_with_slash()? {
            return Err(PayloadCodecError::InvalidRequest);
        }
        if self.values.len() > MAX_ARGV_ITEMS || self.total_bytes > MAX_COMMAND_TOTAL_BYTES {
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

struct WireCommandArgv<'a>(CommandArgv<'a>);

impl<'de: 'a, 'a> Deserialize<'de> for WireCommandArgv<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CommandArgvVisitor<'a>(PhantomData<&'a ()>);

        impl<'de: 'a, 'a> Visitor<'de> for CommandArgvVisitor<'a> {
            type Value = WireCommandArgv<'a>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded argv array")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut argv = CommandArgv {
                    values: Vec::new(),
                    total_bytes: 0,
                    invalid_wire_value: false,
                };
                while let Some(value) = sequence.next_element::<EscapedStr<'de>>()? {
                    for fragment in value.fragments() {
                        match fragment
                            .map_err(|_| A::Error::custom("invalid JSON string escape"))?
                        {
                            EscapedStringFragment::NotEscaped(text)
                                if text.bytes().any(|byte| byte < 0x20) =>
                            {
                                return Err(A::Error::custom(
                                    "unescaped control character in JSON string",
                                ));
                            }
                            _ => {}
                        }
                    }
                    argv.push_wire_value(CommandArgument::Escaped(value));
                }
                Ok(WireCommandArgv(argv))
            }
        }

        deserializer.deserialize_seq(CommandArgvVisitor(PhantomData))
    }
}

/// Initial dimensions for an interactive PTY.
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

/// Terminal capabilities passed to an interactive guest process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalEnvironment {
    term: String<MAX_TERM_BYTES>,
    #[serde(skip_serializing_if = "Option::is_none")]
    colorterm: Option<String<MAX_TERMINAL_ENVIRONMENT_VALUE_BYTES>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_color: Option<String<MAX_TERMINAL_ENVIRONMENT_VALUE_BYTES>>,
}

impl TerminalEnvironment {
    pub fn new(
        term: &str,
        colorterm: Option<&str>,
        no_color: Option<&str>,
    ) -> Result<Self, PayloadCodecError> {
        let term = bounded_string::<MAX_TERM_BYTES>(term)?;
        let colorterm = colorterm.map(bounded_string).transpose()?;
        let no_color = no_color.map(bounded_string).transpose()?;
        let environment = Self {
            term,
            colorterm,
            no_color,
        };
        environment.validate()?;
        Ok(environment)
    }

    fn validate(&self) -> Result<(), PayloadCodecError> {
        let term = self.term.as_str();
        if term.is_empty()
            || term == "unknown"
            || !term.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'+' | b'.')
            })
        {
            return Err(PayloadCodecError::InvalidRequest);
        }
        for value in [self.colorterm.as_ref(), self.no_color.as_ref()]
            .into_iter()
            .flatten()
        {
            if value.chars().any(char::is_control) {
                return Err(PayloadCodecError::InvalidRequest);
            }
        }
        Ok(())
    }

    pub fn term(&self) -> &str {
        self.term.as_str()
    }

    pub fn colorterm(&self) -> Option<&str> {
        self.colorterm.as_ref().map(String::as_str)
    }

    pub fn no_color(&self) -> Option<&str> {
        self.no_color.as_ref().map(String::as_str)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaylandProjectionRequest {
    target: String<MAX_PROJECTION_PATH_BYTES>,
}

impl WaylandProjectionRequest {
    pub fn new(target: &str) -> Result<Self, PayloadCodecError> {
        let request = Self {
            target: bounded_string(target)?,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), PayloadCodecError> {
        validate_absolute_path(self.target.as_str())
    }

    pub fn target(&self) -> &str {
        self.target.as_str()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct X11ProjectionRequest {
    display: u16,
    mount_target: String<MAX_PROJECTION_PATH_BYTES>,
    client_path: String<MAX_PROJECTION_PATH_BYTES>,
}

impl X11ProjectionRequest {
    pub fn new(
        display: u16,
        mount_target: &str,
        client_path: &str,
    ) -> Result<Self, PayloadCodecError> {
        let request = Self {
            display,
            mount_target: bounded_string(mount_target)?,
            client_path: bounded_string(client_path)?,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), PayloadCodecError> {
        validate_absolute_path(self.mount_target.as_str())?;
        validate_absolute_path(self.client_path.as_str())
    }

    pub const fn display(&self) -> u16 {
        self.display
    }

    pub fn mount_target(&self) -> &str {
        self.mount_target.as_str()
    }

    pub fn client_path(&self) -> &str {
        self.client_path.as_str()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionRequests {
    #[serde(skip_serializing_if = "Option::is_none")]
    wayland: Option<WaylandProjectionRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    x11: Option<X11ProjectionRequest>,
}

impl ProjectionRequests {
    pub fn new(
        wayland: Option<WaylandProjectionRequest>,
        x11: Option<X11ProjectionRequest>,
    ) -> Self {
        Self { wayland, x11 }
    }

    fn validate(&self) -> Result<(), PayloadCodecError> {
        if let Some(wayland) = &self.wayland {
            wayland.validate()?;
        }
        if let Some(x11) = &self.x11 {
            x11.validate()?;
        }
        Ok(())
    }

    pub fn wayland(&self) -> Option<&WaylandProjectionRequest> {
        self.wayland.as_ref()
    }

    pub fn x11(&self) -> Option<&X11ProjectionRequest> {
        self.x11.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.wayland.is_none() && self.x11.is_none()
    }
}

/// One bounded request to prepare a guest process and its requested projections.
#[derive(Clone, Debug, Serialize)]
pub struct PrepareRequest<'a> {
    user: String<MAX_GUEST_USER_NAME_BYTES>,
    execution: ExecutionKind,
    process: GuestProcess<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal: Option<TerminalEnvironment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_size: Option<TerminalSize>,
    #[serde(default, skip_serializing_if = "ProjectionRequests::is_empty")]
    projections: ProjectionRequests,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
// The guest decoder keeps argv storage inline and does not allocate.
#[allow(clippy::large_enum_variant)]
enum WireGuestProcess<'a> {
    LoginShell,
    Command(#[serde(borrow)] WireCommandArgv<'a>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "'de: 'a"))]
struct WirePrepareRequest<'a> {
    user: String<MAX_GUEST_USER_NAME_BYTES>,
    execution: ExecutionKind,
    #[serde(borrow)]
    process: WireGuestProcess<'a>,
    terminal: Option<TerminalEnvironment>,
    initial_size: Option<WireTerminalSize>,
    #[serde(default)]
    projections: ProjectionRequests,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTerminalSize {
    cols: u16,
    rows: u16,
}

impl<'a> WirePrepareRequest<'a> {
    fn into_request(self) -> Result<PrepareRequest<'a>, PayloadCodecError> {
        let process = match self.process {
            WireGuestProcess::LoginShell => GuestProcess::LoginShell,
            WireGuestProcess::Command(argv) => GuestProcess::Command(argv.0),
        };
        let initial_size = self
            .initial_size
            .map(|size| TerminalSize::new(size.cols, size.rows))
            .transpose()?;
        let request = PrepareRequest {
            user: self.user,
            execution: self.execution,
            process,
            terminal: self.terminal,
            initial_size,
            projections: self.projections,
        };
        request.validate()?;
        Ok(request)
    }
}

impl<'a> PrepareRequest<'a> {
    pub const KIND: MessageKind = MessageKind::PrepareRequest;

    pub fn new(
        user: &str,
        execution: ExecutionKind,
        process: GuestProcess<'a>,
        terminal: Option<TerminalEnvironment>,
        initial_size: Option<TerminalSize>,
        projections: ProjectionRequests,
    ) -> Result<Self, PayloadCodecError> {
        let request = Self {
            user: bounded_string(user)?,
            execution,
            process,
            terminal,
            initial_size,
            projections,
        };
        request.validate()?;
        Ok(request)
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

    pub fn terminal(&self) -> Option<&TerminalEnvironment> {
        self.terminal.as_ref()
    }

    pub const fn initial_size(&self) -> Option<TerminalSize> {
        self.initial_size
    }

    pub fn projections(&self) -> &ProjectionRequests {
        &self.projections
    }

    pub fn encode_payload(&self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        self.validate()?;
        encode_json_payload(self, output)
    }

    pub fn decode_payload(bytes: &'a [u8]) -> Result<Self, PayloadCodecError> {
        let mut unescape_buffer = [0; MAX_COMMAND_ARGUMENT_BYTES];
        let wire: WirePrepareRequest<'a> =
            decode_json_payload_escaped(bytes, &mut unescape_buffer)?;
        wire.into_request()
    }

    fn validate(&self) -> Result<(), PayloadCodecError> {
        validate_user_name(self.user.as_str())?;
        self.projections.validate()?;
        if let GuestProcess::Command(argv) = &self.process {
            argv.validate()?;
        }
        match self.execution {
            ExecutionKind::Interactive => {
                let terminal = self
                    .terminal
                    .as_ref()
                    .ok_or(PayloadCodecError::InvalidRequest)?;
                terminal.validate()?;
                self.initial_size
                    .ok_or(PayloadCodecError::InvalidRequest)?
                    .validate()?;
            }
            ExecutionKind::Bounded => {
                if !matches!(self.process, GuestProcess::Command(_))
                    || self.terminal.is_some()
                    || self.initial_size.is_some()
                    || !self.projections.is_empty()
                {
                    return Err(PayloadCodecError::InvalidRequest);
                }
            }
        }
        Ok(())
    }
}

/// Identifies one prepared guest process within a broker generation.
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

/// Guest acknowledgement that resources for one request have been prepared.
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
            ExecutionKind::Interactive => 3,
            ExecutionKind::Bounded => 2,
        }
    }

    /// Interactive requests add a PTY master before the identity channel and
    /// assigned worker pidfd. The guardian must obtain that pidfd while it
    /// still owns the child, before reaping can release the child's PID.
    pub const fn descriptor_role_for(
        execution: ExecutionKind,
        index: u16,
    ) -> Option<PrepareReadyDescriptorRole> {
        match (execution, index) {
            (ExecutionKind::Interactive, 0) => Some(PrepareReadyDescriptorRole::PtyMaster),
            (ExecutionKind::Interactive, 1) | (ExecutionKind::Bounded, 0) => {
                Some(PrepareReadyDescriptorRole::IdentityChannel)
            }
            (ExecutionKind::Interactive, 2) | (ExecutionKind::Bounded, 1) => {
                Some(PrepareReadyDescriptorRole::WorkerPidfd)
            }
            _ => None,
        }
    }

    pub fn encode_payload(self, output: &mut [u8]) -> Result<usize, PayloadCodecError> {
        self.validate()?;
        encode_json_payload(&self, output)
    }

    pub fn decode_payload(bytes: &[u8]) -> Result<Self, PayloadCodecError> {
        let wire: WirePrepareReady = crate::messages::decode_json_payload(bytes)?;
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
    PtyMaster,
    IdentityChannel,
    WorkerPidfd,
}

/// A complete Prepare request frame: its correlation ID, payload, and zero-FD contract.
/// `broker_id` comes from the completed Hello exchange and binds this request to that generation.
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
    pub fn encode_into(&self, output: &mut [u8]) -> Result<usize, PrepareFrameError> {
        encode_prepare_frame(
            PrepareRequest::KIND,
            self.request_id,
            0,
            &self.request,
            output,
        )
    }

    /// Decodes one request after the transport has associated its received FDs.
    pub fn decode(
        bytes: &'a [u8],
        received_descriptors: usize,
        broker_id: crate::BrokerId,
    ) -> Result<Self, PrepareFrameError> {
        let frame = ControlFrame::decode(bytes).map_err(PrepareFrameError::Frame)?;
        Self::decode_parts(
            frame.header(),
            frame.payload(),
            received_descriptors,
            broker_id,
        )
    }

    /// Checks the envelope before allocating payload storage. The completed
    /// payload must still pass `decode_parts`.
    pub fn validate_header(
        header: ControlFrameHeader,
        received_descriptors: usize,
    ) -> Result<(), PrepareFrameError> {
        validate_prepare_header(header, received_descriptors, PrepareRequest::KIND, 0)
    }

    /// Decodes a request whose header and payload have separate storage.
    pub fn decode_parts(
        header: ControlFrameHeader,
        payload: &'a [u8],
        received_descriptors: usize,
        broker_id: crate::BrokerId,
    ) -> Result<Self, PrepareFrameError> {
        validate_prepare_parts(
            header,
            payload,
            received_descriptors,
            PrepareRequest::KIND,
            0,
        )?;
        let request =
            PrepareRequest::decode_payload(payload).map_err(PrepareFrameError::Payload)?;
        Ok(Self::new(header.request_id(), broker_id, request))
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

    /// Decodes the response to this request, enforcing correlation and its FD contract.
    pub fn decode_ready(
        &self,
        bytes: &[u8],
        received_descriptors: usize,
    ) -> Result<PrepareReadyFrame, PrepareFrameError> {
        PrepareReadyFrame::decode_for(self, bytes, received_descriptors)
    }

    /// Validates a response already split into header, payload and owned FDs
    /// by the control transport, without rebuilding its serialized header.
    pub fn decode_ready_parts(
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
    pub fn encode_into(self, output: &mut [u8]) -> Result<usize, PrepareFrameError> {
        encode_prepare_frame(
            PrepareReady::KIND,
            self.request_id,
            self.descriptor_count(),
            &self.response,
            output,
        )
    }

    fn decode_for(
        request: &PrepareRequestFrame<'_>,
        bytes: &[u8],
        received_descriptors: usize,
    ) -> Result<Self, PrepareFrameError> {
        let frame = ControlFrame::decode(bytes).map_err(PrepareFrameError::Frame)?;
        request.decode_ready_parts(frame.header(), frame.payload(), received_descriptors)
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
    let header = ControlFrameHeader::new(kind, request_id, payload_bytes as u32, descriptor_count)
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

pub(crate) fn validate_absolute_path(value: &str) -> Result<(), PayloadCodecError> {
    if !value.starts_with('/')
        || value.is_empty()
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|component| matches!(component, "." | ".."))
    {
        return Err(PayloadCodecError::InvalidRequest);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    fn interactive_request() -> PrepareRequest<'static> {
        PrepareRequest::new(
            "alice",
            ExecutionKind::Interactive,
            GuestProcess::Command(
                CommandArgv::new(&["/usr/bin/editor", "file with spaces", "", "line\nbreak"])
                    .unwrap(),
            ),
            Some(TerminalEnvironment::new("xterm-256color", Some("truecolor"), None).unwrap()),
            Some(TerminalSize::new(120, 40).unwrap()),
            ProjectionRequests::new(
                Some(WaylandProjectionRequest::new("/mnt/wayland/socket").unwrap()),
                Some(
                    X11ProjectionRequest::new(3, "/mnt/host-x11/X3", "/tmp/.X11-unix/X3").unwrap(),
                ),
            ),
        )
        .unwrap()
    }

    #[test]
    fn prepare_payload_round_trips_command_boundaries_and_both_displays() {
        let request = interactive_request();
        let mut payload = std::vec![0; crate::MAX_CONTROL_PAYLOAD_BYTES as usize];

        let length = request.encode_payload(&mut payload).unwrap();
        let decoded = PrepareRequest::decode_payload(&payload[..length]).unwrap();

        assert_eq!(decoded.user(), request.user());
        assert_eq!(decoded.execution(), request.execution());
        let GuestProcess::Command(argv) = decoded.process() else {
            panic!("expected an explicit command");
        };
        let mut argv_bytes = std::vec![0; MAX_COMMAND_TOTAL_BYTES + 1];
        let argv_length = argv.write_nul_terminated_into(&mut argv_bytes).unwrap();
        assert_eq!(
            &argv_bytes[..argv_length],
            b"/usr/bin/editor\0file with spaces\0\0line\nbreak\0"
        );
        assert_eq!(
            decoded.projections().wayland().unwrap().target(),
            "/mnt/wayland/socket"
        );
        assert_eq!(decoded.projections().x11().unwrap().display(), 3);
    }

    #[test]
    fn bounded_request_has_no_terminal_or_display_context() {
        let request = PrepareRequest::new(
            "root",
            ExecutionKind::Bounded,
            GuestProcess::Command(CommandArgv::new(&["/sbin/poweroff"]).unwrap()),
            None,
            None,
            ProjectionRequests::default(),
        )
        .unwrap();
        let mut payload = [0; 512];

        let length = request.encode_payload(&mut payload).unwrap();
        let decoded = PrepareRequest::decode_payload(&payload[..length]).unwrap();
        assert_eq!(decoded.user(), "root");
        assert!(matches!(decoded.process(), GuestProcess::Command(_)));
    }

    #[test]
    fn interactive_prepare_frame_round_trips_request_response_and_descriptor_order() {
        let broker_id = crate::BrokerId::new([0xa5; 16]);
        let request = PrepareRequestFrame::new(41, broker_id, interactive_request());
        let mut request_bytes = [0; 4096];

        let request_length = request.encode_into(&mut request_bytes).unwrap();
        let decoded_request =
            PrepareRequestFrame::decode(&request_bytes[..request_length], 0, broker_id).unwrap();
        let request_header = ControlFrame::decode(&request_bytes[..request_length])
            .unwrap()
            .header();
        assert_eq!(request_header.kind(), MessageKind::PrepareRequest);
        assert_eq!(request_header.request_id(), 41);
        assert_eq!(request_header.descriptor_count(), 0);
        assert_eq!(decoded_request.request().user(), "alice");

        let ready = PrepareReady::new(
            PreparationId::new([0x5a; PREPARATION_ID_BYTES]),
            broker_id,
            30_000,
        )
        .unwrap();
        let ready_frame = decoded_request.ready_frame(ready).unwrap();
        let challenge = ready_frame.identity_challenge([0x7c; crate::IDENTITY_CHALLENGE_BYTES]);
        assert_eq!(challenge.preparation_id(), ready.preparation_id());
        assert_eq!(challenge.nonce(), &[0x7c; crate::IDENTITY_CHALLENGE_BYTES]);
        assert_eq!(ready_frame.descriptor_count(), 3);
        assert_eq!(
            ready_frame.descriptor_role(0),
            Some(PrepareReadyDescriptorRole::PtyMaster)
        );
        assert_eq!(
            ready_frame.descriptor_role(1),
            Some(PrepareReadyDescriptorRole::IdentityChannel)
        );
        assert_eq!(
            ready_frame.descriptor_role(2),
            Some(PrepareReadyDescriptorRole::WorkerPidfd)
        );
        assert_eq!(ready_frame.descriptor_role(3), None);

        let mut ready_bytes = [0; 256];
        let ready_length = ready_frame.encode_into(&mut ready_bytes).unwrap();
        let ready_header = ControlFrame::decode(&ready_bytes[..ready_length])
            .unwrap()
            .header();
        assert_eq!(ready_header.kind(), MessageKind::PrepareReady);
        assert_eq!(ready_header.request_id(), 41);
        assert_eq!(ready_header.descriptor_count(), 3);

        let decoded_ready = decoded_request
            .decode_ready(&ready_bytes[..ready_length], 3)
            .unwrap();
        assert_eq!(decoded_ready.request_id(), 41);
        assert_eq!(decoded_ready.response(), ready);
        assert_eq!(decoded_ready.response().broker_id(), broker_id);
    }

    #[test]
    fn bounded_prepare_frame_carries_identity_channel_and_assigned_worker() {
        let request = PrepareRequest::new(
            "root",
            ExecutionKind::Bounded,
            GuestProcess::Command(CommandArgv::new(&["/sbin/poweroff"]).unwrap()),
            None,
            None,
            ProjectionRequests::default(),
        )
        .unwrap();
        let broker_id = crate::BrokerId::new([9; 16]);
        let request = PrepareRequestFrame::new(92, broker_id, request);
        let mut request_bytes = [0; 1024];
        let request_length = request.encode_into(&mut request_bytes).unwrap();
        let decoded_request =
            PrepareRequestFrame::decode(&request_bytes[..request_length], 0, broker_id).unwrap();

        let ready = PrepareReady::new(
            PreparationId::new([7; PREPARATION_ID_BYTES]),
            broker_id,
            MAX_PREPARED_LEASE_MS,
        )
        .unwrap();
        let ready_frame = decoded_request.ready_frame(ready).unwrap();
        assert_eq!(ready_frame.descriptor_count(), 2);
        assert_eq!(
            ready_frame.descriptor_role(0),
            Some(PrepareReadyDescriptorRole::IdentityChannel)
        );
        assert_eq!(
            ready_frame.descriptor_role(1),
            Some(PrepareReadyDescriptorRole::WorkerPidfd)
        );
        assert_eq!(ready_frame.descriptor_role(2), None);

        let mut ready_bytes = [0; 256];
        let ready_length = ready_frame.encode_into(&mut ready_bytes).unwrap();
        let decoded_ready = decoded_request
            .decode_ready(&ready_bytes[..ready_length], 2)
            .unwrap();
        assert_eq!(decoded_ready.response(), ready);
    }

    #[test]
    fn prepare_ready_frame_rejects_wrong_request_id_and_descriptor_contract() {
        let broker_id = crate::BrokerId::new([4; 16]);
        let request = PrepareRequestFrame::new(101, broker_id, interactive_request());
        let response = PrepareReady::new(
            PreparationId::new([3; PREPARATION_ID_BYTES]),
            broker_id,
            1_000,
        )
        .unwrap();
        let mut bytes = [0; 256];

        let other_request = PrepareRequestFrame::new(102, broker_id, interactive_request());
        let other_length = other_request
            .ready_frame(response)
            .unwrap()
            .encode_into(&mut bytes)
            .unwrap();
        assert_eq!(
            request.decode_ready(&bytes[..other_length], 3),
            Err(PrepareFrameError::MismatchedRequestId {
                expected: 101,
                actual: 102,
            })
        );

        let valid_length = request
            .ready_frame(response)
            .unwrap()
            .encode_into(&mut bytes)
            .unwrap();
        assert_eq!(
            request.decode_ready(&bytes[..valid_length], 1),
            Err(PrepareFrameError::Frame(
                ControlFrameError::DescriptorCountMismatch {
                    declared: 3,
                    actual: 1,
                }
            ))
        );

        let frame = ControlFrame::decode(&bytes[..valid_length]).unwrap();
        let wrong_header = ControlFrameHeader::new(
            MessageKind::PrepareReady,
            request.request_id(),
            frame.payload().len() as u32,
            1,
        )
        .unwrap();
        let mut wrong_contract = [0; 256];
        let wrong_length = wrong_header
            .encode_frame(frame.payload(), &mut wrong_contract)
            .unwrap();
        assert_eq!(
            request.decode_ready(&wrong_contract[..wrong_length], 1),
            Err(PrepareFrameError::UnexpectedDescriptorCount {
                expected: 3,
                actual: 1,
            })
        );
    }

    #[test]
    fn prepare_ready_frame_rejects_a_different_broker_generation() {
        let expected_broker = crate::BrokerId::new([1; 16]);
        let other_broker = crate::BrokerId::new([2; 16]);
        let request = PrepareRequestFrame::new(27, expected_broker, interactive_request());
        let response = PrepareReady::new(
            PreparationId::new([3; PREPARATION_ID_BYTES]),
            other_broker,
            1_000,
        )
        .unwrap();

        assert_eq!(
            request.ready_frame(response),
            Err(PrepareFrameError::MismatchedBrokerId {
                expected: expected_broker,
                actual: other_broker,
            })
        );

        let mut bytes = [0; 256];
        let length = encode_prepare_frame(
            PrepareReady::KIND,
            request.request_id(),
            PrepareReady::descriptor_count_for(ExecutionKind::Interactive),
            &response,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(
            request.decode_ready(&bytes[..length], 3),
            Err(PrepareFrameError::MismatchedBrokerId {
                expected: expected_broker,
                actual: other_broker,
            })
        );
    }

    #[test]
    fn prepare_request_frame_rejects_wrong_kind_and_any_descriptor() {
        let broker_id = crate::BrokerId::new([8; 16]);
        let request = PrepareRequestFrame::new(15, broker_id, interactive_request());
        let mut bytes = [0; 4096];
        let length = request.encode_into(&mut bytes).unwrap();
        let frame = ControlFrame::decode(&bytes[..length]).unwrap();

        let wrong_kind = ControlFrameHeader::new(
            MessageKind::PrepareReady,
            request.request_id(),
            frame.payload().len() as u32,
            0,
        )
        .unwrap();
        let mut wrong_kind_bytes = [0; 4096];
        let wrong_kind_length = wrong_kind
            .encode_frame(frame.payload(), &mut wrong_kind_bytes)
            .unwrap();
        assert!(matches!(
            PrepareRequestFrame::decode(&wrong_kind_bytes[..wrong_kind_length], 0, broker_id),
            Err(PrepareFrameError::UnexpectedMessageKind {
                expected: MessageKind::PrepareRequest,
                actual: MessageKind::PrepareReady,
            })
        ));

        let declared_descriptor = ControlFrameHeader::new(
            MessageKind::PrepareRequest,
            request.request_id(),
            frame.payload().len() as u32,
            1,
        )
        .unwrap();
        let mut declared_descriptor_bytes = [0; 4096];
        let declared_length = declared_descriptor
            .encode_frame(frame.payload(), &mut declared_descriptor_bytes)
            .unwrap();
        assert!(matches!(
            PrepareRequestFrame::decode(
                &declared_descriptor_bytes[..declared_length],
                1,
                broker_id
            ),
            Err(PrepareFrameError::UnexpectedDescriptorCount {
                expected: 0,
                actual: 1,
            })
        ));

        assert!(matches!(
            PrepareRequestFrame::decode(&bytes[..length], 1, broker_id),
            Err(PrepareFrameError::Frame(
                ControlFrameError::DescriptorCountMismatch {
                    declared: 0,
                    actual: 1,
                }
            ))
        ));
    }

    #[test]
    fn prepare_ready_round_trips_ids_lease_and_fd_contract() {
        let ready = PrepareReady::new(
            PreparationId::new([0x5a; PREPARATION_ID_BYTES]),
            crate::BrokerId::new([0xa5; 16]),
            MAX_PREPARED_LEASE_MS,
        )
        .unwrap();
        let mut payload = [0; 256];

        let length = ready.encode_payload(&mut payload).unwrap();
        let decoded = PrepareReady::decode_payload(&payload[..length]).unwrap();

        assert_eq!(decoded, ready);
        assert_eq!(decoded.preparation_id().as_bytes(), &[0x5a; 16]);
        assert_eq!(decoded.broker_id().as_bytes(), &[0xa5; 16]);
        assert_eq!(decoded.remaining_lease_ms(), MAX_PREPARED_LEASE_MS);
        assert_eq!(
            PrepareReady::descriptor_count_for(ExecutionKind::Interactive),
            3
        );
        assert_eq!(
            PrepareReady::descriptor_count_for(ExecutionKind::Bounded),
            2
        );
    }

    #[test]
    fn prepare_ready_rejects_zero_and_overlong_leases() {
        assert!(matches!(
            PrepareReady::new(
                PreparationId::new([1; PREPARATION_ID_BYTES]),
                crate::BrokerId::new([2; 16]),
                0,
            ),
            Err(PayloadCodecError::InvalidRequest)
        ));
        assert!(matches!(
            PrepareReady::new(
                PreparationId::new([1; PREPARATION_ID_BYTES]),
                crate::BrokerId::new([2; 16]),
                MAX_PREPARED_LEASE_MS + 1,
            ),
            Err(PayloadCodecError::InvalidRequest)
        ));

        let zeros = std::iter::repeat_n("0", PREPARATION_ID_BYTES)
            .collect::<std::vec::Vec<_>>()
            .join(",");
        let overlong_lease = std::format!(
            r#"{{"preparation_id":[{zeros}],"broker_id":[{zeros}],"remaining_lease_ms":{}}}"#,
            MAX_PREPARED_LEASE_MS + 1
        );
        assert!(matches!(
            PrepareReady::decode_payload(overlong_lease.as_bytes()),
            Err(PayloadCodecError::InvalidRequest)
        ));
    }

    #[test]
    fn prepare_decode_rejects_zero_terminal_dimensions() {
        let valid = serde_json_core::from_slice::<TerminalSize>(br#"{"cols":80,"rows":24}"#)
            .unwrap()
            .0;
        assert_eq!(valid, TerminalSize::new(80, 24).unwrap());
        assert!(serde_json_core::from_slice::<TerminalSize>(br#"{"cols":0,"rows":24}"#).is_err());
        assert!(serde_json_core::from_slice::<TerminalSize>(br#"{"cols":80,"rows":0}"#).is_err());

        let invalid_size = br#"{"user":"alice","execution":"interactive","process":"login_shell","terminal":{"term":"xterm-256color"},"initial_size":{"cols":0,"rows":24}}"#;
        assert!(matches!(
            PrepareRequest::decode_payload(invalid_size),
            Err(PayloadCodecError::InvalidRequest)
        ));
    }

    #[test]
    fn payload_decoding_distinguishes_semantic_and_json_errors() {
        let invalid_command =
            br#"{"user":"alice","execution":"bounded","process":{"command":["relative"]}}"#;
        assert!(matches!(
            PrepareRequest::decode_payload(invalid_command),
            Err(PayloadCodecError::InvalidRequest)
        ));

        let oversized_argument = std::format!(
            r#"{{"user":"alice","execution":"bounded","process":{{"command":["/bin/tool","{}"]}}}}"#,
            "x".repeat(MAX_COMMAND_ARGUMENT_BYTES + 1)
        );
        assert!(matches!(
            PrepareRequest::decode_payload(oversized_argument.as_bytes()),
            Err(PayloadCodecError::InvalidRequest)
        ));

        let nul_argument = br#"{"user":"alice","execution":"bounded","process":{"command":["/bin/tool","bad\u0000arg"]}}"#;
        assert!(matches!(
            PrepareRequest::decode_payload(nul_argument),
            Err(PayloadCodecError::InvalidRequest)
        ));

        let too_many_arguments = std::format!(
            r#"{{"user":"alice","execution":"bounded","process":{{"command":["/bin/tool",{}]}}}}"#,
            std::iter::repeat_n(r#""x""#, MAX_COMMAND_ARGUMENTS + 1)
                .collect::<std::vec::Vec<_>>()
                .join(",")
        );
        assert!(matches!(
            PrepareRequest::decode_payload(too_many_arguments.as_bytes()),
            Err(PayloadCodecError::InvalidRequest)
        ));

        assert!(matches!(
            PrepareRequest::decode_payload(br#"{"user":"alice","execution":"bounded","process":}"#),
            Err(PayloadCodecError::InvalidJson)
        ));
        assert!(matches!(
            PrepareRequest::decode_payload(br#"{"user":"alice","execution":"bounded","process":{"command":["/bin/tool","bad\q"]}}"#),
            Err(PayloadCodecError::InvalidJson)
        ));

        let invalid_control =
            "{\"user\":\"alice\",\"execution\":\"bounded\",\"process\":{\"command\":[\"/bin/tool\",\"bad\u{1}\"]}}";
        assert!(matches!(
            PrepareRequest::decode_payload(invalid_control.as_bytes()),
            Err(PayloadCodecError::InvalidJson)
        ));
    }

    #[test]
    fn command_argv_preserves_the_64k_boundary_without_losing_empty_arguments() {
        let mut arguments = std::vec!["x".repeat(MAX_COMMAND_ARGUMENT_BYTES); 15];
        arguments.push("x".repeat(MAX_COMMAND_ARGUMENT_BYTES - 18));
        let mut borrowed = std::vec!["/x"];
        borrowed.extend(arguments.iter().map(std::string::String::as_str));

        let encoded = CommandArgv::new(&borrowed).unwrap();
        assert_eq!(encoded.values.len(), 17);
        assert_eq!(encoded.argument_count(), 16);
        assert_eq!(encoded.total_bytes, MAX_COMMAND_TOTAL_BYTES);
        let mut output = std::vec![0; MAX_COMMAND_TOTAL_BYTES + 1];
        assert_eq!(
            encoded.write_nul_terminated_into(&mut output).unwrap(),
            MAX_COMMAND_TOTAL_BYTES + 1
        );

        let too_large = std::vec!["x".repeat(MAX_COMMAND_ARGUMENT_BYTES); 16];
        let mut borrowed = std::vec!["/x"];
        borrowed.extend(too_large.iter().map(std::string::String::as_str));
        assert!(matches!(
            CommandArgv::new(&borrowed),
            Err(PayloadCodecError::InvalidRequest)
        ));
    }

    #[test]
    fn request_validation_rejects_invalid_names_paths_modes_and_dimensions() {
        let terminal = TerminalEnvironment::new("xterm", None, None).unwrap();
        assert!(matches!(
            PrepareRequest::new(
                "../root",
                ExecutionKind::Interactive,
                GuestProcess::LoginShell,
                Some(terminal.clone()),
                Some(TerminalSize::new(80, 24).unwrap()),
                ProjectionRequests::default(),
            ),
            Err(PayloadCodecError::InvalidRequest)
        ));
        assert_eq!(
            WaylandProjectionRequest::new("/run/../tmp/socket"),
            Err(PayloadCodecError::InvalidRequest)
        );
        assert_eq!(
            TerminalSize::new(0, 24),
            Err(PayloadCodecError::InvalidRequest)
        );
        assert!(matches!(
            PrepareRequest::new(
                "root",
                ExecutionKind::Bounded,
                GuestProcess::LoginShell,
                None,
                None,
                ProjectionRequests::default(),
            ),
            Err(PayloadCodecError::InvalidRequest)
        ));
    }

    #[test]
    fn command_rejects_relative_program_nul_and_too_many_arguments() {
        assert!(matches!(
            CommandArgv::new(&["relative"]),
            Err(PayloadCodecError::InvalidRequest)
        ));
        assert!(matches!(
            CommandArgv::new(&["/bin/echo", "bad\0arg"]),
            Err(PayloadCodecError::InvalidRequest)
        ));
        let arguments = std::vec!["arg"; MAX_COMMAND_ARGUMENTS + 1];
        let mut argv = std::vec!["/bin/echo"];
        argv.extend(arguments);
        assert!(matches!(
            CommandArgv::new(&argv),
            Err(PayloadCodecError::InvalidRequest)
        ));
    }
}

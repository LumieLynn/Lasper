use super::*;
use serde::de::{DeserializeSeed, EnumAccess, MapAccess, VariantAccess};

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum Field {
    User,
    Execution,
    Process,
    Environment,
    InitialSize,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum ProcessKind {
    LoginShell,
    Command,
}

pub(super) struct WirePrepareRequest<'a> {
    user: Option<EscapedStr<'a>>,
    execution: Option<ExecutionKind>,
    process: Option<ProcessKind>,
    argv: CommandArgv<'a>,
    environment: ProcessEnvironment<'a>,
    initial_size: Option<WireTerminalSize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTerminalSize {
    cols: u16,
    rows: u16,
}

#[derive(Default)]
pub(super) struct DecoderWorkspace {
    pub(super) fields: u8,
    pub(super) valid: bool,
    // Preserve a field rejection when Serde unwinds the enclosing visitor.
    pub(super) invalid_request: bool,
}

impl<'a> WirePrepareRequest<'a> {
    pub(super) const fn new() -> Self {
        Self {
            user: None,
            execution: None,
            process: None,
            argv: CommandArgv::empty(),
            environment: ProcessEnvironment::empty(),
            initial_size: None,
        }
    }

    pub(super) fn decode(
        &mut self,
        workspace: &mut DecoderWorkspace,
        bytes: &'a [u8],
        scratch: &mut [u8],
        capacities: &crate::ProfileCapacities,
    ) -> Result<(), PayloadCodecError> {
        self.user = None;
        self.execution = None;
        self.process = None;
        self.argv.clear();
        self.environment.clear();
        self.initial_size = None;
        workspace.fields = 0;
        workspace.valid = false;
        workspace.invalid_request = false;
        crate::json::validate_json_strings(bytes)?;
        let mut decoder = serde_json_core::de::Deserializer::new(bytes, Some(scratch));
        RequestSeed {
            wire: self,
            workspace,
            capacities,
        }
        .deserialize(&mut decoder)
        .map_err(|_| {
            if workspace.invalid_request {
                PayloadCodecError::InvalidRequest
            } else {
                PayloadCodecError::InvalidJson
            }
        })?;
        decoder.end().map_err(|_| PayloadCodecError::InvalidJson)?;
        let fields: &[&str] = if matches!(self.process, Some(ProcessKind::LoginShell)) {
            &["execution", "process"]
        } else {
            &["execution"]
        };
        crate::json::require_top_level_strings(bytes, fields)?;
        Ok(())
    }

    pub(super) fn finish(
        &mut self,
        workspace: &DecoderWorkspace,
        output: &mut PrepareRequest<'a>,
        capacities: &crate::ProfileCapacities,
    ) -> Result<(), PayloadCodecError> {
        if workspace.fields & 0b1111 != 0b1111 {
            return Err(PayloadCodecError::InvalidJson);
        }
        output.user.clear();
        for fragment in fragments(self.user.ok_or(PayloadCodecError::InvalidJson)?) {
            match fragment.map_err(|_| PayloadCodecError::InvalidRequest)? {
                EscapedStringFragment::NotEscaped(text) => output.user.push_str(text),
                EscapedStringFragment::Escaped(ch) => output.user.push(ch),
            }
            .map_err(|_| PayloadCodecError::InvalidRequest)?;
        }
        output.execution = self.execution.ok_or(PayloadCodecError::InvalidJson)?;
        output.initial_size = self
            .initial_size
            .as_ref()
            .map(|size| TerminalSize::new(size.cols, size.rows))
            .transpose()?;
        match self.process.ok_or(PayloadCodecError::InvalidJson)? {
            ProcessKind::LoginShell => output.process = GuestProcess::LoginShell,
            ProcessKind::Command => {
                if !matches!(output.process, GuestProcess::Command(_)) {
                    output.process = GuestProcess::Command(CommandArgv::empty());
                }
                let GuestProcess::Command(argv) = &mut output.process else {
                    unreachable!()
                };
                argv.clear();
                argv.total_bytes = self.argv.total_bytes;
                argv.invalid_wire_value = self.argv.invalid_wire_value;
                // Move views in place to avoid a fixed-capacity argv temporary
                // on the guest's stack. Popping reverses their order.
                while let Some(value) = self.argv.values.pop() {
                    argv.values
                        .push(value)
                        .map_err(|_| PayloadCodecError::InvalidRequest)?;
                }
                argv.values.reverse();
            }
        }
        self.environment.move_into(&mut output.environment);
        output.validate_with(capacities)
    }
}

struct RequestSeed<'s, 'a> {
    wire: &'s mut WirePrepareRequest<'a>,
    workspace: &'s mut DecoderWorkspace,
    capacities: &'s crate::ProfileCapacities,
}

impl<'de: 'a, 'a> DeserializeSeed<'de> for RequestSeed<'_, 'a> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        decoder.deserialize_map(self)
    }
}

impl<'de: 'a, 'a> Visitor<'de> for RequestSeed<'_, 'a> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Prepare request")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(field) = map.next_key::<Field>()? {
            let bit = match field {
                Field::User => 1,
                Field::Execution => 2,
                Field::Process => 4,
                Field::Environment => 8,
                Field::InitialSize => 16,
            };
            if self.workspace.fields & bit != 0 {
                return Err(A::Error::custom("duplicate Prepare field"));
            }
            self.workspace.fields |= bit;
            match field {
                Field::User => self.wire.user = Some(map.next_value()?),
                Field::Execution => self.wire.execution = Some(map.next_value()?),
                Field::Process => map.next_value_seed(ProcessSeed(self.wire))?,
                Field::Environment => map.next_value_seed(environment::EnvironmentSeed {
                    output: &mut self.wire.environment,
                    capacities: self.capacities,
                    invalid_request: &mut self.workspace.invalid_request,
                })?,
                Field::InitialSize => self.wire.initial_size = Some(map.next_value()?),
            }
        }
        Ok(())
    }
}

struct ProcessSeed<'s, 'a>(&'s mut WirePrepareRequest<'a>);

impl<'de: 'a, 'a> DeserializeSeed<'de> for ProcessSeed<'_, 'a> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        decoder.deserialize_enum("process", &["login_shell", "command"], self)
    }
}

impl<'de: 'a, 'a> Visitor<'de> for ProcessSeed<'_, 'a> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("login_shell or a command")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, access: A) -> Result<(), A::Error> {
        let (kind, variant) = access.variant::<ProcessKind>()?;
        self.0.process = Some(kind);
        match kind {
            ProcessKind::LoginShell => variant.unit_variant(),
            ProcessKind::Command => variant.newtype_variant_seed(ArgvSeed(&mut self.0.argv)),
        }
    }
}

struct ArgvSeed<'s, 'a>(&'s mut CommandArgv<'a>);

impl<'de: 'a, 'a> DeserializeSeed<'de> for ArgvSeed<'_, 'a> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        decoder.deserialize_seq(self)
    }
}

impl<'de: 'a, 'a> Visitor<'de> for ArgvSeed<'_, 'a> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded argv array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        while let Some(value) = sequence.next_element::<EscapedStr<'de>>()? {
            self.0.push_wire_value(CommandArgument::Escaped(value));
        }
        Ok(())
    }
}

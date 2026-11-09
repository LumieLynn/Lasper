use super::*;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
enum WireGuestProcess<'a> {
    LoginShell,
    Command(#[serde(borrow)] WireCommandArgv<'a>),
}

// Keep strings borrowed during serde's nested visitors. Inline path/environment
// arrays there otherwise multiply the stack cost before validation even starts.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "'de: 'a"))]
pub(super) struct WirePrepareRequest<'a> {
    #[serde(borrow)]
    user: EscapedStr<'a>,
    execution: ExecutionKind,
    #[serde(borrow)]
    process: WireGuestProcess<'a>,
    #[serde(borrow)]
    terminal: Option<WireTerminalEnvironment<'a>>,
    initial_size: Option<WireTerminalSize>,
    #[serde(default, borrow)]
    projections: WireProjections<'a>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTerminalSize {
    cols: u16,
    rows: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTerminalEnvironment<'a> {
    #[serde(borrow)]
    term: EscapedStr<'a>,
    #[serde(borrow)]
    colorterm: Option<EscapedStr<'a>>,
    #[serde(borrow)]
    no_color: Option<EscapedStr<'a>>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProjections<'a> {
    #[serde(borrow)]
    wayland: Option<WireWayland<'a>>,
    #[serde(borrow)]
    x11: Option<WireX11<'a>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireWayland<'a> {
    #[serde(borrow)]
    target: EscapedStr<'a>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireX11<'a> {
    display: u16,
    #[serde(borrow)]
    mount_target: EscapedStr<'a>,
    #[serde(borrow)]
    client_path: EscapedStr<'a>,
}

impl<'a> WirePrepareRequest<'a> {
    pub(super) fn into_request(self) -> Result<PrepareRequest<'a>, PayloadCodecError> {
        let process = match self.process {
            WireGuestProcess::LoginShell => GuestProcess::LoginShell,
            WireGuestProcess::Command(argv) => GuestProcess::Command(argv.0),
        };
        let initial_size = self
            .initial_size
            .map(|size| TerminalSize::new(size.cols, size.rows))
            .transpose()?;
        let terminal = self
            .terminal
            .map(|terminal| -> Result<_, PayloadCodecError> {
                Ok(TerminalEnvironment {
                    term: unescape(terminal.term)?,
                    colorterm: terminal.colorterm.map(unescape).transpose()?,
                    no_color: terminal.no_color.map(unescape).transpose()?,
                })
            })
            .transpose()?;
        let wayland = self
            .projections
            .wayland
            .map(|wire| -> Result<_, PayloadCodecError> {
                Ok(WaylandProjectionRequest {
                    target: unescape(wire.target)?,
                })
            })
            .transpose()?;
        let x11 = self
            .projections
            .x11
            .map(|wire| -> Result<_, PayloadCodecError> {
                Ok(X11ProjectionRequest {
                    display: wire.display,
                    mount_target: unescape(wire.mount_target)?,
                    client_path: unescape(wire.client_path)?,
                })
            })
            .transpose()?;
        let request = PrepareRequest {
            user: unescape(self.user)?,
            execution: self.execution,
            process,
            terminal,
            initial_size,
            projections: ProjectionRequests::new(wayland, x11),
        };
        request.validate()?;
        Ok(request)
    }
}

fn unescape<const N: usize>(value: EscapedStr<'_>) -> Result<String<N>, PayloadCodecError> {
    let mut string = String::new();
    for fragment in value.fragments() {
        match fragment.map_err(|_| PayloadCodecError::InvalidRequest)? {
            EscapedStringFragment::NotEscaped(text) => string.push_str(text),
            EscapedStringFragment::Escaped(character) => string.push(character),
        }
        .map_err(|_| PayloadCodecError::InvalidRequest)?;
    }
    Ok(string)
}

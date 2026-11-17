//! Converts owned process settings into the selected profile's request fields.

use super::{error::invalid, RequestError};
use lasper_guest_protocol::{
    CommandArgv, ExecutionKind, GuestProcess, PrepareRequest, ProcessEnvironment,
    ProfileCapacities, TerminalSize,
};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub user: String,
    pub program: Program,
    pub stdio: Stdio,
    pub environment: BTreeMap<String, String>,
}
#[derive(Clone, Debug)]
pub enum Program {
    LoginShell,
    Argv(Vec<String>),
}
#[derive(Clone, Debug)]
pub enum Stdio {
    Terminal { size: TerminalSize },
    Piped,
}
impl ProcessSpec {
    pub(super) fn wire(&self) -> Result<PrepareRequest<'_>, RequestError> {
        let process = match &self.program {
            Program::LoginShell => GuestProcess::LoginShell,
            Program::Argv(args) => {
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                GuestProcess::Command(
                    CommandArgv::new(&argv).map_err(|e| invalid("program.argv", e))?,
                )
            }
        };
        let environment = ProcessEnvironment::new(
            self.environment
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .map_err(|e| invalid("environment", e))?;
        let (execution, size) = match self.stdio {
            Stdio::Terminal { size } => (ExecutionKind::Terminal, Some(size)),
            Stdio::Piped => (ExecutionKind::Piped, None),
        };
        PrepareRequest::new(&self.user, execution, process, environment, size)
            .map_err(|e| invalid("process", e))
    }
    pub(super) fn validate_profile(
        &self,
        request: &PrepareRequest<'_>,
        caps: &ProfileCapacities,
    ) -> Result<(), RequestError> {
        request
            .environment()
            .validate(caps)
            .map_err(|e| invalid("environment", e))?;
        if self.user.len() > caps.user_name_bytes {
            return Err(invalid("user", "exceeds selected profile capacity"));
        }
        if let Program::Argv(args) = &self.program {
            if args.len().saturating_sub(1) > caps.command_arguments
                || args.iter().any(|a| a.len() > caps.command_argument_bytes)
                || args.iter().map(String::len).sum::<usize>() + args.len().saturating_sub(1)
                    > caps.command_total_bytes
            {
                return Err(invalid("program.argv", "exceeds selected profile capacity"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/client/profile.rs"]
mod tests;

use core::ffi::CStr;
use lasper_guest_protocol::{
    CommandArgv, GuestProcess, PrepareRequest, StartRequest, MAX_COMMAND_ARGUMENTS,
};

use super::account::Account;
use super::linux;
use super::{WorkerFailure, WorkerStage};

const MAX_ENVIRONMENT_ENTRIES: usize = 10;
const E2BIG: i32 = 7;
const EINVAL: i32 = 22;
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub(super) fn execute(
    account: &Account,
    request: &PrepareRequest<'_>,
    start: &StartRequest,
    argv_storage: &mut [u8],
    environment_storage: &mut [u8],
) -> WorkerFailure {
    let mut environment = Environment::new(environment_storage);
    if let Err(errno) = environment.for_account(account, request, start) {
        return WorkerFailure::new(WorkerStage::Environment, errno);
    }
    let arguments = match request.process() {
        GuestProcess::LoginShell => Arguments::login(account, argv_storage),
        GuestProcess::Command(command) => Arguments::command(command, argv_storage),
    };
    let arguments = match arguments {
        Ok(arguments) => arguments,
        Err(errno) => return WorkerFailure::new(WorkerStage::Exec, errno),
    };
    let variables = environment.pointers();
    WorkerFailure::new(WorkerStage::Exec, exec(&arguments, &variables))
}

struct Arguments<'a> {
    program: &'a CStr,
    values: [*const u8; MAX_COMMAND_ARGUMENTS + 2],
}

impl<'a> Arguments<'a> {
    fn command(command: &CommandArgv<'_>, storage: &'a mut [u8]) -> Result<Self, i32> {
        let length = command
            .write_nul_terminated_into(storage)
            .map_err(|_| E2BIG)?;
        let mut values = [core::ptr::null(); MAX_COMMAND_ARGUMENTS + 2];
        for (target, value) in values
            .iter_mut()
            .zip(storage[..length].split_inclusive(|byte| *byte == 0))
        {
            *target = value.as_ptr();
        }
        let end = storage[..length]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(EINVAL)?;
        let program = CStr::from_bytes_with_nul(&storage[..=end]).map_err(|_| EINVAL)?;
        Ok(Self { program, values })
    }

    fn login(account: &'a Account, storage: &'a mut [u8]) -> Result<Self, i32> {
        let basename = account.shell.as_str().rsplit('/').next().ok_or(EINVAL)?;
        if basename.is_empty() || storage.len() < basename.len() + 2 {
            return Err(EINVAL);
        }
        storage[0] = b'-';
        storage[1..=basename.len()].copy_from_slice(basename.as_bytes());
        storage[basename.len() + 1] = 0;
        let mut values = [core::ptr::null(); MAX_COMMAND_ARGUMENTS + 2];
        values[0] = storage.as_ptr();
        Ok(Self {
            program: account.shell.as_c_str(),
            values,
        })
    }
}

fn exec(arguments: &Arguments<'_>, environment: &[*const u8]) -> i32 {
    // SAFETY: Both pointer arrays end in null. Their strings remain in the
    // caller's account and buffers until execve replaces the worker or fails.
    unsafe { linux::execve(arguments.program, &arguments.values, environment) }
}

struct Environment<'a> {
    storage: &'a mut [u8],
    length: usize,
    offsets: [usize; MAX_ENVIRONMENT_ENTRIES],
    count: usize,
}

impl<'a> Environment<'a> {
    fn new(storage: &'a mut [u8]) -> Self {
        Self {
            storage,
            length: 0,
            offsets: [0; MAX_ENVIRONMENT_ENTRIES],
            count: 0,
        }
    }

    fn for_account(
        &mut self,
        account: &Account,
        request: &PrepareRequest<'_>,
        start: &StartRequest,
    ) -> Result<(), i32> {
        self.push("HOME", account.home.as_str())?;
        self.push("USER", account.username.as_str())?;
        self.push("LOGNAME", account.username.as_str())?;
        self.push("SHELL", account.shell.as_str())?;
        self.push("PATH", GUEST_PATH)?;
        if let Some(terminal) = request.terminal() {
            self.push("TERM", terminal.term())?;
            if let Some(value) = terminal.colorterm() {
                self.push("COLORTERM", value)?;
            }
            if let Some(value) = terminal.no_color() {
                self.push("NO_COLOR", value)?;
            }
        }
        if let Some(value) = start.wayland_display() {
            self.push("WAYLAND_DISPLAY", value)?;
        }
        if let Some(number) = start.x11_display() {
            let mut display = [0; 6];
            let mut offset = display.len();
            let mut number = number;
            loop {
                offset -= 1;
                display[offset] = b'0' + (number % 10) as u8;
                number /= 10;
                if number == 0 {
                    break;
                }
            }
            offset -= 1;
            display[offset] = b':';
            self.push("DISPLAY", core::str::from_utf8(&display[offset..]).unwrap())?;
        }
        Ok(())
    }

    fn push(&mut self, name: &str, value: &str) -> Result<(), i32> {
        let end = self
            .length
            .checked_add(name.len())
            .and_then(|length| length.checked_add(value.len() + 2))
            .ok_or(E2BIG)?;
        if self.count == self.offsets.len() || end > self.storage.len() {
            return Err(E2BIG);
        }
        if value.as_bytes().contains(&0) {
            return Err(EINVAL);
        }
        let slot = &mut self.storage[self.length..end];
        let last = slot.len() - 1;
        slot[..name.len()].copy_from_slice(name.as_bytes());
        slot[name.len()] = b'=';
        slot[name.len() + 1..last].copy_from_slice(value.as_bytes());
        *slot.last_mut().unwrap() = 0;
        self.offsets[self.count] = self.length;
        self.count += 1;
        self.length = end;
        Ok(())
    }

    fn pointers(&self) -> [*const u8; MAX_ENVIRONMENT_ENTRIES + 1] {
        let mut pointers = [core::ptr::null(); MAX_ENVIRONMENT_ENTRIES + 1];
        for (target, offset) in pointers.iter_mut().zip(&self.offsets[..self.count]) {
            *target = self.storage[*offset..].as_ptr();
        }
        pointers
    }
}

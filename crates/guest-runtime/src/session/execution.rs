use crate::linux::uapi::errno::{E2BIG, EINVAL};
use core::ffi::CStr;
use lasper_guest_protocol::{
    CommandArgv, GuestProcess, PrepareRequest, ProfileCapacities, MAX_COMMAND_ARGUMENTS,
};

use super::account::Account;
use super::{WorkerFailure, WorkerStage};
use crate::linux;

const MAX_ENVIRONMENT_ENTRIES: usize = 6 + lasper_guest_protocol::MAX_PROCESS_ENVIRONMENT_ENTRIES;

struct EnvironmentLayout {
    scratch_bytes: usize,
    string_bytes: usize,
    pointer_offset: usize,
    pointer_slots: usize,
    mapping_bytes: usize,
}

impl EnvironmentLayout {
    const fn new(capacities: &ProfileCapacities) -> Self {
        let scratch_bytes = capacities.prepare_unescape_bytes();
        let string_bytes = capacities.merged_environment_bytes();
        let pointer_offset =
            (scratch_bytes + string_bytes).next_multiple_of(core::mem::align_of::<*const u8>());
        let pointer_slots = 6 + capacities.environment_entries + 1;
        // Keep the strings and null-terminated envp in one allocation whose
        // granularity works on 4, 16 and 64 KiB page hosts.
        let mapping_bytes = (pointer_offset + pointer_slots * core::mem::size_of::<*const u8>())
            .next_multiple_of(64 * 1024);
        Self {
            scratch_bytes,
            string_bytes,
            pointer_offset,
            pointer_slots,
            mapping_bytes,
        }
    }
}

const _: () = {
    let layout = EnvironmentLayout::new(&lasper_guest_protocol::V1_0_CAPACITIES);
    assert!(layout.mapping_bytes == 64 * 1024);
    assert!(layout.pointer_slots == 39);
};

pub(super) struct ExecutionMemory {
    mapping: crate::memory::Mapping,
    capacities: &'static ProfileCapacities,
}

impl ExecutionMemory {
    pub(super) fn new(capacities: &'static ProfileCapacities) -> Result<Self, WorkerFailure> {
        let layout = EnvironmentLayout::new(capacities);
        let mapping = crate::memory::Mapping::new(layout.mapping_bytes)
            .map_err(|errno| WorkerFailure::new(WorkerStage::Memory, errno))?;
        Ok(Self {
            mapping,
            capacities,
        })
    }

    pub(super) fn prepare<'a>(
        &'a mut self,
        account: &'a Account,
        request: &PrepareRequest<'_>,
        argv_storage: &'a mut [u8],
    ) -> Result<Execution<'a>, WorkerFailure> {
        // Decoding the next entry must not overwrite strings already referenced by envp.
        let layout = EnvironmentLayout::new(self.capacities);
        let (strings, pointers) = self.mapping.as_mut().split_at_mut(layout.pointer_offset);
        let (decode_scratch, strings) = strings.split_at_mut(layout.scratch_bytes);
        // SAFETY: mmap is page-aligned and pointer_offset preserves pointer
        // alignment. This disjoint tail has room for every slot. All slots are
        // initialized before they are read and remain borrowed from the mapping.
        let pointers = unsafe {
            let pointer = pointers.as_mut_ptr().cast::<*const u8>();
            for index in 0..layout.pointer_slots {
                pointer.add(index).write(core::ptr::null());
            }
            core::slice::from_raw_parts_mut(pointer, layout.pointer_slots)
        };
        let mut environment = Environment::new(&mut strings[..layout.string_bytes], pointers);
        environment
            .for_account(account, request, self.capacities, decode_scratch)
            .map_err(|errno| WorkerFailure::new(WorkerStage::Environment, errno))?;
        let arguments = match request.process() {
            GuestProcess::LoginShell => Arguments::login(account, argv_storage),
            GuestProcess::Command(command) => Arguments::command(command, argv_storage),
        }
        .map_err(|errno| WorkerFailure::new(WorkerStage::Exec, errno))?;
        Ok(Execution {
            arguments,
            environment,
        })
    }
}

// Both borrowed buffers stay alive while the worker answers identity challenges.
// Start uses these argv/environment buffers without decoding or allocating them again.
pub(super) struct Execution<'a> {
    arguments: Arguments<'a>,
    environment: Environment<'a>,
}

impl Execution<'_> {
    pub(super) fn execute(mut self) -> WorkerFailure {
        WorkerFailure::new(
            WorkerStage::Exec,
            exec(&self.arguments, self.environment.pointers()),
        )
    }
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
    pointers: &'a mut [*const u8],
    length: usize,
    offsets: [usize; MAX_ENVIRONMENT_ENTRIES],
    count: usize,
}

impl<'a> Environment<'a> {
    fn new(storage: &'a mut [u8], pointers: &'a mut [*const u8]) -> Self {
        pointers.fill(core::ptr::null());
        Self {
            storage,
            pointers,
            length: 0,
            offsets: [0; MAX_ENVIRONMENT_ENTRIES],
            count: 0,
        }
    }

    fn for_account(
        &mut self,
        account: &Account,
        request: &PrepareRequest<'_>,
        capacities: &ProfileCapacities,
        decode_scratch: &mut [u8],
    ) -> Result<(), i32> {
        self.push("HOME", account.home.as_str())?;
        self.push("USER", account.username.as_str())?;
        self.push("LOGNAME", account.username.as_str())?;
        self.push("SHELL", account.shell.as_str())?;
        self.push("PATH", crate::paths::EXEC_PATH)?;
        if request.execution() == lasper_guest_protocol::ExecutionKind::Terminal {
            self.push("TERM", "dumb")?;
        }
        if self.length > capacities.default_environment_bytes {
            return Err(E2BIG);
        }
        for entry in request.environment().entries() {
            let (name, value) = entry.decode_into(decode_scratch).map_err(|_| EINVAL)?;
            self.push(name, value)?;
        }
        Ok(())
    }

    fn push(&mut self, name: &str, value: &str) -> Result<(), i32> {
        let end = self
            .length
            .checked_add(name.len())
            .and_then(|length| length.checked_add(value.len() + 2))
            .ok_or(E2BIG)?;
        let existing = self.offsets[..self.count].iter().position(|offset| {
            let bytes = &self.storage[*offset..];
            bytes.starts_with(name.as_bytes()) && bytes.get(name.len()) == Some(&b'=')
        });
        if (existing.is_none()
            && (self.count == self.offsets.len() || self.count + 1 >= self.pointers.len()))
            || end > self.storage.len()
        {
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
        if let Some(index) = existing {
            // Leave the old string in place and replace its offset. The mapping
            // budgets for defaults plus additions, even when names overlap.
            self.offsets[index] = self.length;
        } else {
            self.offsets[self.count] = self.length;
            self.count += 1;
        }
        self.length = end;
        Ok(())
    }

    fn pointers(&mut self) -> &[*const u8] {
        for (target, offset) in self.pointers.iter_mut().zip(&self.offsets[..self.count]) {
            *target = self.storage[*offset..].as_ptr();
        }
        &self.pointers[..=self.count]
    }
}

#[cfg(test)]
#[path = "../../tests/session/execution.rs"]
mod tests;

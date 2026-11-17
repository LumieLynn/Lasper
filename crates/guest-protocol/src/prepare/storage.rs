use super::{
    wire::{DecoderWorkspace, WirePrepareRequest},
    *,
};
use core::mem::{align_of, offset_of, size_of};

// BUILD_CAPACITIES sizes the view arrays, but does not resize these slots.
// A larger metadata type requires revising the slots and layout assertions.
const REQUEST_SLOT_BYTES: usize = 16 * 1024;
const WORKSPACE_BYTES: usize = 8 * 1024;

#[repr(C, align(16))]
struct WireSlot<'a> {
    value: WirePrepareRequest<'a>,
    padding: [u8; REQUEST_SLOT_BYTES - size_of::<WirePrepareRequest<'static>>()],
}

#[repr(C, align(16))]
struct RequestSlot<'a> {
    value: PrepareRequestFrame<'a>,
    padding: [u8; REQUEST_SLOT_BYTES - size_of::<PrepareRequestFrame<'static>>()],
}

#[repr(C, align(16))]
struct WorkspaceSlot {
    value: DecoderWorkspace,
    padding: [u8; WORKSPACE_BYTES - size_of::<DecoderWorkspace>()],
}

/// Keeps Prepare's large view arrays in caller-owned storage so the guest can
/// decode without stack copies. Strings borrow the payload; scratch is supplied
/// separately and can be reused after decoding. The fixed metadata layout uses
/// 48 KiB, independent of the selected profile's scratch budget.
#[repr(C, align(16))]
pub struct PrepareDecodeStorage<'a> {
    wire: WireSlot<'a>,
    output: RequestSlot<'a>,
    workspace: WorkspaceSlot,
    reserve: [u8; 8 * 1024],
}

impl<'a> PrepareDecodeStorage<'a> {
    #[cfg(test)]
    pub(crate) fn install_fixture(
        &mut self,
        frame: PrepareRequestFrame<'a>,
    ) -> &PrepareRequestFrame<'a> {
        self.output.value = frame;
        self.workspace.value.valid = true;
        &self.output.value
    }
    pub const fn new() -> Self {
        Self {
            wire: WireSlot {
                value: WirePrepareRequest::new(),
                padding: [0; REQUEST_SLOT_BYTES - size_of::<WirePrepareRequest<'static>>()],
            },
            output: RequestSlot {
                value: PrepareRequestFrame {
                    request_id: 0,
                    broker_id: crate::BrokerId::new([0; 16]),
                    request: PrepareRequest {
                        user: String::new(),
                        execution: ExecutionKind::Piped,
                        process: GuestProcess::Command(CommandArgv::empty()),
                        environment: ProcessEnvironment::empty(),
                        initial_size: None,
                    },
                },
                padding: [0; REQUEST_SLOT_BYTES - size_of::<PrepareRequestFrame<'static>>()],
            },
            workspace: WorkspaceSlot {
                value: DecoderWorkspace {
                    fields: 0,
                    valid: false,
                    invalid_request: false,
                },
                padding: [0; WORKSPACE_BYTES - size_of::<DecoderWorkspace>()],
            },
            reserve: [0; 8 * 1024],
        }
    }

    /// Returns the last successfully decoded frame. Failed decoding invalidates it.
    pub fn request(&self) -> Option<&PrepareRequestFrame<'a>> {
        self.workspace.value.valid.then_some(&self.output.value)
    }

    pub(crate) fn invalidate(&mut self) {
        self.workspace.value.valid = false;
    }

    pub(crate) fn into_request(self) -> PrepareRequest<'a> {
        self.output.value.request
    }

    pub(crate) fn into_frame(self) -> PrepareRequestFrame<'a> {
        self.output.value
    }

    pub(crate) fn decode(
        &mut self,
        payload: &'a [u8],
        scratch: &mut [u8],
        capacities: &crate::ProfileCapacities,
    ) -> Result<(), PayloadCodecError> {
        if scratch.len() < capacities.prepare_unescape_bytes() {
            return Err(PayloadCodecError::OutputTooSmall);
        }
        if payload.len() > crate::MAX_CONTROL_PAYLOAD_BYTES as usize {
            return Err(PayloadCodecError::PayloadTooLarge);
        }
        self.wire
            .value
            .decode(&mut self.workspace.value, payload, scratch, capacities)?;
        self.wire.value.finish(
            &self.workspace.value,
            &mut self.output.value.request,
            capacities,
        )
    }

    pub(crate) fn decode_frame(
        &mut self,
        header: ControlFrameHeader,
        payload: &'a [u8],
        descriptors: usize,
        broker: crate::BrokerId,
        scratch: &mut [u8],
        capacities: &crate::ProfileCapacities,
    ) -> Result<&PrepareRequestFrame<'a>, PrepareFrameError> {
        self.workspace.value.valid = false;
        super::validate_prepare_parts(header, payload, descriptors, PrepareRequest::KIND, 0)?;
        self.decode(payload, scratch, capacities)
            .map_err(PrepareFrameError::Payload)?;
        self.output.value.request_id = header.request_id();
        self.output.value.broker_id = broker;
        self.workspace.value.valid = true;
        Ok(&self.output.value)
    }
}

impl Default for PrepareDecodeStorage<'_> {
    fn default() -> Self {
        Self::new()
    }
}

const _: () = {
    assert!(size_of::<CommandArgument<'static>>() <= 32);
    assert!(align_of::<CommandArgument<'static>>() <= 16);
    assert!(
        size_of::<WirePrepareRequest<'static>>() <= 11296,
        "Prepare wire metadata exceeds the 1.0 estimate; revise storage.rs bounds, slots and layout tests"
    );
    assert!(
        size_of::<PrepareRequestFrame<'static>>() <= 11296,
        "Prepare request metadata exceeds the 1.0 estimate; revise storage.rs bounds, slots and layout tests"
    );
    assert!(align_of::<WirePrepareRequest<'static>>() <= 16);
    assert!(align_of::<PrepareRequestFrame<'static>>() <= 16);
    assert!(size_of::<DecoderWorkspace>() <= WORKSPACE_BYTES);
    assert!(align_of::<DecoderWorkspace>() <= 16);
    assert!(offset_of!(PrepareDecodeStorage<'static>, wire) == 0);
    assert!(offset_of!(PrepareDecodeStorage<'static>, output) == REQUEST_SLOT_BYTES);
    assert!(offset_of!(PrepareDecodeStorage<'static>, workspace) == 2 * REQUEST_SLOT_BYTES);
    assert!(
        offset_of!(PrepareDecodeStorage<'static>, reserve)
            == 2 * REQUEST_SLOT_BYTES + WORKSPACE_BYTES
    );
    assert!(size_of::<PrepareDecodeStorage<'static>>() == 48 * 1024);
};

#[cfg(test)]
#[path = "../../tests/prepare/storage.rs"]
mod tests;

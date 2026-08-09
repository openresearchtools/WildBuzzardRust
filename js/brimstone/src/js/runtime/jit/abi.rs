//! Stable C-layout boundary between Rust and generated code.
//!
//! Generated code only receives raw boxed bits and C-layout pointers. It must never construct a
//! `Value` from zero, retain a moving heap pointer outside the shadow-frame slots, call a Rust-ABI
//! function, or unwind across this boundary.

use std::{mem::size_of, ptr};

use crate::runtime::bytecode::verifier::VerifiedBytecode;

pub(crate) const GENERATED_CODE_ABI_VERSION: u32 = 1;

pub(crate) const STATUS_RETURNED: u32 = 0;
pub(crate) const STATUS_SIDE_EXIT: u32 = 1;
pub(crate) const STATUS_INVALID_ACTIVATION: u32 = 2;
pub(crate) const STATUS_INTERRUPTED: u32 = 3;

pub(crate) const NO_SAFEPOINT: u32 = u32::MAX;

/// Native entry ABI. Generated code must not unwind and must return one of the `STATUS_*` values.
///
/// This raw type is not an embedding entry point. A null pointer may be used to exercise the
/// generated header-rejection path, but any non-null pointer must identify a live, correctly
/// aligned `JitActivation` allocation. Generated code must read that header before it can reject
/// malformed fields, so an arbitrary non-null address is outside the contract. Normal calls go
/// through the lifetime-branded `ActivationOwner` accepted by `ExecutableMemory::call`.
pub(crate) type GeneratedEntry = unsafe extern "C" fn(*mut JitActivation) -> u32;

/// Future non-allocating helper ABI. Allocation-capable helpers are intentionally absent from the
/// first gate, and the prototype compiler emits no helper calls at all.
pub(crate) type NonAllocatingHelper =
    unsafe extern "C" fn(*mut JitActivation, u64, u64) -> JitHelperResult;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct JitHelperResult {
    pub(crate) status: u32,
    pub(crate) reserved: u32,
    /// Raw non-zero NaN-boxed bits. Zero is never a valid Brimstone `Value` representation.
    pub(crate) value_bits: u64,
}

/// Versioned helper table. The activation owner leaves this null and the current compiler never
/// reads it, so no helper (allocating or otherwise) is active in this slice.
#[repr(C)]
pub(crate) struct JitHelperTable {
    abi_version: u32,
    struct_size: u32,
    reserved: u64,
    non_allocating_helper: Option<NonAllocatingHelper>,
}

/// C-layout shadow-frame schema for future GC integration. This schema is **not yet linked into
/// Brimstone's root walker** and therefore must not be described or treated as GC-visible. The
/// current compiler has no helper calls or native safepoints.
#[repr(C)]
pub(crate) struct JitShadowFrame {
    previous: *mut JitShadowFrame,
    slots: *mut u64,
    slot_count: usize,
    bytecode_offset: u32,
    safepoint_index: u32,
}

/// Lifetime-branded owner for a shadow-frame schema. The slot slice cannot safely die while this
/// owner remains usable.
pub(crate) struct ShadowFrameOwner<'slots> {
    raw: JitShadowFrame,
    slots: &'slots mut [u64],
}

impl<'slots> ShadowFrameOwner<'slots> {
    /// Create a frame over already initialized raw value slots.
    ///
    /// Zero is not a Brimstone `Value` representation. Rejecting it here means generated code can
    /// load any verifier-bounded slot without ever constructing or propagating an invalid value.
    pub(crate) fn new(slots: &'slots mut [u64]) -> Result<Self, ShadowFrameError> {
        if let Some(index) = slots.iter().position(|&bits| bits == 0) {
            return Err(ShadowFrameError::ZeroSlot(index));
        }

        let expected_slots = slots.as_mut_ptr();
        let expected_slot_count = slots.len();
        Ok(Self {
            raw: JitShadowFrame {
                previous: ptr::null_mut(),
                slots: expected_slots,
                slot_count: expected_slot_count,
                bytecode_offset: 0,
                safepoint_index: NO_SAFEPOINT,
            },
            slots,
        })
    }

    fn as_mut_ptr(&mut self) -> *mut JitShadowFrame {
        &mut self.raw
    }

    fn as_ptr(&self) -> *mut JitShadowFrame {
        ptr::from_ref(&self.raw).cast_mut()
    }

    fn validate_schema(&self) -> Result<(), ActivationResultError> {
        let expected_slots = self.slots.as_ptr().cast_mut();
        let expected_slot_count = self.slots.len();
        if !self.raw.previous.is_null()
            || self.raw.slots != expected_slots
            || self.raw.slot_count != expected_slot_count
            || self.raw.bytecode_offset != 0
            || self.raw.safepoint_index != NO_SAFEPOINT
        {
            return Err(ActivationResultError::ShadowFrameChanged);
        }
        if let Some(index) = self.slots.iter().position(|&bits| bits == 0) {
            return Err(ActivationResultError::ZeroShadowSlot(index));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShadowFrameError {
    ZeroSlot(usize),
}

/// Per-entry C-layout activation schema shared by Rust and generated code. Fields stay private so
/// safe Rust cannot fabricate raw pointers or bypass the lifetime-branded owners.
#[repr(C)]
pub(crate) struct JitActivation {
    abi_version: u32,
    struct_size: u32,
    frame: *mut JitShadowFrame,
    helpers: *const JitHelperTable,
    side_exit_offset: u32,
    reserved: u32,
    return_value_bits: u64,
}

#[cfg(test)]
impl JitActivation {
    /// Build a real, live activation header whose version is invalid and whose frame address must
    /// therefore never be dereferenced by generated code. This is intentionally unavailable to
    /// non-test code; normal callers must use `ActivationOwner`.
    pub(crate) fn invalid_header_with_dangling_frame_for_test() -> Self {
        Self {
            abi_version: GENERATED_CODE_ABI_VERSION.wrapping_add(1),
            struct_size: size_of::<Self>() as u32,
            frame: ptr::NonNull::<JitShadowFrame>::dangling().as_ptr(),
            helpers: ptr::null(),
            side_exit_offset: 0,
            reserved: 0,
            return_value_bits: 0,
        }
    }
}

/// Lifetime-branded activation owner. It keeps the frame borrow alive throughout a generated call
/// and validates only its owned C-layout storage; no safe method dereferences a raw pointer.
pub(crate) struct ActivationOwner<'frame, 'slots> {
    raw: JitActivation,
    frame: &'frame mut ShadowFrameOwner<'slots>,
}

impl<'frame, 'slots> ActivationOwner<'frame, 'slots> {
    pub(crate) fn new(frame: &'frame mut ShadowFrameOwner<'slots>) -> Self {
        let frame_ptr = frame.as_mut_ptr();
        Self {
            raw: JitActivation {
                abi_version: GENERATED_CODE_ABI_VERSION,
                struct_size: size_of::<JitActivation>() as u32,
                frame: frame_ptr,
                helpers: ptr::null(),
                side_exit_offset: 0,
                reserved: 0,
                return_value_bits: 0,
            },
            frame,
        }
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut JitActivation {
        &mut self.raw
    }

    pub(crate) fn validate_header(&self) -> Result<(), ActivationResultError> {
        if self.raw.abi_version != GENERATED_CODE_ABI_VERSION
            || self.raw.struct_size as usize != size_of::<JitActivation>()
            || self.raw.frame != self.frame.as_ptr()
            || !self.raw.helpers.is_null()
        {
            return Err(ActivationResultError::InvalidHeader);
        }
        if self.raw.reserved != 0 {
            return Err(ActivationResultError::ReservedFieldChanged);
        }
        self.frame.validate_schema()
    }

    /// Validate generated outputs without ever constructing a `Value` from raw bits.
    pub(crate) fn validate_result(
        &self,
        status: u32,
        bytecode: &VerifiedBytecode<'_>,
    ) -> Result<ActivationOutcome, ActivationResultError> {
        self.validate_header()?;
        match status {
            STATUS_RETURNED => {
                if self.raw.return_value_bits == 0 {
                    return Err(ActivationResultError::ZeroReturnValue);
                }
                if self.raw.side_exit_offset != 0 {
                    return Err(ActivationResultError::UnexpectedSideExitOffset);
                }
                Ok(ActivationOutcome::Returned(self.raw.return_value_bits))
            }
            STATUS_SIDE_EXIT => {
                if self.raw.return_value_bits != 0 {
                    return Err(ActivationResultError::UnexpectedReturnValue);
                }
                let offset = self.raw.side_exit_offset as usize;
                if !bytecode.is_instruction_start(offset) {
                    return Err(ActivationResultError::InvalidSideExitOffset(offset));
                }
                Ok(ActivationOutcome::SideExit(offset))
            }
            STATUS_INVALID_ACTIVATION => {
                if self.raw.return_value_bits != 0 {
                    return Err(ActivationResultError::UnexpectedReturnValue);
                }
                if self.raw.side_exit_offset != 0 {
                    return Err(ActivationResultError::UnexpectedSideExitOffset);
                }
                Ok(ActivationOutcome::InvalidActivation)
            }
            STATUS_INTERRUPTED => {
                if self.raw.return_value_bits != 0 {
                    return Err(ActivationResultError::UnexpectedReturnValue);
                }
                let offset = self.raw.side_exit_offset as usize;
                if !bytecode.is_instruction_start(offset) {
                    return Err(ActivationResultError::InvalidSideExitOffset(offset));
                }
                Ok(ActivationOutcome::Interrupted)
            }
            other => Err(ActivationResultError::UnknownStatus(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActivationOutcome {
    Returned(u64),
    /// A verified bytecode boundary recorded in the ABI schema. No interpreter-resume path is
    /// connected to this outcome in the contained prototype.
    SideExit(usize),
    InvalidActivation,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActivationResultError {
    InvalidHeader,
    ShadowFrameChanged,
    ZeroShadowSlot(usize),
    ReservedFieldChanged,
    UnknownStatus(u32),
    ZeroReturnValue,
    UnexpectedReturnValue,
    UnexpectedSideExitOffset,
    InvalidSideExitOffset(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct SafepointRecord {
    pub(crate) native_offset: u32,
    pub(crate) bytecode_offset: u32,
    pub(crate) first_live_slot: u32,
    pub(crate) live_slot_count: u32,
    pub(crate) flags: u32,
}

pub(crate) const ACTIVATION_FRAME_OFFSET: i32 = std::mem::offset_of!(JitActivation, frame) as i32;
pub(crate) const ACTIVATION_ABI_VERSION_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, abi_version) as i32;
pub(crate) const ACTIVATION_STRUCT_SIZE_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, struct_size) as i32;
pub(crate) const ACTIVATION_HELPERS_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, helpers) as i32;
pub(crate) const ACTIVATION_SIDE_EXIT_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, side_exit_offset) as i32;
pub(crate) const ACTIVATION_RESERVED_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, reserved) as i32;
pub(crate) const ACTIVATION_RETURN_VALUE_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, return_value_bits) as i32;
pub(crate) const SHADOW_FRAME_PREVIOUS_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, previous) as i32;
pub(crate) const SHADOW_FRAME_SLOTS_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, slots) as i32;
pub(crate) const SHADOW_FRAME_SLOT_COUNT_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, slot_count) as i32;
pub(crate) const SHADOW_FRAME_BYTECODE_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, bytecode_offset) as i32;
pub(crate) const SHADOW_FRAME_SAFEPOINT_INDEX_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, safepoint_index) as i32;
pub(crate) const JIT_ACTIVATION_SIZE: u32 = size_of::<JitActivation>() as u32;

const _: () = {
    assert!(size_of::<JitHelperResult>() == 16);
    assert!(size_of::<JitShadowFrame>() == 32);
    assert!(size_of::<JitActivation>() == 40);
    assert!(size_of::<SafepointRecord>() == 20);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::bytecode::{
        instruction::OpCode,
        verifier::{VerificationLimits, VerifiedBytecode},
    };

    fn verified_return() -> VerifiedBytecode<'static> {
        let bytes = Box::leak(Box::new([OpCode::Ret as u8, u8::MAX]));
        VerifiedBytecode::verify(bytes, VerificationLimits::empty(1, 0)).unwrap()
    }

    #[test]
    fn activation_is_lifetime_branded_versioned_and_non_dereferencing() {
        let mut slots = [1_u64, 2];
        let mut frame = ShadowFrameOwner::new(&mut slots).unwrap();
        let mut activation = ActivationOwner::new(&mut frame);
        assert_eq!(activation.validate_header(), Ok(()));
        assert!(!activation.as_mut_ptr().is_null());

        activation.raw.abi_version += 1;
        assert_eq!(activation.validate_header(), Err(ActivationResultError::InvalidHeader));
    }

    #[test]
    fn result_validation_rejects_zero_unknown_and_non_boundary_outputs() {
        let bytecode = verified_return();
        let mut slots = [1_u64];
        let mut frame = ShadowFrameOwner::new(&mut slots).unwrap();
        let mut activation = ActivationOwner::new(&mut frame);

        assert_eq!(
            activation.validate_result(STATUS_RETURNED, &bytecode),
            Err(ActivationResultError::ZeroReturnValue)
        );
        assert_eq!(
            activation.validate_result(99, &bytecode),
            Err(ActivationResultError::UnknownStatus(99))
        );

        activation.raw.side_exit_offset = 1;
        assert_eq!(
            activation.validate_result(STATUS_SIDE_EXIT, &bytecode),
            Err(ActivationResultError::InvalidSideExitOffset(1))
        );
        activation.raw.side_exit_offset = 0;
        assert_eq!(
            activation.validate_result(STATUS_SIDE_EXIT, &bytecode),
            Ok(ActivationOutcome::SideExit(0))
        );

        activation.raw.return_value_bits = 1;
        assert_eq!(
            activation.validate_result(STATUS_RETURNED, &bytecode),
            Ok(ActivationOutcome::Returned(1))
        );

        activation.raw.side_exit_offset = 1;
        assert_eq!(
            activation.validate_result(STATUS_RETURNED, &bytecode),
            Err(ActivationResultError::UnexpectedSideExitOffset)
        );
    }

    #[test]
    fn validation_rejects_rewritten_frame_identity_and_schema() {
        let mut slots = [1_u64];
        let mut frame = ShadowFrameOwner::new(&mut slots).unwrap();
        let mut activation = ActivationOwner::new(&mut frame);

        let expected = activation.raw.frame;
        activation.raw.frame = expected.wrapping_add(1);
        assert_eq!(activation.validate_header(), Err(ActivationResultError::InvalidHeader));

        activation.raw.frame = expected;
        activation.frame.raw.slot_count += 1;
        assert_eq!(activation.validate_header(), Err(ActivationResultError::ShadowFrameChanged));
    }

    #[test]
    fn abi_layout_is_fixed_for_generated_code() {
        assert_eq!(ACTIVATION_ABI_VERSION_OFFSET, 0);
        assert_eq!(ACTIVATION_STRUCT_SIZE_OFFSET, 4);
        assert_eq!(ACTIVATION_FRAME_OFFSET, 8);
        assert_eq!(ACTIVATION_HELPERS_OFFSET, 16);
        assert_eq!(ACTIVATION_SIDE_EXIT_OFFSET, 24);
        assert_eq!(ACTIVATION_RESERVED_OFFSET, 28);
        assert_eq!(ACTIVATION_RETURN_VALUE_OFFSET, 32);
        assert_eq!(SHADOW_FRAME_PREVIOUS_OFFSET, 0);
        assert_eq!(SHADOW_FRAME_SLOTS_OFFSET, 8);
        assert_eq!(SHADOW_FRAME_SLOT_COUNT_OFFSET, 16);
        assert_eq!(SHADOW_FRAME_BYTECODE_OFFSET, 24);
        assert_eq!(SHADOW_FRAME_SAFEPOINT_INDEX_OFFSET, 28);
        assert_eq!(JIT_ACTIVATION_SIZE, 40);
    }

    #[test]
    fn shadow_frame_rejects_zero_value_slots() {
        let mut slots = [1_u64, 0, 2];
        assert!(matches!(ShadowFrameOwner::new(&mut slots), Err(ShadowFrameError::ZeroSlot(1))));
    }
}

//! Stable C-layout boundary between Rust, generated code, and the moving collector.
//!
//! The only allocation-capable generated call in this gate is the audited zero-capacity
//! `NewObject` helper below. Native backedges consume their ordinary deterministic budget inline;
//! a second nonallocating helper is entered only for an interrupt, quantum boundary, hard native
//! residency boundary, or invalid policy state. Generated code receives C-layout pointers and
//! boxed bits, never a Rust reference. Every live boxed value is spilled to a compiler-derived
//! stack-map slot before an allocating call, and the context-owned frame chain lets Brimstone's
//! existing `HeapVisitor` update those slots in place. No panic or Rust unwind may cross the
//! generated-code boundary.

#[cfg(test)]
use std::num::NonZeroU32;
use std::{
    marker::PhantomData,
    mem::{align_of, size_of},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    rc::Rc,
    thread::{self, ThreadId},
};

#[cfg(test)]
use crate::runtime::object_value::ObjectValue;
use crate::runtime::{
    Context, JitContextScope, Value,
    gc::{AnyHeapItem, Handle, HandleScopeGuard, Heap, HeapItem, HeapPtr, HeapVisitor},
    jit::hotness::{DeterministicInterruptBudget, InlinePollState},
    ordinary_object::ordinary_object_create,
};

pub(crate) const GENERATED_CODE_ABI_VERSION: u32 = 4;

pub(crate) const STATUS_RETURNED: u32 = 0;
pub(crate) const STATUS_SIDE_EXIT: u32 = 1;
pub(crate) const STATUS_INVALID_ACTIVATION: u32 = 2;
pub(crate) const STATUS_INTERRUPTED: u32 = 3;
pub(crate) const STATUS_ALLOCATION_FAILED: u32 = 4;
pub(crate) const STATUS_POISONED: u32 = 5;

const HELPER_STATUS_OK: u32 = 0;
const HELPER_STATUS_INVALID_ACTIVATION: u32 = 1;
const HELPER_STATUS_INTERRUPTED: u32 = 2;
const HELPER_STATUS_ALLOCATION_FAILED: u32 = 3;
const HELPER_STATUS_POISONED: u32 = 4;
const HELPER_STATUS_SIDE_EXIT: u32 = 5;

pub(crate) const NO_SAFEPOINT: u32 = u32::MAX;
pub(crate) const NO_BYTECODE_OFFSET: u32 = u32::MAX;

pub(crate) const SAFEPOINT_FLAG_ALLOCATING_HELPER: u32 = 1;
pub(crate) const MAX_SAFEPOINT_RECORDS: usize = 4_096;
pub(crate) const MAX_LIVE_ROOT_ENTRIES: usize = 1 << 20;
pub(crate) const MAX_STACK_MAP_BYTES: usize = 8 * 1024 * 1024;
const MAX_NATIVE_ACTIVATION_DEPTH: usize = 64;

/// Hard upper bound on taken native backedges in one activation.
///
/// The ordinary deterministic interrupt budget is still polled on every edge. This independent
/// cap bounds native residency even when an embedding supplies an unusually large quantum. The
/// edge which consumes the final unit is polled first, then side-exits at its already-published
/// target without replaying any bytecode effect.
pub(crate) const MAX_NATIVE_BACKEDGE_WORK_UNITS: u32 = 1_000_000;

/// One initialized native-frame slot with the exact in-memory representation of `Value`.
///
/// The field is private, there is no raw-word constructor/setter, and activation linking validates
/// every slot against the exact context. Generated code may mutate this storage only through its
/// audited valid encodings; helper and collector writes remain private trusted paths.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub(in crate::runtime::jit) struct JitSlot(Value);

impl PartialEq for JitSlot {
    fn eq(&self, other: &Self) -> bool {
        self.value().as_raw_bits() == other.value().as_raw_bits()
    }
}

impl Eq for JitSlot {}

impl std::fmt::Debug for JitSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("JitSlot")
            .field(&self.value().as_raw_bits())
            .finish()
    }
}

const _: () = assert!(size_of::<JitSlot>() == size_of::<Value>());
const _: () = assert!(align_of::<JitSlot>() == align_of::<Value>());
const _: () = assert!(size_of::<JitSlot>() == size_of::<u64>());
const _: () = assert!(align_of::<JitSlot>() == align_of::<u64>());

impl JitSlot {
    pub(in crate::runtime::jit) const fn undefined() -> Self {
        Self(Value::undefined())
    }

    pub(in crate::runtime::jit) const fn null() -> Self {
        Self(Value::null())
    }

    /// Construct from a `Value` only after checking its exact representation and heap identity.
    pub(in crate::runtime::jit) fn try_from_value(
        context: &JitContextScope<'_>,
        value: Value,
    ) -> Result<Self, JitSlotValidationError> {
        let slot = Self(value);
        validate_jit_slots(context.raw(), std::slice::from_ref(&slot))?;
        Ok(slot)
    }

    pub(in crate::runtime::jit) const fn value(&self) -> Value {
        self.0
    }

    /// Store a value produced by an audited, non-allocating contained continuation operation.
    ///
    /// # Safety
    ///
    /// `value` must have a canonical immediate encoding or be a currently valid pointer belonging
    /// to this activation's context. No allocation or collection may have invalidated it.
    pub(in crate::runtime::jit) unsafe fn write_trusted_value(&mut self, value: Value) {
        self.0 = value;
    }

    fn write_gc_value(&mut self, value: Value) {
        self.0 = value;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JitSlotError {
    NonCanonicalImmediate,
    PointerIsNotAllocatedItemStart,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JitSlotValidationError {
    AllocationFailed,
    InvalidSlot { index: usize, error: JitSlotError },
}

/// Opaque roots for the exact full native slot snapshot captured before activation unlink.
///
/// Construction validates every slot while the native shadow frame is still registered. Once
/// built, GC may rewrite the handle cells, and copying those trusted values back to the caller's
/// slots is allocation-free and cannot expose a partially refreshed stale-pointer suffix.
pub(in crate::runtime::jit) struct RootedSlotSet<'scope> {
    roots: Vec<Handle<Value>>,
    _brand: PhantomData<&'scope mut &'scope ()>,
}

impl RootedSlotSet<'_> {
    pub(in crate::runtime::jit) fn handles(&self) -> &[Handle<Value>] {
        &self.roots
    }

    pub(in crate::runtime::jit) fn sync_to_slots(
        &self,
        slots: &mut [JitSlot],
    ) -> Result<(), RootedSlotSyncError> {
        if slots.len() != self.roots.len() {
            slots.fill(JitSlot::undefined());
            return Err(RootedSlotSyncError::CountMismatch {
                actual: slots.len(),
                expected: self.roots.len(),
            });
        }

        for (slot, root) in slots.iter_mut().zip(&self.roots) {
            slot.write_gc_value(**root);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RootedSlotSyncError {
    CountMismatch { actual: usize, expected: usize },
}

fn validate_jit_slots(context: Context, slots: &[JitSlot]) -> Result<(), JitSlotValidationError> {
    let mut pointer_slots = Vec::new();
    pointer_slots
        .try_reserve_exact(slots.len())
        .map_err(|_| JitSlotValidationError::AllocationFailed)?;

    for (index, slot) in slots.iter().enumerate() {
        let value = slot.value();
        if value.is_pointer() {
            pointer_slots.push((value.as_raw_bits() as usize, index));
        } else if !is_canonical_immediate(value) {
            return Err(JitSlotValidationError::InvalidSlot {
                index,
                error: JitSlotError::NonCanonicalImmediate,
            });
        }
    }

    if pointer_slots.is_empty() {
        return Ok(());
    }

    pointer_slots.sort_unstable_by_key(|&(address, _)| address);
    let mut found = Vec::new();
    found
        .try_reserve_exact(pointer_slots.len())
        .map_err(|_| JitSlotValidationError::AllocationFailed)?;
    found.resize(pointer_slots.len(), false);

    let permanent = context.heap.permanent_heap_bounds();
    mark_allocated_item_starts(permanent.start, permanent.end, &pointer_slots, &mut found);
    let (current_start, current_end) = context.heap.current_used_heap_bounds();
    mark_allocated_item_starts(current_start, current_end, &pointer_slots, &mut found);

    if let Some((position, _)) = found.iter().enumerate().find(|(_, present)| !**present) {
        return Err(JitSlotValidationError::InvalidSlot {
            index: pointer_slots[position].1,
            error: JitSlotError::PointerIsNotAllocatedItemStart,
        });
    }
    Ok(())
}

fn is_canonical_immediate(value: Value) -> bool {
    let bits = value.as_raw_bits();
    if value.is_undefined() {
        return bits == Value::undefined().as_raw_bits();
    }
    if value.is_null() {
        return bits == Value::null().as_raw_bits();
    }
    if value.is_empty() {
        return bits == Value::empty().as_raw_bits();
    }
    if value.is_bool() {
        return bits == Value::bool(value.as_bool()).as_raw_bits();
    }
    if value.is_smi() {
        return bits == Value::raw_smi(value.as_smi()).as_raw_bits();
    }
    if value.is_double() {
        return !value.as_double().is_nan() || value.is_nan();
    }
    false
}

fn mark_allocated_item_starts(
    mut current: *const u8,
    end: *const u8,
    pointer_slots: &[(usize, usize)],
    found: &mut [bool],
) {
    while current < end {
        let address = current as usize;
        let mut position = pointer_slots.partition_point(|&(candidate, _)| candidate < address);
        while position < pointer_slots.len() && pointer_slots[position].0 == address {
            found[position] = true;
            position += 1;
        }

        // SAFETY: Context heap metadata defines this fully allocated contiguous range. Advancing
        // by each heap item's aligned allocation size is the same traversal used by collection and
        // serialization; only actual allocation starts are compared with candidate slot pointers.
        let item = HeapPtr::<AnyHeapItem>::from_ptr(current.cast_mut().cast());
        let byte_size = AnyHeapItem::byte_size(item);
        let allocation_size = Heap::alloc_size_for_request_size(byte_size);
        let Some(next_address) = (current as usize).checked_add(allocation_size) else {
            std::process::abort();
        };
        if allocation_size == 0 || next_address > end as usize {
            std::process::abort();
        }
        // SAFETY: The checked allocation size remains within the same live heap allocation.
        current = unsafe { current.add(allocation_size) };
    }
}

/// Native entry ABI. Generated code must not unwind and must return one of the `STATUS_*` values.
///
/// This raw type is not an embedding entry point. A null pointer may be used to exercise the
/// generated header-rejection path, but every non-null pointer must identify the live activation
/// owned by `ActivationOwner` for the full synchronous call.
pub(crate) type GeneratedEntry = unsafe extern "C" fn(*mut JitActivation) -> u32;

/// The only allocating helper signature admitted in this slice.
pub(crate) type AllocatingHelper = unsafe extern "C" fn(*mut JitActivation) -> u32;

/// Nonallocating taken-backedge slow poll. It may inspect only the activation/frame headers and
/// the lifetime-owned deterministic budget; it cannot allocate in or collect the JavaScript heap.
pub(crate) type BackedgePollHelper = unsafe extern "C" fn(*mut JitActivation) -> u32;

/// Versioned helper table. Safe code cannot substitute another function table.
#[repr(C)]
pub(crate) struct JitHelperTable {
    abi_version: u32,
    struct_size: u32,
    reserved: u64,
    new_object_zero: AllocatingHelper,
    backedge_poll: BackedgePollHelper,
}

static JIT_HELPERS: JitHelperTable = JitHelperTable {
    abi_version: GENERATED_CODE_ABI_VERSION,
    struct_size: size_of::<JitHelperTable>() as u32,
    reserved: 0,
    new_object_zero: new_object_zero_helper,
    backedge_poll: backedge_poll_helper,
};

/// One generated callsite and its exact live-root slice.
///
/// `native_return_offset` is Cranelift's checked call return PC relative to the RX entry. The live
/// range indexes immutable flattened `u32` slot indices. `result_slot` is deliberately not live at
/// the safepoint: the helper writes it only after all allocation/forced collection has completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct SafepointRecord {
    pub(crate) native_return_offset: u32,
    pub(crate) bytecode_offset: u32,
    pub(crate) live_slot_start: u32,
    pub(crate) live_slot_count: u32,
    pub(crate) result_slot: u32,
    pub(crate) flags: u32,
}

/// Immutable, fully checked metadata borrowed by a registered native frame.
#[derive(Debug)]
pub(crate) struct SafepointMetadata {
    frame_slot_count: usize,
    bytecode_len: u32,
    native_code_len: u32,
    records: Box<[SafepointRecord]>,
    live_slots: Box<[u32]>,
    instruction_starts: Box<[u32]>,
}

impl SafepointMetadata {
    pub(crate) fn new(
        frame_slot_count: usize,
        bytecode_len: usize,
        native_code_len: usize,
        records: Vec<SafepointRecord>,
        live_slots: Vec<u32>,
        instruction_starts: Vec<u32>,
    ) -> Result<Self, SafepointMetadataError> {
        if records.len() > MAX_SAFEPOINT_RECORDS {
            return Err(SafepointMetadataError::TooManySafepoints(records.len()));
        }
        if live_slots.len() > MAX_LIVE_ROOT_ENTRIES {
            return Err(SafepointMetadataError::TooManyLiveRoots(live_slots.len()));
        }

        let bytecode_len = u32::try_from(bytecode_len)
            .map_err(|_| SafepointMetadataError::BytecodeLengthTooLarge(bytecode_len))?;
        let native_code_len = u32::try_from(native_code_len)
            .map_err(|_| SafepointMetadataError::NativeCodeLengthTooLarge(native_code_len))?;
        let metadata_bytes = records
            .len()
            .checked_mul(size_of::<SafepointRecord>())
            .and_then(|bytes| {
                live_slots
                    .len()
                    .checked_mul(size_of::<u32>())
                    .and_then(|live_bytes| bytes.checked_add(live_bytes))
            })
            .and_then(|bytes| {
                instruction_starts
                    .len()
                    .checked_mul(size_of::<u32>())
                    .and_then(|starts_bytes| bytes.checked_add(starts_bytes))
            })
            .ok_or(SafepointMetadataError::SizeOverflow)?;
        if metadata_bytes > MAX_STACK_MAP_BYTES {
            return Err(SafepointMetadataError::MetadataTooLarge(metadata_bytes));
        }

        if instruction_starts.is_empty()
            || instruction_starts[0] != 0
            || instruction_starts.windows(2).any(|pair| pair[0] >= pair[1])
            || instruction_starts
                .last()
                .is_none_or(|&offset| offset >= bytecode_len)
        {
            return Err(SafepointMetadataError::InvalidInstructionStarts);
        }

        let mut native_offsets = Vec::new();
        native_offsets
            .try_reserve_exact(records.len())
            .map_err(|_| SafepointMetadataError::AllocationFailed)?;

        for (safepoint_index, record) in records.iter().enumerate() {
            if record.native_return_offset == 0 || record.native_return_offset > native_code_len {
                return Err(SafepointMetadataError::InvalidNativeOffset {
                    safepoint_index,
                    offset: record.native_return_offset,
                });
            }
            if instruction_starts
                .binary_search(&record.bytecode_offset)
                .is_err()
            {
                return Err(SafepointMetadataError::InvalidBytecodeOffset {
                    safepoint_index,
                    offset: record.bytecode_offset,
                });
            }
            if record.flags != SAFEPOINT_FLAG_ALLOCATING_HELPER {
                return Err(SafepointMetadataError::InvalidFlags {
                    safepoint_index,
                    flags: record.flags,
                });
            }
            if usize::try_from(record.result_slot)
                .ok()
                .is_none_or(|slot| slot >= frame_slot_count)
            {
                return Err(SafepointMetadataError::InvalidResultSlot {
                    safepoint_index,
                    slot: record.result_slot,
                });
            }

            let start = usize::try_from(record.live_slot_start)
                .map_err(|_| SafepointMetadataError::LiveRangeOverflow(safepoint_index))?;
            let count = usize::try_from(record.live_slot_count)
                .map_err(|_| SafepointMetadataError::LiveRangeOverflow(safepoint_index))?;
            let end = start
                .checked_add(count)
                .ok_or(SafepointMetadataError::LiveRangeOverflow(safepoint_index))?;
            let Some(record_slots) = live_slots.get(start..end) else {
                return Err(SafepointMetadataError::LiveRangeOutOfBounds(safepoint_index));
            };
            if record_slots.windows(2).any(|pair| pair[0] >= pair[1]) {
                return Err(SafepointMetadataError::LiveSlotsNotStrictlySorted(safepoint_index));
            }
            for &slot in record_slots {
                if usize::try_from(slot)
                    .ok()
                    .is_none_or(|slot| slot >= frame_slot_count)
                {
                    return Err(SafepointMetadataError::LiveSlotOutOfBounds {
                        safepoint_index,
                        slot,
                    });
                }
                if slot == record.result_slot {
                    return Err(SafepointMetadataError::ResultSlotIsLive(safepoint_index));
                }
            }
            native_offsets.push(record.native_return_offset);
        }

        native_offsets.sort_unstable();
        if native_offsets.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(SafepointMetadataError::DuplicateNativeOffset);
        }

        Ok(Self {
            frame_slot_count,
            bytecode_len,
            native_code_len,
            records: records.into_boxed_slice(),
            live_slots: live_slots.into_boxed_slice(),
            instruction_starts: instruction_starts.into_boxed_slice(),
        })
    }

    pub(crate) const fn frame_slot_count(&self) -> usize {
        self.frame_slot_count
    }

    pub(crate) const fn bytecode_len(&self) -> u32 {
        self.bytecode_len
    }

    pub(crate) const fn native_code_len(&self) -> u32 {
        self.native_code_len
    }

    pub(crate) fn records(&self) -> &[SafepointRecord] {
        &self.records
    }

    pub(crate) fn live_slots(&self) -> &[u32] {
        &self.live_slots
    }

    pub(crate) fn is_instruction_start(&self, offset: usize) -> bool {
        u32::try_from(offset)
            .ok()
            .is_some_and(|offset| self.instruction_starts.binary_search(&offset).is_ok())
    }

    fn record_for_publication(
        &self,
        safepoint_index: u32,
        bytecode_offset: u32,
    ) -> Option<&SafepointRecord> {
        let record = self.records.get(usize::try_from(safepoint_index).ok()?)?;
        (record.bytecode_offset == bytecode_offset).then_some(record)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SafepointMetadataError {
    TooManySafepoints(usize),
    TooManyLiveRoots(usize),
    BytecodeLengthTooLarge(usize),
    NativeCodeLengthTooLarge(usize),
    SizeOverflow,
    MetadataTooLarge(usize),
    InvalidInstructionStarts,
    InvalidNativeOffset { safepoint_index: usize, offset: u32 },
    InvalidBytecodeOffset { safepoint_index: usize, offset: u32 },
    InvalidFlags { safepoint_index: usize, flags: u32 },
    InvalidResultSlot { safepoint_index: usize, slot: u32 },
    LiveRangeOverflow(usize),
    LiveRangeOutOfBounds(usize),
    LiveSlotsNotStrictlySorted(usize),
    LiveSlotOutOfBounds { safepoint_index: usize, slot: u32 },
    ResultSlotIsLive(usize),
    DuplicateNativeOffset,
    AllocationFailed,
}

/// C-layout native root frame linked into `Context::visit_roots_for_gc`.
#[repr(C)]
pub(crate) struct JitShadowFrame {
    previous: *mut JitShadowFrame,
    slots: *mut JitSlot,
    slot_count: usize,
    records: *const SafepointRecord,
    record_count: usize,
    live_slots: *const u32,
    live_slot_count: usize,
    bytecode_offset: u32,
    safepoint_index: u32,
}

/// Lifetime-branded owner for initialized slot storage and immutable checked metadata.
pub(crate) struct ShadowFrameOwner<'slots, 'metadata> {
    raw: JitShadowFrame,
    slots: &'slots mut [JitSlot],
    metadata: &'metadata SafepointMetadata,
}

impl<'slots, 'metadata> ShadowFrameOwner<'slots, 'metadata> {
    pub(in crate::runtime::jit) fn new(
        slots: &'slots mut [JitSlot],
        metadata: &'metadata SafepointMetadata,
    ) -> Result<Self, ShadowFrameError> {
        if slots.len() != metadata.frame_slot_count() {
            return Err(ShadowFrameError::SlotCountMismatch {
                actual: slots.len(),
                expected: metadata.frame_slot_count(),
            });
        }

        Ok(Self {
            raw: JitShadowFrame {
                previous: ptr::null_mut(),
                slots: slots.as_mut_ptr(),
                slot_count: slots.len(),
                records: metadata.records.as_ptr(),
                record_count: metadata.records.len(),
                live_slots: metadata.live_slots.as_ptr(),
                live_slot_count: metadata.live_slots.len(),
                bytecode_offset: NO_BYTECODE_OFFSET,
                safepoint_index: NO_SAFEPOINT,
            },
            slots,
            metadata,
        })
    }

    fn as_mut_ptr(&mut self) -> *mut JitShadowFrame {
        ptr::from_mut(&mut self.raw)
    }

    fn as_ptr(&self) -> *mut JitShadowFrame {
        ptr::from_ref(&self.raw).cast_mut()
    }

    fn validate_schema(
        &self,
        expected_previous: *mut JitShadowFrame,
        require_quiescent: bool,
    ) -> Result<(), ActivationResultError> {
        if self.raw.previous != expected_previous
            || self.raw.slots != self.slots.as_ptr().cast_mut()
            || self.raw.slot_count != self.slots.len()
            || self.raw.records != self.metadata.records.as_ptr()
            || self.raw.record_count != self.metadata.records.len()
            || self.raw.live_slots != self.metadata.live_slots.as_ptr()
            || self.raw.live_slot_count != self.metadata.live_slots.len()
        {
            return Err(ActivationResultError::ShadowFrameChanged);
        }

        let publication_is_clear = self.raw.bytecode_offset == NO_BYTECODE_OFFSET
            && self.raw.safepoint_index == NO_SAFEPOINT;
        let publication_is_set = self.raw.bytecode_offset != NO_BYTECODE_OFFSET
            && self.raw.safepoint_index != NO_SAFEPOINT
            && self
                .metadata
                .record_for_publication(self.raw.safepoint_index, self.raw.bytecode_offset)
                .is_some();
        if (require_quiescent || !publication_is_set) && !publication_is_clear {
            return Err(ActivationResultError::InvalidSafepointPublication);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShadowFrameError {
    SlotCountMismatch { actual: usize, expected: usize },
}

/// Per-entry C-layout activation schema shared by Rust and generated code.
#[repr(C)]
pub(crate) struct JitActivation {
    abi_version: u32,
    struct_size: u32,
    frame: *mut JitShadowFrame,
    helpers: *const JitHelperTable,
    context_identity: *mut (),
    interrupt_budget: *mut DeterministicInterruptBudget,
    poll_state: *const InlinePollState,
    side_exit_offset: u32,
    native_backedge_work_remaining: u32,
    return_value_bits: u64,
    poisoned: u32,
    reserved_tail: u32,
}

#[cfg(test)]
impl JitActivation {
    pub(crate) fn invalid_header_with_dangling_frame_for_test() -> Self {
        Self {
            abi_version: GENERATED_CODE_ABI_VERSION.wrapping_add(1),
            struct_size: size_of::<Self>() as u32,
            frame: ptr::NonNull::<JitShadowFrame>::dangling().as_ptr(),
            helpers: ptr::null(),
            context_identity: ptr::null_mut(),
            interrupt_budget: ptr::null_mut(),
            poll_state: ptr::null(),
            side_exit_offset: 0,
            native_backedge_work_remaining: MAX_NATIVE_BACKEDGE_WORK_UNITS,
            return_value_bits: 0,
            poisoned: 0,
            reserved_tail: 0,
        }
    }
}

/// Lifetime-branded, thread-affine owner of one registered activation.
pub(crate) struct ActivationOwner<'context, 'owner, 'frame, 'slots, 'metadata, 'budget> {
    raw: JitActivation,
    context: &'context mut JitContextScope<'owner>,
    frame: &'frame mut ShadowFrameOwner<'slots, 'metadata>,
    budget: &'budget mut DeterministicInterruptBudget,
    previous: *mut JitShadowFrame,
    owner_thread: ThreadId,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl<'context, 'owner, 'frame, 'slots, 'metadata, 'budget>
    ActivationOwner<'context, 'owner, 'frame, 'slots, 'metadata, 'budget>
{
    pub(crate) fn new(
        context: &'context mut JitContextScope<'owner>,
        frame: &'frame mut ShadowFrameOwner<'slots, 'metadata>,
        budget: &'budget mut DeterministicInterruptBudget,
    ) -> Result<Self, ActivationCreateError> {
        frame
            .validate_schema(ptr::null_mut(), true)
            .map_err(ActivationCreateError::InvalidFrame)?;

        let raw_context = context.raw();
        validate_jit_slots(raw_context, &*frame.slots)
            .map_err(ActivationCreateError::InvalidSlots)?;

        let mut raw_context = raw_context;
        let previous = raw_context.jit_frame_head();
        let mut depth = 0_usize;
        let mut cursor = previous;
        while !cursor.is_null() {
            depth = depth
                .checked_add(1)
                .ok_or(ActivationCreateError::ActivationDepthExceeded)?;
            if depth >= MAX_NATIVE_ACTIVATION_DEPTH {
                return Err(ActivationCreateError::ActivationDepthExceeded);
            }
            // SAFETY: Every existing link is owned by a still-live `ActivationOwner` on this
            // thread. The bounded walk does not mutate it.
            cursor = unsafe { (*cursor).previous };
        }

        frame.raw.previous = previous;
        let frame_ptr = frame.as_mut_ptr();
        // SAFETY: `frame` is borrowed for this owner's full lifetime and records the exact previous
        // head. Drop restores that head before either borrow can end.
        unsafe { raw_context.set_jit_frame_head(frame_ptr) };

        let poll_state = budget.inline_state_ptr();
        #[cfg(not(test))]
        let native_backedge_work_remaining = MAX_NATIVE_BACKEDGE_WORK_UNITS;
        #[cfg(test)]
        let native_backedge_work_remaining =
            if test_backedge_poll_behavior() != TestBackedgePollBehavior::Normal {
                // Drive the real generated slow edge without waiting for the production hard cap.
                1
            } else {
                MAX_NATIVE_BACKEDGE_WORK_UNITS
            };
        Ok(Self {
            raw: JitActivation {
                abi_version: GENERATED_CODE_ABI_VERSION,
                struct_size: size_of::<JitActivation>() as u32,
                frame: frame_ptr,
                helpers: ptr::from_ref(&JIT_HELPERS),
                context_identity: raw_context.jit_raw_identity(),
                interrupt_budget: ptr::from_mut(budget),
                poll_state,
                side_exit_offset: 0,
                native_backedge_work_remaining,
                return_value_bits: 0,
                poisoned: 0,
                reserved_tail: 0,
            },
            context,
            frame,
            budget,
            previous,
            owner_thread: thread::current().id(),
            _not_send_or_sync: PhantomData,
        })
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut JitActivation {
        ptr::from_mut(&mut self.raw)
    }

    #[cfg(test)]
    pub(crate) fn set_native_backedge_work_remaining_for_test(&mut self, remaining: u32) {
        assert!(remaining <= MAX_NATIVE_BACKEDGE_WORK_UNITS);
        self.raw.native_backedge_work_remaining = remaining;
    }

    #[cfg(test)]
    pub(crate) fn native_backedge_work_remaining_for_test(&self) -> u32 {
        self.raw.native_backedge_work_remaining
    }

    pub(in crate::runtime::jit) fn context(&self) -> &JitContextScope<'owner> {
        &*self.context
    }

    pub(crate) fn validate_header(&self) -> Result<(), ActivationResultError> {
        if self.owner_thread != thread::current().id()
            || self.raw.abi_version != GENERATED_CODE_ABI_VERSION
            || self.raw.struct_size as usize != size_of::<JitActivation>()
            || self.raw.frame != self.frame.as_ptr()
            || self.raw.helpers != ptr::from_ref(&JIT_HELPERS)
            || self.raw.context_identity != self.context.raw().jit_raw_identity()
            || self.raw.interrupt_budget != ptr::from_ref(self.budget).cast_mut()
            || !self.budget.validate_inline_state_ptr(self.raw.poll_state)
        {
            return Err(ActivationResultError::InvalidHeader);
        }
        if self.raw.native_backedge_work_remaining > MAX_NATIVE_BACKEDGE_WORK_UNITS
            || self.raw.reserved_tail != 0
        {
            return Err(ActivationResultError::ReservedFieldChanged);
        }
        self.frame.validate_schema(self.previous, true)
    }

    /// Validate generated outputs without constructing a `Value` from unchecked raw bits.
    pub(crate) fn validate_result(
        &mut self,
        status: u32,
    ) -> Result<ActivationOutcome, ActivationResultError> {
        let result = self.validate_result_inner(status);
        if result.is_err() {
            self.frame.slots.fill(JitSlot::undefined());
        }
        result
    }

    fn validate_result_inner(
        &self,
        status: u32,
    ) -> Result<ActivationOutcome, ActivationResultError> {
        self.validate_header()?;
        validate_jit_slots(self.context.raw(), &*self.frame.slots)
            .map_err(ActivationResultError::InvalidFinalSlots)?;
        match status {
            STATUS_RETURNED => {
                if self.raw.poisoned != 0 {
                    return Err(ActivationResultError::UnexpectedPoison);
                }
                if self.raw.return_value_bits == 0 {
                    return Err(ActivationResultError::ZeroReturnValue);
                }
                let returned = JitSlot(Value::from_raw_bits(self.raw.return_value_bits));
                validate_jit_slots(self.context.raw(), std::slice::from_ref(&returned))
                    .map_err(ActivationResultError::InvalidReturnValue)?;
                if self.raw.side_exit_offset != 0 {
                    return Err(ActivationResultError::UnexpectedSideExitOffset);
                }
                Ok(ActivationOutcome::Returned(self.raw.return_value_bits))
            }
            STATUS_SIDE_EXIT | STATUS_INTERRUPTED | STATUS_ALLOCATION_FAILED | STATUS_POISONED => {
                if self.raw.return_value_bits != 0 {
                    return Err(ActivationResultError::UnexpectedReturnValue);
                }
                let offset = self.raw.side_exit_offset as usize;
                if !self.frame.metadata.is_instruction_start(offset) {
                    return Err(ActivationResultError::InvalidSideExitOffset(offset));
                }

                match status {
                    STATUS_SIDE_EXIT if self.raw.poisoned == 0 => {
                        Ok(ActivationOutcome::SideExit(offset))
                    }
                    STATUS_INTERRUPTED if self.raw.poisoned == 0 => {
                        Ok(ActivationOutcome::Interrupted(offset))
                    }
                    STATUS_ALLOCATION_FAILED if self.raw.poisoned == 0 => {
                        Ok(ActivationOutcome::AllocationFailed(offset))
                    }
                    STATUS_POISONED if self.raw.poisoned == 1 => {
                        Ok(ActivationOutcome::Poisoned(offset))
                    }
                    STATUS_POISONED => Err(ActivationResultError::MissingPoison),
                    _ => Err(ActivationResultError::UnexpectedPoison),
                }
            }
            STATUS_INVALID_ACTIVATION => {
                if self.raw.return_value_bits != 0 {
                    return Err(ActivationResultError::UnexpectedReturnValue);
                }
                if self.raw.side_exit_offset != 0 {
                    return Err(ActivationResultError::UnexpectedSideExitOffset);
                }
                if self.raw.poisoned != 0 {
                    return Err(ActivationResultError::UnexpectedPoison);
                }
                Ok(ActivationOutcome::InvalidActivation)
            }
            other => Err(ActivationResultError::UnknownStatus(other)),
        }
    }

    /// Copy every validated native slot into the active Brimstone handle scope.
    ///
    /// This is the bridge from a completed native activation to an ordinary VM frame. It performs
    /// no Brimstone heap allocation, and it must run before this activation unlinks its shadow
    /// frame. The returned handles remain roots until the higher-ranked JIT context scope exits.
    pub(in crate::runtime::jit) fn capture_all_slot_roots(
        &mut self,
    ) -> Result<RootedSlotSet<'owner>, ActivationResultError> {
        let context = self.context.raw();
        let slots = self.validated_side_exit_slots()?;

        let mut roots = Vec::new();
        roots
            .try_reserve_exact(slots.len())
            .map_err(|_| ActivationResultError::BridgeRootAllocationFailed)?;
        for slot in slots {
            roots.push(slot.value().to_handle(context));
        }
        Ok(RootedSlotSet { roots, _brand: PhantomData })
    }

    /// Return the activation-owned slots after validating a completed ordinary side exit.
    pub(in crate::runtime::jit) fn validated_side_exit_slots(
        &mut self,
    ) -> Result<&[JitSlot], ActivationResultError> {
        if let Err(error) = self.validate_side_exit_slot_state() {
            self.frame.slots.fill(JitSlot::undefined());
            return Err(error);
        }
        Ok(&*self.frame.slots)
    }

    fn validate_side_exit_slot_state(&self) -> Result<(), ActivationResultError> {
        self.validate_header()?;
        if self.frame.raw.bytecode_offset != NO_BYTECODE_OFFSET
            || self.frame.raw.safepoint_index != NO_SAFEPOINT
        {
            return Err(ActivationResultError::InvalidSafepointPublication);
        }
        validate_jit_slots(self.context.raw(), &*self.frame.slots)
            .map_err(ActivationResultError::InvalidSideExitSlots)
    }

    /// Root a return value which has already been accepted by `validate_result`.
    pub(crate) fn capture_validated_return_root(
        &mut self,
        bits: u64,
    ) -> Result<Handle<Value>, ActivationResultError> {
        let result = self.capture_validated_return_root_inner(bits);
        if result.is_err() {
            self.frame.slots.fill(JitSlot::undefined());
        }
        result
    }

    fn capture_validated_return_root_inner(
        &self,
        bits: u64,
    ) -> Result<Handle<Value>, ActivationResultError> {
        if bits == 0 || bits != self.raw.return_value_bits {
            return Err(ActivationResultError::UnexpectedReturnValue);
        }
        let returned = JitSlot(Value::from_raw_bits(bits));
        validate_jit_slots(self.context.raw(), std::slice::from_ref(&returned))
            .map_err(ActivationResultError::InvalidReturnValue)?;
        Ok(returned.value().to_handle(self.context.raw()))
    }

    #[cfg(test)]
    fn publish_for_test(&mut self, safepoint_index: u32, bytecode_offset: u32) {
        self.frame.raw.safepoint_index = safepoint_index;
        self.frame.raw.bytecode_offset = bytecode_offset;
    }

    #[cfg(test)]
    fn clear_publication_for_test(&mut self) {
        clear_frame_publication(&mut self.frame.raw);
    }
}

impl Drop for ActivationOwner<'_, '_, '_, '_, '_, '_> {
    fn drop(&mut self) {
        if self.owner_thread != thread::current().id() {
            std::process::abort();
        }

        let mut context = self.context.raw();
        if context.jit_frame_head() != self.frame.as_ptr() {
            // Out-of-order unlink would leave a dangling intrusive link. This cannot be recovered
            // safely, so release builds fail closed as well.
            std::process::abort();
        }

        clear_frame_publication(&mut self.frame.raw);
        // SAFETY: Exact LIFO identity was checked above and `previous` was captured at link time.
        unsafe { context.set_jit_frame_head(self.previous) };
        self.frame.raw.previous = ptr::null_mut();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActivationCreateError {
    InvalidFrame(ActivationResultError),
    InvalidSlots(JitSlotValidationError),
    ActivationDepthExceeded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActivationOutcome {
    Returned(u64),
    SideExit(usize),
    Interrupted(usize),
    AllocationFailed(usize),
    Poisoned(usize),
    InvalidActivation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActivationResultError {
    InvalidHeader,
    ShadowFrameChanged,
    InvalidSafepointPublication,
    ReservedFieldChanged,
    UnknownStatus(u32),
    ZeroReturnValue,
    InvalidReturnValue(JitSlotValidationError),
    InvalidFinalSlots(JitSlotValidationError),
    InvalidSideExitSlots(JitSlotValidationError),
    BridgeRootAllocationFailed,
    UnexpectedReturnValue,
    UnexpectedSideExitOffset,
    InvalidSideExitOffset(usize),
    UnexpectedPoison,
    MissingPoison,
}

fn clear_frame_publication(frame: &mut JitShadowFrame) {
    frame.bytecode_offset = NO_BYTECODE_OFFSET;
    frame.safepoint_index = NO_SAFEPOINT;
}

/// Visit every published native root using Brimstone's existing `Value` semantics.
///
/// # Safety
///
/// `head` must be the context's activation chain. Every node, slot array, and metadata array must
/// remain live through this call. `ActivationOwner` is the only linker and establishes those
/// invariants. Corruption aborts before an unchecked slot or pointer is presented to the visitor.
pub(crate) unsafe fn visit_registered_roots(
    mut head: *mut JitShadowFrame,
    visitor: &mut impl HeapVisitor,
) {
    let mut depth = 0_usize;
    while !head.is_null() {
        depth += 1;
        if depth > MAX_NATIVE_ACTIVATION_DEPTH {
            std::process::abort();
        }

        // SAFETY: Required by this function's contract and bounded above.
        let frame = unsafe { &mut *head };
        if frame.slot_count > crate::runtime::jit::compiler::MAX_PROTOTYPE_FRAME_SLOTS
            || frame.record_count > MAX_SAFEPOINT_RECORDS
            || frame.live_slot_count > MAX_LIVE_ROOT_ENTRIES
            || frame.slots.is_null()
            || frame.records.is_null()
            || frame.live_slots.is_null()
        {
            std::process::abort();
        }
        if frame.bytecode_offset == NO_BYTECODE_OFFSET || frame.safepoint_index == NO_SAFEPOINT {
            // A collection while any linked frame is between explicit safepoints would expose
            // native temporaries with no liveness proof. This gate never does that and fails closed
            // if future re-entrancy violates the rule.
            std::process::abort();
        }

        let Ok(record_index) = usize::try_from(frame.safepoint_index) else {
            std::process::abort();
        };
        if record_index >= frame.record_count {
            std::process::abort();
        }
        // SAFETY: Count and index were bounded above.
        let record = unsafe { &*frame.records.add(record_index) };
        if record.bytecode_offset != frame.bytecode_offset
            || record.flags != SAFEPOINT_FLAG_ALLOCATING_HELPER
            || record.result_slot as usize >= frame.slot_count
        {
            std::process::abort();
        }
        let Ok(start) = usize::try_from(record.live_slot_start) else {
            std::process::abort();
        };
        let Ok(count) = usize::try_from(record.live_slot_count) else {
            std::process::abort();
        };
        let Some(end) = start.checked_add(count) else {
            std::process::abort();
        };
        if end > frame.live_slot_count {
            std::process::abort();
        }

        let mut previous_slot = None;
        for live_index in start..end {
            // SAFETY: The flattened range was checked against the live-slot allocation.
            let slot_index = unsafe { *frame.live_slots.add(live_index) };
            if previous_slot.is_some_and(|previous| previous >= slot_index) {
                std::process::abort();
            }
            previous_slot = Some(slot_index);
            let Ok(slot_index) = usize::try_from(slot_index) else {
                std::process::abort();
            };
            if slot_index >= frame.slot_count || slot_index == record.result_slot as usize {
                std::process::abort();
            }

            // SAFETY: The frame owner keeps this initialized `JitSlot` live. Only
            // liveness-derived indices reach this point; dead slots are never read or presented.
            let slot = unsafe { &mut *frame.slots.add(slot_index) };
            let mut value = slot.value();
            visitor.visit_value(&mut value);
            slot.write_gc_value(value);
        }

        head = frame.previous;
    }
}

/// Validate the current publication before an allocating helper can enter the runtime.
fn validate_helper_activation(
    activation: &mut JitActivation,
) -> Result<(*mut JitShadowFrame, SafepointRecord, Context), u32> {
    if activation.abi_version != GENERATED_CODE_ABI_VERSION
        || activation.struct_size as usize != size_of::<JitActivation>()
        || activation.frame.is_null()
        || activation.helpers != ptr::from_ref(&JIT_HELPERS)
        || activation.context_identity.is_null()
        || activation.interrupt_budget.is_null()
        || activation.poll_state.is_null()
        || activation.return_value_bits != 0
        || activation.poisoned != 0
        || activation.native_backedge_work_remaining == 0
        || activation.native_backedge_work_remaining > MAX_NATIVE_BACKEDGE_WORK_UNITS
        || activation.reserved_tail != 0
    {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }

    // SAFETY: The private activation owner keeps this exact budget alive and uniquely borrowed
    // for the synchronous generated call. The stable poll allocation is jointly owned by that
    // budget and request handles, and its identity/header must still match before any helper work.
    let budget = unsafe { &mut *activation.interrupt_budget };
    if !budget.validate_inline_state_ptr(activation.poll_state) {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }

    // SAFETY: Non-null frame identity is lifetime-owned by the validated activation contract.
    let frame = unsafe { &mut *activation.frame };
    // SAFETY: The private activation owner binds this exact identity to a live context. Checking
    // the current head before touching frame arrays rejects stale or out-of-order activation use.
    let context = unsafe { Context::from_jit_raw_identity(activation.context_identity) };
    if context.jit_frame_head() != activation.frame {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }
    if frame.slot_count > crate::runtime::jit::compiler::MAX_PROTOTYPE_FRAME_SLOTS
        || frame.record_count > MAX_SAFEPOINT_RECORDS
        || frame.live_slot_count > MAX_LIVE_ROOT_ENTRIES
        || frame.slots.is_null()
        || frame.records.is_null()
        || frame.live_slots.is_null()
        || frame.bytecode_offset == NO_BYTECODE_OFFSET
        || frame.safepoint_index == NO_SAFEPOINT
    {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }
    let record_index =
        usize::try_from(frame.safepoint_index).map_err(|_| HELPER_STATUS_INVALID_ACTIVATION)?;
    if record_index >= frame.record_count {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }
    // SAFETY: Bounds checked against the immutable record array.
    let record = unsafe { &*frame.records.add(record_index) };
    if record.bytecode_offset != frame.bytecode_offset
        || record.flags != SAFEPOINT_FLAG_ALLOCATING_HELPER
    {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }
    let result_slot =
        usize::try_from(record.result_slot).map_err(|_| HELPER_STATUS_INVALID_ACTIVATION)?;
    if result_slot >= frame.slot_count {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }

    let start =
        usize::try_from(record.live_slot_start).map_err(|_| HELPER_STATUS_INVALID_ACTIVATION)?;
    let count =
        usize::try_from(record.live_slot_count).map_err(|_| HELPER_STATUS_INVALID_ACTIVATION)?;
    let end = start
        .checked_add(count)
        .ok_or(HELPER_STATUS_INVALID_ACTIVATION)?;
    if end > frame.live_slot_count {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }
    let mut previous = None;
    for index in start..end {
        // SAFETY: Flattened range checked above.
        let slot = unsafe { *frame.live_slots.add(index) };
        if previous.is_some_and(|previous| previous >= slot)
            || usize::try_from(slot)
                .ok()
                .is_none_or(|slot| slot >= frame.slot_count || slot == result_slot)
        {
            return Err(HELPER_STATUS_INVALID_ACTIVATION);
        }
        previous = Some(slot);
    }

    Ok((ptr::from_mut(frame), *record, context))
}

/// Validate the strictly nonallocating backedge helper boundary.
///
/// The exact branch target has already been published in `side_exit_offset` by generated code.
/// The helper does not need to interpret that value to continue: the target is an immediate in the
/// verified generated CFG, while any terminal result is independently checked against immutable
/// instruction-start metadata by `ActivationOwner`. The shadow frame must be quiescent because a
/// backedge poll is not a moving-GC safepoint and may not overlap an allocating helper.
fn validate_backedge_poll_activation(
    activation: &mut JitActivation,
) -> Result<*mut DeterministicInterruptBudget, u32> {
    if activation.abi_version != GENERATED_CODE_ABI_VERSION
        || activation.struct_size as usize != size_of::<JitActivation>()
        || activation.frame.is_null()
        || activation.helpers != ptr::from_ref(&JIT_HELPERS)
        || activation.context_identity.is_null()
        || activation.interrupt_budget.is_null()
        || activation.poll_state.is_null()
        || activation.return_value_bits != 0
        || activation.poisoned != 0
        || activation.native_backedge_work_remaining > MAX_NATIVE_BACKEDGE_WORK_UNITS
        || activation.reserved_tail != 0
    {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }

    // SAFETY: Same lifetime-owned activation/budget contract as the allocating helper. Slow poll
    // entry may observe a zero native-residency count because generated code consumes the exact
    // edge inline before calling Rust for the hard-cap decision.
    let budget = unsafe { &mut *activation.interrupt_budget };
    if !budget.validate_inline_state_ptr(activation.poll_state) {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }

    // SAFETY: The private activation owner binds this identity to a live context for the complete
    // synchronous generated call. Checking the current frame head rejects stale and out-of-order
    // use before the budget is touched.
    let context = unsafe { Context::from_jit_raw_identity(activation.context_identity) };
    if context.jit_frame_head() != activation.frame {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }

    // SAFETY: The activation owner keeps this exact frame alive and registered. Unlike the
    // allocating helper, a poll neither reads nor rewrites slot storage or stack-map arrays.
    let frame = unsafe { &*activation.frame };
    if frame.slot_count > crate::runtime::jit::compiler::MAX_PROTOTYPE_FRAME_SLOTS
        || frame.record_count > MAX_SAFEPOINT_RECORDS
        || frame.live_slot_count > MAX_LIVE_ROOT_ENTRIES
        || frame.slots.is_null()
        || frame.records.is_null()
        || frame.live_slots.is_null()
        || frame.bytecode_offset != NO_BYTECODE_OFFSET
        || frame.safepoint_index != NO_SAFEPOINT
    {
        return Err(HELPER_STATUS_INVALID_ACTIVATION);
    }

    Ok(ptr::from_mut(budget))
}

/// Clear every slot not proven live by this exact safepoint, including the pre-call result slot.
///
/// The moving collector deliberately visits only compiler-derived live roots. Clearing the
/// complement before any helper poll/allocation/panic ensures dead pointer bits can never survive
/// a moving collection and later escape through a terminal native outcome.
fn clear_non_live_helper_slots(frame: *mut JitShadowFrame, record: SafepointRecord) {
    // SAFETY: `validate_helper_activation` just validated this exact frame, record, flattened live
    // range, sorted uniqueness, and every slot index.
    let frame = unsafe { &mut *frame };
    let start = record.live_slot_start as usize;
    let end = start + record.live_slot_count as usize;
    let mut live_index = start;

    for slot_index in 0..frame.slot_count {
        let is_live = if live_index < end {
            // SAFETY: The validated flattened range is within `live_slot_count`.
            unsafe { *frame.live_slots.add(live_index) as usize == slot_index }
        } else {
            false
        };
        if is_live {
            live_index += 1;
        } else {
            // SAFETY: Every slot is initialized and `slot_index < slot_count`.
            unsafe { (*frame.slots.add(slot_index)).write_gc_value(Value::undefined()) };
        }
    }
    debug_assert_eq!(live_index, end);
}

unsafe extern "C" fn backedge_poll_helper(activation: *mut JitActivation) -> u32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if activation.is_null() {
            return HELPER_STATUS_INVALID_ACTIVATION;
        }
        // SAFETY: Generated code obtains this function only from the private versioned table and
        // passes back its unchanged lifetime-owned activation pointer.
        let activation = unsafe { &mut *activation };
        backedge_poll_helper_inner(activation)
    }));

    match result {
        Ok(status) => status,
        Err(_) => {
            // A Rust unwind must never cross generated code. Mark only the activation header; the
            // owner validates, unlinks, and terminates the contained run without resuming.
            if !activation.is_null() {
                // SAFETY: Same private helper-entry contract as above; this touches no frame,
                // context, slot, or budget storage.
                unsafe { (*activation).poisoned = 1 };
            }
            HELPER_STATUS_POISONED
        }
    }
}

fn backedge_poll_helper_inner(activation: &mut JitActivation) -> u32 {
    let budget = match validate_backedge_poll_activation(activation) {
        Ok(budget) => budget,
        Err(status) => return status,
    };

    #[cfg(test)]
    let behavior = test_backedge_poll_started();

    // Generated code has already consumed the hard native-residency unit. Consume the ordinary
    // deterministic work unit here only because the inline path selected this slow boundary.
    // Poll first so an external request or quantum expiry has priority over a simultaneous hard
    // cap or injected policy failure.
    // SAFETY: Validation proved this is the exact non-null pointer uniquely borrowed by the live
    // activation owner, and no other helper or generated instruction can access it concurrently.
    let poll = match unsafe { &mut *budget }.poll_after_work(1) {
        Ok(poll) => poll,
        Err(_) => return HELPER_STATUS_INVALID_ACTIVATION,
    };
    if poll.is_due() {
        return HELPER_STATUS_INTERRUPTED;
    }

    #[cfg(test)]
    match behavior {
        TestBackedgePollBehavior::Panic => panic!("injected contained backedge-poll panic"),
        TestBackedgePollBehavior::PolicyFailure => return HELPER_STATUS_INVALID_ACTIVATION,
        TestBackedgePollBehavior::PolicySideExit => return HELPER_STATUS_SIDE_EXIT,
        TestBackedgePollBehavior::Normal => {}
    }

    if activation.native_backedge_work_remaining == 0 {
        return HELPER_STATUS_SIDE_EXIT;
    }
    HELPER_STATUS_OK
}

unsafe extern "C" fn new_object_zero_helper(activation: *mut JitActivation) -> u32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if activation.is_null() {
            return HELPER_STATUS_INVALID_ACTIVATION;
        }
        // SAFETY: Generated code can obtain this function only from the private helper table in a
        // lifetime-owned activation. It passes that same live activation pointer back unchanged.
        let activation = unsafe { &mut *activation };
        new_object_zero_helper_inner(activation)
    }));

    match result {
        Ok(status) => status,
        Err(_) => {
            // No unwind may cross generated code. The contained runner is poisoned and may only
            // unlink/terminate; it must not resume or perform another allocation.
            if !activation.is_null() {
                // SAFETY: Same exact private helper-entry contract as above. This write does not
                // dereference context/frame storage and is used only for terminal validation.
                unsafe { (*activation).poisoned = 1 };
            }
            HELPER_STATUS_POISONED
        }
    }
}

fn new_object_zero_helper_inner(activation: &mut JitActivation) -> u32 {
    let (frame, record, context) = match validate_helper_activation(activation) {
        Ok(parts) => parts,
        Err(status) => return status,
    };

    clear_non_live_helper_slots(frame, record);

    #[cfg(test)]
    let behavior = test_helper_call_started();

    #[cfg(test)]
    if behavior == TestHelperBehavior::PanicBeforeAllocation {
        panic!("injected contained-helper panic");
    }

    // SAFETY: The activation owner holds the budget borrow and validated its exact pointer.
    let budget = unsafe { &mut *activation.interrupt_budget };
    let Ok(poll) = budget.poll_after_work(1) else {
        return HELPER_STATUS_INVALID_ACTIVATION;
    };
    if poll.is_due() {
        return HELPER_STATUS_INTERRUPTED;
    }

    #[cfg(test)]
    if behavior == TestHelperBehavior::AllocationFailure {
        return HELPER_STATUS_ALLOCATION_FAILED;
    }

    #[cfg(test)]
    if behavior == TestHelperBehavior::ForceCollectionThenAllocationFailure {
        Heap::run_gc(context, crate::runtime::gc::GcType::Normal);
        return HELPER_STATUS_ALLOCATION_FAILED;
    }

    #[cfg(test)]
    if behavior == TestHelperBehavior::ForceCollectionThenPanic {
        Heap::run_gc(context, crate::runtime::gc::GcType::Normal);
        panic!("injected contained-helper panic after collection");
    }

    let guard = HandleScopeGuard::new(context);
    let object = match ordinary_object_create(context) {
        Ok(object) => object,
        Err(_) => return HELPER_STATUS_ALLOCATION_FAILED,
    };

    #[cfg(test)]
    test_record_object_before(object);

    #[cfg(test)]
    if behavior == TestHelperBehavior::ForceCollectionAfterAllocation {
        Heap::run_gc(context, crate::runtime::gc::GcType::Normal);
    }

    #[cfg(test)]
    test_record_object_after(object);

    let object_value: Handle<Value> = object.into();
    let value = *object_value;
    // SAFETY: The validated result index is within the lifetime-owned initialized slot array. The
    // object remains protected by `object_value` until after its post-collection bits are stored.
    // SAFETY: `validate_helper_activation` checked this exact live frame and result index. It is
    // still lifetime-owned by the activation, and no operation above can unlink it.
    unsafe {
        (*frame)
            .slots
            .add(record.result_slot as usize)
            .write(JitSlot(value))
    };
    drop(guard);
    HELPER_STATUS_OK
}

pub(crate) const ACTIVATION_ABI_VERSION_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, abi_version) as i32;
pub(crate) const ACTIVATION_STRUCT_SIZE_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, struct_size) as i32;
pub(crate) const ACTIVATION_FRAME_OFFSET: i32 = std::mem::offset_of!(JitActivation, frame) as i32;
pub(crate) const ACTIVATION_HELPERS_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, helpers) as i32;
pub(crate) const ACTIVATION_CONTEXT_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, context_identity) as i32;
pub(crate) const ACTIVATION_INTERRUPT_BUDGET_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, interrupt_budget) as i32;
pub(crate) const ACTIVATION_POLL_STATE_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, poll_state) as i32;
pub(crate) const ACTIVATION_SIDE_EXIT_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, side_exit_offset) as i32;
pub(crate) const ACTIVATION_NATIVE_BACKEDGE_WORK_REMAINING_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, native_backedge_work_remaining) as i32;
pub(crate) const ACTIVATION_RETURN_VALUE_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, return_value_bits) as i32;
pub(crate) const ACTIVATION_POISONED_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, poisoned) as i32;
pub(crate) const ACTIVATION_RESERVED_TAIL_OFFSET: i32 =
    std::mem::offset_of!(JitActivation, reserved_tail) as i32;

pub(crate) const HELPER_TABLE_ABI_VERSION_OFFSET: i32 =
    std::mem::offset_of!(JitHelperTable, abi_version) as i32;
pub(crate) const HELPER_TABLE_STRUCT_SIZE_OFFSET: i32 =
    std::mem::offset_of!(JitHelperTable, struct_size) as i32;
pub(crate) const HELPER_TABLE_RESERVED_OFFSET: i32 =
    std::mem::offset_of!(JitHelperTable, reserved) as i32;
pub(crate) const HELPER_TABLE_NEW_OBJECT_ZERO_OFFSET: i32 =
    std::mem::offset_of!(JitHelperTable, new_object_zero) as i32;
pub(crate) const HELPER_TABLE_BACKEDGE_POLL_OFFSET: i32 =
    std::mem::offset_of!(JitHelperTable, backedge_poll) as i32;

pub(crate) const SHADOW_FRAME_PREVIOUS_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, previous) as i32;
pub(crate) const SHADOW_FRAME_SLOTS_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, slots) as i32;
pub(crate) const SHADOW_FRAME_SLOT_COUNT_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, slot_count) as i32;
pub(crate) const SHADOW_FRAME_RECORDS_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, records) as i32;
pub(crate) const SHADOW_FRAME_RECORD_COUNT_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, record_count) as i32;
pub(crate) const SHADOW_FRAME_LIVE_SLOTS_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, live_slots) as i32;
pub(crate) const SHADOW_FRAME_LIVE_SLOT_COUNT_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, live_slot_count) as i32;
pub(crate) const SHADOW_FRAME_BYTECODE_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, bytecode_offset) as i32;
pub(crate) const SHADOW_FRAME_SAFEPOINT_INDEX_OFFSET: i32 =
    std::mem::offset_of!(JitShadowFrame, safepoint_index) as i32;

pub(crate) const JIT_ACTIVATION_SIZE: u32 = size_of::<JitActivation>() as u32;
pub(crate) const JIT_HELPER_TABLE_SIZE: u32 = size_of::<JitHelperTable>() as u32;

const _: () = {
    assert!(size_of::<Value>() == size_of::<u64>());
    assert!(size_of::<JitHelperTable>() == 32);
    assert!(size_of::<SafepointRecord>() == 24);
    assert!(size_of::<JitShadowFrame>() == 64);
    assert!(size_of::<JitActivation>() == 72);
};

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TestHelperBehavior {
    #[default]
    Normal,
    ForceCollectionAfterAllocation,
    ForceCollectionThenAllocationFailure,
    ForceCollectionThenPanic,
    AllocationFailure,
    PanicBeforeAllocation,
}

/// Deterministic adversarial behavior for the nonallocating native-backedge helper.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TestBackedgePollBehavior {
    #[default]
    Normal,
    /// Model a hard native-residency policy boundary without executing one million iterations.
    PolicySideExit,
    /// Model an internal budget/owner-policy failure.
    PolicyFailure,
    Panic,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TestBackedgePollObservation {
    pub(crate) calls: u32,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TestHelperObservation {
    pub(crate) calls: u32,
    pub(crate) object_before: usize,
    pub(crate) object_after: usize,
}

#[cfg(test)]
std::thread_local! {
    static TEST_HELPER_BEHAVIOR: std::cell::Cell<TestHelperBehavior> =
        const { std::cell::Cell::new(TestHelperBehavior::Normal) };
    static TEST_HELPER_OBSERVATION: std::cell::Cell<TestHelperObservation> =
        const { std::cell::Cell::new(TestHelperObservation {
            calls: 0,
            object_before: 0,
            object_after: 0,
        }) };
    static TEST_BACKEDGE_POLL_BEHAVIOR: std::cell::Cell<TestBackedgePollBehavior> =
        const { std::cell::Cell::new(TestBackedgePollBehavior::Normal) };
    static TEST_BACKEDGE_POLL_OBSERVATION: std::cell::Cell<TestBackedgePollObservation> =
        const { std::cell::Cell::new(TestBackedgePollObservation { calls: 0 }) };
}

#[cfg(test)]
pub(crate) fn with_test_helper_behavior<R>(
    behavior: TestHelperBehavior,
    f: impl FnOnce() -> R,
) -> (R, TestHelperObservation) {
    struct ResetGuard(TestHelperBehavior);
    impl Drop for ResetGuard {
        fn drop(&mut self) {
            TEST_HELPER_BEHAVIOR.with(|cell| cell.set(self.0));
        }
    }

    let previous = TEST_HELPER_BEHAVIOR.with(|cell| cell.replace(behavior));
    TEST_HELPER_OBSERVATION.with(|cell| cell.set(TestHelperObservation::default()));
    let guard = ResetGuard(previous);
    let result = f();
    let observation = TEST_HELPER_OBSERVATION.with(std::cell::Cell::get);
    drop(guard);
    (result, observation)
}

#[cfg(test)]
pub(crate) fn with_test_backedge_poll_behavior<R>(
    behavior: TestBackedgePollBehavior,
    f: impl FnOnce() -> R,
) -> (R, TestBackedgePollObservation) {
    struct ResetGuard(TestBackedgePollBehavior);
    impl Drop for ResetGuard {
        fn drop(&mut self) {
            TEST_BACKEDGE_POLL_BEHAVIOR.with(|cell| cell.set(self.0));
        }
    }

    let previous = TEST_BACKEDGE_POLL_BEHAVIOR.with(|cell| cell.replace(behavior));
    TEST_BACKEDGE_POLL_OBSERVATION.with(|cell| {
        cell.set(TestBackedgePollObservation::default());
    });
    let guard = ResetGuard(previous);
    let result = f();
    let observation = TEST_BACKEDGE_POLL_OBSERVATION.with(std::cell::Cell::get);
    drop(guard);
    (result, observation)
}

#[cfg(test)]
fn test_helper_call_started() -> TestHelperBehavior {
    TEST_HELPER_OBSERVATION.with(|cell| {
        let mut observation = cell.get();
        observation.calls = observation.calls.saturating_add(1);
        cell.set(observation);
    });
    TEST_HELPER_BEHAVIOR.with(std::cell::Cell::get)
}

#[cfg(test)]
fn test_backedge_poll_started() -> TestBackedgePollBehavior {
    TEST_BACKEDGE_POLL_OBSERVATION.with(|cell| {
        let mut observation = cell.get();
        observation.calls = observation.calls.saturating_add(1);
        cell.set(observation);
    });
    TEST_BACKEDGE_POLL_BEHAVIOR.with(std::cell::Cell::get)
}

#[cfg(test)]
fn test_backedge_poll_behavior() -> TestBackedgePollBehavior {
    TEST_BACKEDGE_POLL_BEHAVIOR.with(std::cell::Cell::get)
}

#[cfg(test)]
fn test_record_object_before(object: Handle<ObjectValue>) {
    TEST_HELPER_OBSERVATION.with(|cell| {
        let mut observation = cell.get();
        observation.object_before = object.as_value().as_raw_bits() as usize;
        cell.set(observation);
    });
}

#[cfg(test)]
fn test_record_object_after(object: Handle<ObjectValue>) {
    TEST_HELPER_OBSERVATION.with(|cell| {
        let mut observation = cell.get();
        observation.object_after = object.as_value().as_raw_bits() as usize;
        cell.set(observation);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ContextBuilder, gc::GcType};

    fn metadata(live_slots: Vec<u32>) -> SafepointMetadata {
        SafepointMetadata::new(
            3,
            8,
            8,
            vec![SafepointRecord {
                native_return_offset: 4,
                bytecode_offset: 0,
                live_slot_start: 0,
                live_slot_count: live_slots.len() as u32,
                result_slot: 1,
                flags: SAFEPOINT_FLAG_ALLOCATING_HELPER,
            }],
            live_slots,
            vec![0, 2],
        )
        .unwrap()
    }

    #[test]
    fn malformed_stack_maps_are_rejected_before_registration() {
        let record = SafepointRecord {
            native_return_offset: 4,
            bytecode_offset: 0,
            live_slot_start: 0,
            live_slot_count: 2,
            result_slot: 1,
            flags: SAFEPOINT_FLAG_ALLOCATING_HELPER,
        };
        assert_eq!(
            SafepointMetadata::new(3, 8, 8, vec![record], vec![2, 0], vec![0]).unwrap_err(),
            SafepointMetadataError::LiveSlotsNotStrictlySorted(0)
        );
        assert_eq!(
            SafepointMetadata::new(3, 8, 8, vec![record], vec![0, 1], vec![0]).unwrap_err(),
            SafepointMetadataError::ResultSlotIsLive(0)
        );

        let mut bad_boundary = record;
        bad_boundary.bytecode_offset = 1;
        assert_eq!(
            SafepointMetadata::new(3, 8, 8, vec![bad_boundary], vec![0, 2], vec![0, 2])
                .unwrap_err(),
            SafepointMetadataError::InvalidBytecodeOffset { safepoint_index: 0, offset: 1 }
        );

        assert_eq!(
            SafepointMetadata::new(3, 8, 8, vec![record], vec![0, 2], vec![0, 8]).unwrap_err(),
            SafepointMetadataError::InvalidInstructionStarts
        );

        let mut zero_native_pc = record;
        zero_native_pc.native_return_offset = 0;
        assert_eq!(
            SafepointMetadata::new(3, 8, 8, vec![zero_native_pc], vec![0, 2], vec![0]).unwrap_err(),
            SafepointMetadataError::InvalidNativeOffset { safepoint_index: 0, offset: 0 }
        );

        let mut duplicate_pc = record;
        duplicate_pc.bytecode_offset = 2;
        duplicate_pc.live_slot_start = 2;
        duplicate_pc.result_slot = 2;
        assert_eq!(
            SafepointMetadata::new(
                3,
                8,
                8,
                vec![record, duplicate_pc],
                vec![0, 2, 0, 1],
                vec![0, 2],
            )
            .unwrap_err(),
            SafepointMetadataError::DuplicateNativeOffset
        );
    }

    #[test]
    fn activation_registration_is_nested_lifo_and_unwind_safe() {
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let maps = metadata(vec![0]);
            let mut outer_slots = [JitSlot::undefined(); 3];
            let mut inner_slots = [JitSlot::undefined(); 3];
            let mut outer_frame = ShadowFrameOwner::new(&mut outer_slots, &maps).unwrap();
            let mut inner_frame = ShadowFrameOwner::new(&mut inner_slots, &maps).unwrap();
            let (mut outer_budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let (mut inner_budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());

            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let mut outer =
                    ActivationOwner::new(context, &mut outer_frame, &mut outer_budget).unwrap();
                assert!(outer.context.has_registered_jit_frame());
                outer.publish_for_test(0, 0);
                {
                    let mut inner =
                        ActivationOwner::new(outer.context, &mut inner_frame, &mut inner_budget)
                            .unwrap();
                    inner.publish_for_test(0, 0);
                    panic!("exercise nested activation cleanup");
                }
            }));
            assert!(result.is_err());
            assert!(!context.has_registered_jit_frame());
        });
    }

    #[test]
    fn final_native_backedge_unit_is_polled_before_hard_cap_side_exit() {
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let maps = metadata(vec![]);
            let mut slots = [JitSlot::undefined(); 3];
            {
                let mut frame = ShadowFrameOwner::new(&mut slots, &maps).unwrap();
                let (mut budget, _) =
                    DeterministicInterruptBudget::new(NonZeroU32::new(2).unwrap());
                let mut activation =
                    ActivationOwner::new(context, &mut frame, &mut budget).unwrap();
                activation.raw.side_exit_offset = 2;
                // Generated code consumes the cap unit inline before entering this slow helper.
                activation.raw.native_backedge_work_remaining = 0;

                let (status, observation) =
                    with_test_backedge_poll_behavior(TestBackedgePollBehavior::Normal, || {
                        // SAFETY: This is the exact live activation and private versioned helper
                        // used by generated code. The frame is quiescent and target 2 is an
                        // instruction.
                        unsafe { backedge_poll_helper(activation.as_mut_ptr()) }
                    });
                assert_eq!(status, HELPER_STATUS_SIDE_EXIT);
                assert_eq!(observation.calls, 1);
                assert_eq!(activation.raw.native_backedge_work_remaining, 0);
                drop(activation);
                assert_eq!(budget.remaining(), 1, "the hard-cap edge consumed one work unit");
            }

            let mut frame = ShadowFrameOwner::new(&mut slots, &maps).unwrap();
            let (mut interrupt_budget, _) =
                DeterministicInterruptBudget::new(NonZeroU32::new(1).unwrap());
            let mut activation =
                ActivationOwner::new(context, &mut frame, &mut interrupt_budget).unwrap();
            activation.raw.side_exit_offset = 2;
            activation.raw.native_backedge_work_remaining = 0;
            // SAFETY: Same exact live private helper contract as above.
            let status = unsafe { backedge_poll_helper(activation.as_mut_ptr()) };
            assert_eq!(
                status, HELPER_STATUS_INTERRUPTED,
                "ordinary interruption has priority when the hard cap expires on the same edge"
            );
            assert_eq!(activation.raw.native_backedge_work_remaining, 0);
        });
    }

    fn allocated_string_slot(context: &mut JitContextScope<'_>, contents: &str) -> JitSlot {
        let mut raw = context.raw();
        let guard = HandleScopeGuard::new(raw);
        let string = match raw.alloc_string(contents) {
            Ok(string) => string,
            Err(_) => panic!("test string allocation failed"),
        };
        let slot = JitSlot::try_from_value(context, *string.as_value()).unwrap();
        drop(guard);
        slot
    }

    fn assert_link_rejected(
        context: &mut JitContextScope<'_>,
        invalid: JitSlot,
        expected: JitSlotError,
    ) {
        let maps = metadata(vec![]);
        let mut slots = [JitSlot::undefined(); 3];
        slots[0] = invalid;
        let mut frame = ShadowFrameOwner::new(&mut slots, &maps).unwrap();
        let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
        let result = ActivationOwner::new(context, &mut frame, &mut budget);
        let rejected = matches!(
            &result,
            Err(ActivationCreateError::InvalidSlots(
                JitSlotValidationError::InvalidSlot { index: 0, error }
            )) if *error == expected
        );
        drop(result);
        assert!(rejected);
        assert!(context.raw().jit_frame_head().is_null());
    }

    #[test]
    fn checked_slot_api_rejects_forged_and_noncanonical_words_before_link() {
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let forged_pointer = Value::from_raw_bits(1);
            assert_eq!(
                JitSlot::try_from_value(context, forged_pointer).err(),
                Some(JitSlotValidationError::InvalidSlot {
                    index: 0,
                    error: JitSlotError::PointerIsNotAllocatedItemStart,
                })
            );

            let noncanonical_bool = Value::from_raw_bits(Value::bool(false).as_raw_bits() | 0b10);
            assert_eq!(
                JitSlot::try_from_value(context, noncanonical_bool).err(),
                Some(JitSlotValidationError::InvalidSlot {
                    index: 0,
                    error: JitSlotError::NonCanonicalImmediate,
                })
            );

            let valid = allocated_string_slot(context, "exact allocation start");
            let interior = Value::from_raw_bits(valid.value().as_raw_bits() + 8);
            assert_eq!(
                JitSlot::try_from_value(context, interior).err(),
                Some(JitSlotValidationError::InvalidSlot {
                    index: 0,
                    error: JitSlotError::PointerIsNotAllocatedItemStart,
                })
            );

            // The tuple field and raw mutation path are private. This type assertion records the
            // sole checked general constructor shape; zero cannot be represented as a valid
            // `Value`, and no `Default`, `From<u64>`, raw setter, or mutable raw accessor exists.
            let _: fn(&JitContextScope<'_>, Value) -> Result<JitSlot, JitSlotValidationError> =
                JitSlot::try_from_value;

            // Internal adversarial injection proves link-time validation independently of the
            // checked constructor and, critically, before frame-head publication.
            assert_link_rejected(
                context,
                JitSlot(forged_pointer),
                JitSlotError::PointerIsNotAllocatedItemStart,
            );
            assert_link_rejected(
                context,
                JitSlot(noncanonical_bool),
                JitSlotError::NonCanonicalImmediate,
            );
            assert_link_rejected(
                context,
                JitSlot(interior),
                JitSlotError::PointerIsNotAllocatedItemStart,
            );
        });
    }

    #[test]
    fn link_rejects_foreign_and_stale_moving_heap_slots_before_publication() {
        let mut foreign = ContextBuilder::new().build().unwrap();
        let mut foreign_slot = None;
        foreign.with_jit_context(|context| {
            foreign_slot = Some(allocated_string_slot(context, "foreign context"));
        });

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let foreign_slot = foreign_slot.unwrap();
            assert_eq!(
                JitSlot::try_from_value(context, foreign_slot.value()).err(),
                Some(JitSlotValidationError::InvalidSlot {
                    index: 0,
                    error: JitSlotError::PointerIsNotAllocatedItemStart,
                })
            );
            assert_link_rejected(
                context,
                foreign_slot,
                JitSlotError::PointerIsNotAllocatedItemStart,
            );

            let stale_slot = allocated_string_slot(context, "stale after moving collection");
            Heap::run_gc(context.raw(), GcType::Normal);
            assert_eq!(
                JitSlot::try_from_value(context, stale_slot.value()).err(),
                Some(JitSlotValidationError::InvalidSlot {
                    index: 0,
                    error: JitSlotError::PointerIsNotAllocatedItemStart,
                })
            );
            assert_link_rejected(context, stale_slot, JitSlotError::PointerIsNotAllocatedItemStart);
        });
    }

    #[test]
    fn returned_bits_must_be_canonical_and_belong_to_the_activation_context() {
        let mut foreign = ContextBuilder::new().build().unwrap();
        let mut foreign_slot = None;
        foreign.with_jit_context(|context| {
            foreign_slot = Some(allocated_string_slot(context, "foreign return"));
        });

        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let current_slot = allocated_string_slot(context, "current return");
            let maps = metadata(vec![]);
            let mut slots = [JitSlot::undefined(); 3];
            let mut frame = ShadowFrameOwner::new(&mut slots, &maps).unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let mut activation = ActivationOwner::new(context, &mut frame, &mut budget).unwrap();

            let noncanonical_bool = Value::from_raw_bits(Value::bool(false).as_raw_bits() | 0b10);
            activation.raw.return_value_bits = noncanonical_bool.as_raw_bits();
            assert_eq!(
                activation.validate_result(STATUS_RETURNED),
                Err(ActivationResultError::InvalidReturnValue(
                    JitSlotValidationError::InvalidSlot {
                        index: 0,
                        error: JitSlotError::NonCanonicalImmediate,
                    }
                ))
            );

            activation.raw.return_value_bits = foreign_slot.unwrap().value().as_raw_bits();
            assert_eq!(
                activation.validate_result(STATUS_RETURNED),
                Err(ActivationResultError::InvalidReturnValue(
                    JitSlotValidationError::InvalidSlot {
                        index: 0,
                        error: JitSlotError::PointerIsNotAllocatedItemStart,
                    }
                ))
            );

            activation.raw.return_value_bits = current_slot.value().as_raw_bits();
            assert_eq!(
                activation.validate_result(STATUS_RETURNED),
                Ok(ActivationOutcome::Returned(current_slot.value().as_raw_bits()))
            );
            activation.raw.return_value_bits = Value::raw_smi(7).as_raw_bits();
            assert_eq!(
                activation.validate_result(STATUS_RETURNED),
                Ok(ActivationOutcome::Returned(Value::raw_smi(7).as_raw_bits()))
            );
        });
    }

    #[test]
    fn late_result_validation_error_clears_every_caller_slot() {
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let maps = metadata(vec![]);
            let mut slots = [JitSlot(Value::raw_smi(1)); 3];
            let mut frame = ShadowFrameOwner::new(&mut slots, &maps).unwrap();
            let (mut budget, _) = DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
            let mut activation = ActivationOwner::new(context, &mut frame, &mut budget).unwrap();

            activation.raw.return_value_bits = Value::raw_smi(7).as_raw_bits();
            activation.raw.side_exit_offset = 2;
            assert_eq!(
                activation.validate_result(STATUS_RETURNED),
                Err(ActivationResultError::UnexpectedSideExitOffset)
            );
            assert!(
                activation
                    .frame
                    .slots
                    .iter()
                    .all(|slot| *slot == JitSlot::undefined())
            );
        });
    }

    #[test]
    fn rooted_slot_count_mismatch_clears_the_entire_destination() {
        let mut owned = ContextBuilder::new().build().unwrap();
        owned.with_jit_context(|context| {
            let raw = context.raw();
            let roots = RootedSlotSet {
                roots: vec![
                    Value::raw_smi(7).to_handle(raw),
                    Value::raw_smi(8).to_handle(raw),
                ],
                _brand: PhantomData,
            };
            let mut slots = [JitSlot(Value::raw_smi(1)); 3];

            assert_eq!(
                roots.sync_to_slots(&mut slots),
                Err(RootedSlotSyncError::CountMismatch { actual: 3, expected: 2 })
            );
            assert!(slots.iter().all(|slot| *slot == JitSlot::undefined()));
        });
    }

    #[test]
    fn abi_layout_is_fixed_for_generated_code() {
        assert_eq!(GENERATED_CODE_ABI_VERSION, 4);
        assert_eq!(MAX_NATIVE_BACKEDGE_WORK_UNITS, 1_000_000);
        assert_eq!(ACTIVATION_ABI_VERSION_OFFSET, 0);
        assert_eq!(ACTIVATION_STRUCT_SIZE_OFFSET, 4);
        assert_eq!(ACTIVATION_FRAME_OFFSET, 8);
        assert_eq!(ACTIVATION_HELPERS_OFFSET, 16);
        assert_eq!(ACTIVATION_CONTEXT_OFFSET, 24);
        assert_eq!(ACTIVATION_INTERRUPT_BUDGET_OFFSET, 32);
        assert_eq!(ACTIVATION_POLL_STATE_OFFSET, 40);
        assert_eq!(ACTIVATION_SIDE_EXIT_OFFSET, 48);
        assert_eq!(ACTIVATION_NATIVE_BACKEDGE_WORK_REMAINING_OFFSET, 52);
        assert_eq!(ACTIVATION_RETURN_VALUE_OFFSET, 56);
        assert_eq!(ACTIVATION_POISONED_OFFSET, 64);
        assert_eq!(ACTIVATION_RESERVED_TAIL_OFFSET, 68);
        assert_eq!(SHADOW_FRAME_PREVIOUS_OFFSET, 0);
        assert_eq!(SHADOW_FRAME_SLOTS_OFFSET, 8);
        assert_eq!(SHADOW_FRAME_SLOT_COUNT_OFFSET, 16);
        assert_eq!(SHADOW_FRAME_RECORDS_OFFSET, 24);
        assert_eq!(SHADOW_FRAME_RECORD_COUNT_OFFSET, 32);
        assert_eq!(SHADOW_FRAME_LIVE_SLOTS_OFFSET, 40);
        assert_eq!(SHADOW_FRAME_LIVE_SLOT_COUNT_OFFSET, 48);
        assert_eq!(SHADOW_FRAME_BYTECODE_OFFSET, 56);
        assert_eq!(SHADOW_FRAME_SAFEPOINT_INDEX_OFFSET, 60);
        assert_eq!(JIT_ACTIVATION_SIZE, 72);
        assert_eq!(HELPER_TABLE_BACKEDGE_POLL_OFFSET, 24);
        assert_eq!(JIT_HELPER_TABLE_SIZE, 32);
        assert_eq!(size_of::<JitSlot>(), size_of::<Value>());
        assert_eq!(align_of::<JitSlot>(), align_of::<Value>());
    }
}

//! Owner-thread executable-memory cache with a strict RW -> RX transition.

use std::{
    collections::VecDeque,
    marker::PhantomData,
    ptr::{self, NonNull},
    rc::Rc,
    thread::{self, ThreadId},
};

use super::{
    abi::{ActivationOwner, GeneratedEntry, SafepointMetadata},
    compiler::{PreparedProgram, PreparedPrototype, VmBindingId},
};

#[cfg(test)]
std::thread_local! {
    static TEST_LIVE_MAPPING_BYTES: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
fn test_mapping_opened(mapped_len: usize) {
    TEST_LIVE_MAPPING_BYTES.with(|bytes| {
        bytes.set(
            bytes
                .get()
                .checked_add(mapped_len)
                .expect("test mapping byte overflow"),
        );
    });
}

#[cfg(test)]
fn test_mapping_closed(mapped_len: usize) {
    TEST_LIVE_MAPPING_BYTES.with(|bytes| {
        bytes.set(
            bytes
                .get()
                .checked_sub(mapped_len)
                .expect("test mapping byte underflow"),
        );
    });
}

#[cfg(test)]
fn test_live_mapping_bytes() -> usize {
    TEST_LIVE_MAPPING_BYTES.with(std::cell::Cell::get)
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CodeMemoryError {
    EmptyCode,
    CapacityIsZero,
    EntryLimitIsZero,
    SizeOverflow,
    CodeTooLarge { mapped_len: usize, capacity: usize },
    MappingFailed(i32),
    ProtectFailed(i32),
    PageSizeUnavailable { result: i64, errno: i32 },
    WrongOwnerThread,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtectionPhase {
    ReadWrite,
    ReadExecute,
}

struct ExecutableMemory {
    ptr: NonNull<u8>,
    code_len: usize,
    mapped_len: usize,
    owner: ThreadId,
    // Executable code and its entry pointers are deliberately thread-affine in this first gate.
    _not_send_or_sync: PhantomData<Rc<()>>,
}

struct MappingGuard {
    ptr: NonNull<u8>,
    mapped_len: usize,
    armed: bool,
}

impl Drop for MappingGuard {
    fn drop(&mut self) {
        if self.armed {
            unmap_or_abort(self.ptr, self.mapped_len);
            #[cfg(test)]
            test_mapping_closed(self.mapped_len);
        }
    }
}

impl ExecutableMemory {
    #[cfg(test)]
    fn from_bytes(code: &[u8]) -> Result<Self, CodeMemoryError> {
        Self::from_bytes_observed(code, |_, _| {})
    }

    fn from_bytes_observed(
        code: &[u8],
        mut observe: impl FnMut(NonNull<u8>, ProtectionPhase),
    ) -> Result<Self, CodeMemoryError> {
        let mapped_len = Self::mapped_len_for(code.len())?;

        // SAFETY: Arguments request a private anonymous non-executable mapping. MAP_FAILED is
        // checked before the result is converted to NonNull.
        let raw = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mapped_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(CodeMemoryError::MappingFailed(last_errno()));
        }
        let ptr = NonNull::new(raw.cast::<u8>()).expect("mmap returned null instead of MAP_FAILED");
        #[cfg(test)]
        test_mapping_opened(mapped_len);
        let mut guard = MappingGuard { ptr, mapped_len, armed: true };
        observe(ptr, ProtectionPhase::ReadWrite);

        // SAFETY: The fresh mapping is writable for `mapped_len` bytes and `code.len()` is no
        // larger than that rounded allocation.
        unsafe { ptr::copy_nonoverlapping(code.as_ptr(), ptr.as_ptr(), code.len()) };

        // SAFETY: The pointer and length exactly identify the live mmap allocation. This removes
        // write permission before adding execute permission; no state is ever RWX.
        let protect_result = unsafe {
            libc::mprotect(ptr.as_ptr().cast(), mapped_len, libc::PROT_READ | libc::PROT_EXEC)
        };
        if protect_result != 0 {
            return Err(CodeMemoryError::ProtectFailed(last_errno()));
        }
        observe(ptr, ProtectionPhase::ReadExecute);
        guard.armed = false;

        Ok(Self {
            ptr,
            code_len: code.len(),
            mapped_len,
            owner: thread::current().id(),
            _not_send_or_sync: PhantomData,
        })
    }

    fn mapped_len_for(code_len: usize) -> Result<usize, CodeMemoryError> {
        if code_len == 0 {
            return Err(CodeMemoryError::EmptyCode);
        }
        code_len
            .checked_next_multiple_of(page_size()?)
            .ok_or(CodeMemoryError::SizeOverflow)
    }

    const fn code_len(&self) -> usize {
        self.code_len
    }

    const fn mapped_len(&self) -> usize {
        self.mapped_len
    }

    fn start_address(&self) -> usize {
        self.ptr.as_ptr() as usize
    }

    /// Call the start of a finalized RX mapping while retaining the executable-memory borrow for
    /// the complete activation. No bare entry pointer can escape and later outlive cache eviction.
    ///
    /// # Safety
    ///
    /// The bytes must encode a function with exactly `GeneratedEntry`'s SysV C ABI and must obey
    /// every shadow-frame/rooting invariant documented by the JIT ABI. This branded call is the
    /// only normal entry contract; passing an arbitrary non-null activation address directly to a
    /// raw `GeneratedEntry` would be undefined before generated header validation can reject it.
    unsafe fn call(
        &self,
        activation: &mut ActivationOwner<'_, '_, '_, '_, '_, '_>,
    ) -> Result<u32, CodeMemoryError> {
        self.ensure_owner()?;
        // SAFETY: The caller establishes that this executable address has the stated function ABI.
        let entry: GeneratedEntry = unsafe { std::mem::transmute(self.ptr.as_ptr()) };
        // SAFETY: The activation owner keeps the branded frame and slots alive through this
        // synchronous call. The generated bytes' ABI and invariants remain the caller's proof.
        Ok(unsafe { entry(activation.as_mut_ptr()) })
    }

    fn ensure_owner(&self) -> Result<(), CodeMemoryError> {
        if self.owner != thread::current().id() {
            return Err(CodeMemoryError::WrongOwnerThread);
        }
        Ok(())
    }
}

impl Drop for ExecutableMemory {
    fn drop(&mut self) {
        debug_assert_eq!(self.owner, thread::current().id());
        unmap_or_abort(self.ptr, self.mapped_len);
        #[cfg(test)]
        test_mapping_closed(self.mapped_len);
    }
}

/// One inseparable loaded artifact: RX bytes and the exact maps/program that produced them.
///
/// Construction is private to this module and consumes a compiler-created `PreparedPrototype`.
/// A borrow returned by the cache prevents eviction for the whole synchronous execution.
pub(crate) struct LoadedPrototype {
    code: ExecutableMemory,
    prepared: PreparedPrototype,
}

impl LoadedPrototype {
    pub(crate) const fn required_frame_slots(&self) -> usize {
        self.prepared.required_frame_slots()
    }

    pub(crate) fn safepoints(&self) -> &SafepointMetadata {
        self.prepared.safepoints()
    }

    pub(crate) fn program(&self) -> &PreparedProgram {
        self.prepared.program()
    }

    pub(in crate::runtime::jit) fn is_vm_bound(&self) -> bool {
        self.prepared.is_vm_bound()
    }

    pub(in crate::runtime::jit) fn is_bound_to_vm(&self, binding_id: VmBindingId) -> bool {
        self.prepared.is_bound_to_vm(binding_id)
    }

    pub(crate) const fn code_len(&self) -> usize {
        self.code.code_len()
    }

    #[cfg(test)]
    pub(crate) fn start_address_for_test(&self) -> usize {
        self.code.start_address()
    }

    /// Enter the exact generated bytes owned by this loaded artifact.
    ///
    /// # Safety
    ///
    /// `activation` must have been built from this same artifact's `safepoints()` and slot count.
    /// The safe contained runner establishes that invariant; direct callers are test-only ABI
    /// probes and must retain this borrow for the complete synchronous call.
    pub(crate) unsafe fn call(
        &self,
        activation: &mut ActivationOwner<'_, '_, '_, '_, '_, '_>,
    ) -> Result<u32, CodeMemoryError> {
        // SAFETY: Required by this method's contract. `self` inseparably owns the exact compiled
        // bytes while its prepared metadata/program remain borrowed with them.
        unsafe { self.code.call(activation) }
    }
}

struct CacheEntry {
    key: u64,
    last_used: u64,
    loaded: LoadedPrototype,
}

pub(crate) struct ExecutableCodeCache {
    owner: ThreadId,
    capacity_bytes: usize,
    max_entries: usize,
    used_bytes: usize,
    clock: u64,
    entries: VecDeque<CacheEntry>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl ExecutableCodeCache {
    pub(crate) fn new(capacity_bytes: usize, max_entries: usize) -> Result<Self, CodeMemoryError> {
        if capacity_bytes == 0 {
            return Err(CodeMemoryError::CapacityIsZero);
        }
        if max_entries == 0 {
            return Err(CodeMemoryError::EntryLimitIsZero);
        }
        Ok(Self {
            owner: thread::current().id(),
            capacity_bytes,
            max_entries,
            used_bytes: 0,
            clock: 0,
            entries: VecDeque::new(),
            _not_send_or_sync: PhantomData,
        })
    }

    pub(crate) fn mapped_len_for(prepared: &PreparedPrototype) -> Result<usize, CodeMemoryError> {
        ExecutableMemory::mapped_len_for(prepared.machine_code().len())
    }

    /// Consume and load one compiler-prepared artifact. Arbitrary byte slices are never accepted.
    pub(crate) fn insert(
        &mut self,
        key: u64,
        prepared: PreparedPrototype,
    ) -> Result<(), CodeMemoryError> {
        self.insert_observed(key, prepared, |_, _| {})
    }

    /// Insert while exposing protection transitions to tests. Existing entries are removed or
    /// evicted before `mmap`, so the configured capacity is a hard bound on live mappings at every
    /// instant. Consequently, an mmap/mprotect failure may leave prior LRU evictions in effect.
    fn insert_observed(
        &mut self,
        key: u64,
        prepared: PreparedPrototype,
        observe: impl FnMut(NonNull<u8>, ProtectionPhase),
    ) -> Result<(), CodeMemoryError> {
        self.ensure_owner()?;
        let mapped_len = Self::mapped_len_for(&prepared)?;
        if mapped_len > self.capacity_bytes {
            return Err(CodeMemoryError::CodeTooLarge {
                mapped_len,
                capacity: self.capacity_bytes,
            });
        }

        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            let old = self.entries.remove(index).expect("entry index was found");
            self.used_bytes = self
                .used_bytes
                .checked_sub(old.loaded.code.mapped_len())
                .expect("cache byte accounting underflow");
        }

        while self.entries.len() >= self.max_entries
            || self
                .used_bytes
                .checked_add(mapped_len)
                .ok_or(CodeMemoryError::SizeOverflow)?
                > self.capacity_bytes
        {
            self.evict_lru();
        }

        // Room is established before the new writable mapping exists. This keeps the mapped-byte
        // budget hard during both the RW staging phase and the final RX phase.
        let code = ExecutableMemory::from_bytes_observed(prepared.machine_code(), observe)?;
        let loaded = LoadedPrototype { code, prepared };

        self.clock = self.clock.wrapping_add(1);
        self.used_bytes = self
            .used_bytes
            .checked_add(loaded.code.mapped_len())
            .ok_or(CodeMemoryError::SizeOverflow)?;
        self.entries
            .push_back(CacheEntry { key, last_used: self.clock, loaded });
        Ok(())
    }

    pub(crate) fn get(&mut self, key: u64) -> Result<Option<&LoadedPrototype>, CodeMemoryError> {
        self.ensure_owner()?;
        let Some(index) = self.entries.iter().position(|entry| entry.key == key) else {
            return Ok(None);
        };
        self.clock = self.clock.wrapping_add(1);
        self.entries[index].last_used = self.clock;
        Ok(Some(&self.entries[index].loaded))
    }

    pub(crate) const fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn evict_lru(&mut self) {
        let index = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(index, _)| index)
            .expect("capacity loop only evicts a nonempty cache");
        let entry = self.entries.remove(index).expect("LRU index is in bounds");
        self.used_bytes -= entry.loaded.code.mapped_len();
    }

    fn ensure_owner(&self) -> Result<(), CodeMemoryError> {
        if self.owner != thread::current().id() {
            return Err(CodeMemoryError::WrongOwnerThread);
        }
        Ok(())
    }
}

fn page_size() -> Result<usize, CodeMemoryError> {
    // SAFETY: `_SC_PAGESIZE` has no pointer arguments or side effects.
    let result = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if result <= 0 {
        return Err(CodeMemoryError::PageSizeUnavailable { result, errno: last_errno() });
    }
    usize::try_from(result)
        .map_err(|_| CodeMemoryError::PageSizeUnavailable { result, errno: last_errno() })
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn unmap_or_abort(ptr: NonNull<u8>, mapped_len: usize) {
    // SAFETY: Both owners call this exactly once with the unchanged base and length returned by a
    // successful mmap. Continuing after an impossible teardown failure would violate the cache's
    // hard live-byte accounting, so release builds fail closed as well.
    let result = unsafe { libc::munmap(ptr.as_ptr().cast(), mapped_len) };
    if result != 0 {
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::runtime::{
        ContextBuilder, Value,
        bytecode::{
            instruction::OpCode,
            verifier::{ConstantKind, VerificationLimits, VerifiedBytecode},
        },
        jit::{
            abi::JitSlot,
            compiler::compile_prototype,
            continuation::{ContainedOutcome, run_unbound_native_for_test},
            hotness::DeterministicInterruptBudget,
        },
    };

    fn prepared_returning(value: i8) -> PreparedPrototype {
        let local_zero = (-1_i8) as u8;
        let bytes = [
            OpCode::LoadImmediate as u8,
            local_zero,
            value as u8,
            OpCode::Ret as u8,
            local_zero,
        ];
        let verified = VerifiedBytecode::verify(&bytes, VerificationLimits::empty(1, 0)).unwrap();
        compile_prototype(&verified).unwrap()
    }

    fn prepared_constant_jump(target: isize) -> PreparedPrototype {
        let local_zero = (-1_i8) as u8;
        let bytes = [
            OpCode::JumpConstant as u8,
            0,
            OpCode::LoadImmediate as u8,
            local_zero,
            11,
            OpCode::Ret as u8,
            local_zero,
            OpCode::LoadImmediate as u8,
            local_zero,
            22,
            OpCode::Ret as u8,
            local_zero,
        ];
        let constants = [ConstantKind::JumpOffset(target)];
        let mut limits = VerificationLimits::empty(1, 0);
        limits.constants = &constants;
        let verified = VerifiedBytecode::verify(&bytes, limits).unwrap();
        compile_prototype(&verified).unwrap()
    }

    fn mapping_permissions(address: usize) -> Option<String> {
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        maps.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            let range = fields.next()?;
            let permissions = fields.next()?;
            let (start, end) = range.split_once('-')?;
            let start = usize::from_str_radix(start, 16).ok()?;
            let end = usize::from_str_radix(end, 16).ok()?;
            (start <= address && address < end).then(|| permissions.to_owned())
        })
    }

    #[test]
    fn mapping_transitions_rw_to_rx_and_is_never_rwx() {
        let mut phases = Vec::new();
        let code = ExecutableMemory::from_bytes_observed(&[0xc3], |ptr, phase| {
            let permissions = mapping_permissions(ptr.as_ptr() as usize).unwrap();
            assert!(!(permissions.contains('w') && permissions.contains('x')));
            phases.push((phase, permissions));
        })
        .unwrap();

        assert_eq!(phases[0], (ProtectionPhase::ReadWrite, "rw-p".to_owned()));
        assert_eq!(phases[1], (ProtectionPhase::ReadExecute, "r-xp".to_owned()));
        assert_eq!(mapping_permissions(code.start_address()).as_deref(), Some("r-xp"));
    }

    #[test]
    fn observer_panic_still_unmaps_the_guarded_mapping() {
        assert_eq!(test_live_mapping_bytes(), 0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = ExecutableMemory::from_bytes_observed(&[0xc3], |_, _| {
                assert_eq!(test_live_mapping_bytes(), page_size().unwrap());
                panic!("test observer panic");
            });
        }));
        assert!(result.is_err());
        assert_eq!(test_live_mapping_bytes(), 0);
    }

    #[test]
    fn cache_enforces_capacity_lru_eviction_and_mapping_accounting() {
        let page = page_size().unwrap();
        assert_eq!(test_live_mapping_bytes(), 0);
        {
            let mut cache = ExecutableCodeCache::new(page * 2, 2).unwrap();
            cache.insert(1, prepared_returning(1)).unwrap();
            cache.insert(2, prepared_returning(2)).unwrap();
            let first_address = cache.get(1).unwrap().unwrap().start_address_for_test();
            assert_eq!(test_live_mapping_bytes(), page * 2);

            // Refresh key 1, then inserting key 3 must evict key 2. An address-level post-eviction
            // check would race with immediate virtual-address reuse by another parallel test, so
            // use the accounting updated only after successful `munmap`.
            cache.get(1).unwrap();
            cache.insert(3, prepared_returning(3)).unwrap();
            assert_eq!(cache.len(), 2);
            assert!(cache.get(1).unwrap().is_some());
            assert!(cache.get(2).unwrap().is_none());
            assert!(cache.get(3).unwrap().is_some());
            assert_eq!(cache.used_bytes(), page * 2);
            assert_eq!(test_live_mapping_bytes(), page * 2);
            assert!(mapping_permissions(first_address).is_some());
        }
        assert_eq!(test_live_mapping_bytes(), 0);
    }

    #[test]
    fn drop_releases_executable_mapping_accounting() {
        assert_eq!(test_live_mapping_bytes(), 0);
        {
            let code = ExecutableMemory::from_bytes(&[0xc3]).unwrap();
            let address = code.start_address();
            assert_eq!(mapping_permissions(address).as_deref(), Some("r-xp"));
            assert_eq!(test_live_mapping_bytes(), code.mapped_len());
        }
        // Another parallel test may immediately reuse the same virtual address, so `/proc/maps`
        // cannot reliably prove teardown after the owner drops. `unmap_or_abort` fails closed if
        // `munmap` fails, and this per-thread accounting is decremented only after that succeeds.
        assert_eq!(test_live_mapping_bytes(), 0);
    }

    #[test]
    fn limits_reject_empty_oversized_and_zero_capacity_inputs() {
        assert_eq!(ExecutableMemory::from_bytes(&[]).err(), Some(CodeMemoryError::EmptyCode));
        assert!(matches!(
            ExecutableCodeCache::new(0, 1).err(),
            Some(CodeMemoryError::CapacityIsZero)
        ));
        assert!(matches!(
            ExecutableCodeCache::new(page_size().unwrap(), 0).err(),
            Some(CodeMemoryError::EntryLimitIsZero)
        ));

        let page = page_size().unwrap();
        let mut cache = ExecutableCodeCache::new(page - 1, 1).unwrap();
        assert!(matches!(
            cache.insert(1, prepared_returning(1)),
            Err(CodeMemoryError::CodeTooLarge { .. })
        ));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_never_transiently_exceeds_its_live_mapping_budget() {
        let page = page_size().unwrap();
        let capacity = page * 2;
        assert_eq!(test_live_mapping_bytes(), 0);
        {
            let mut cache = ExecutableCodeCache::new(capacity, 2).unwrap();
            cache.insert(1, prepared_returning(1)).unwrap();
            cache.insert(2, prepared_returning(2)).unwrap();
            assert_eq!(test_live_mapping_bytes(), capacity);

            let peak = std::cell::Cell::new(0_usize);
            cache
                .insert_observed(3, prepared_returning(3), |_, _| {
                    let mapped = test_live_mapping_bytes();
                    peak.set(peak.get().max(mapped));
                    assert!(mapped <= capacity);
                })
                .unwrap();

            assert_eq!(peak.get(), capacity);
            assert_eq!(cache.used_bytes(), capacity);
        }
        assert_eq!(test_live_mapping_bytes(), 0);
    }

    #[test]
    fn same_bytes_with_distinct_resolved_branches_remain_inseparably_bound() {
        let prepared_a = prepared_constant_jump(2);
        let prepared_b = prepared_constant_jump(7);
        assert_eq!(prepared_a.program().bytes(), prepared_b.program().bytes());
        assert_eq!(prepared_a.program().instructions()[0].branch_target, Some(2));
        assert_eq!(prepared_b.program().instructions()[0].branch_target, Some(7));

        let page = page_size().unwrap();
        let mut cache = ExecutableCodeCache::new(page * 2, 2).unwrap();
        cache.insert(1, prepared_a).unwrap();
        cache.insert(2, prepared_b).unwrap();

        let target_a = cache.get(1).unwrap().unwrap().program().instructions()[0].branch_target;
        let target_b = cache.get(2).unwrap().unwrap().program().instructions()[0].branch_target;
        assert_eq!(target_a, Some(2));
        assert_eq!(target_b, Some(7));

        let mut owned = ContextBuilder::new().build().unwrap();
        for (key, expected) in [(1, 11), (2, 22)] {
            let mut outcome_bits = None;
            owned.with_jit_context(|context| {
                let loaded = cache.get(key).unwrap().unwrap();
                let mut slots = [JitSlot::undefined()];
                let (mut budget, _) =
                    DeterministicInterruptBudget::new(NonZeroU32::new(100).unwrap());
                let outcome =
                    run_unbound_native_for_test(context, loaded, &mut slots, &mut budget).unwrap();
                outcome_bits = Some(match outcome {
                    ContainedOutcome::NativeReturned(value) => {
                        value.bits_for_test(context).unwrap()
                    }
                    other => panic!("expected native return, got {other:?}"),
                });
            });
            assert_eq!(outcome_bits.unwrap(), Value::raw_smi(expected).as_raw_bits());
        }

        let _: fn(&mut ExecutableCodeCache, u64, PreparedPrototype) -> Result<(), CodeMemoryError> =
            ExecutableCodeCache::insert;
    }
}

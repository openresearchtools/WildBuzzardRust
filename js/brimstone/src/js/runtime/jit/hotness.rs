//! Deterministic hotness and interrupt policy primitives.
//!
//! These counters feed the bounded baseline dispatcher, whose product admission remains
//! compile-time false. Reaching a threshold can compile/dispatch only under the test policy.

use std::{
    marker::PhantomData,
    mem::{align_of, size_of},
    num::NonZeroU32,
    ptr,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    thread::{self, ThreadId},
};

/// Version of the stable C-layout state read directly by generated backedge fast paths.
pub(crate) const INLINE_POLL_STATE_ABI_VERSION: u32 = 1;

/// Stable, shared state for deterministic generated-code polling.
///
/// The allocation is owned jointly by the owner-thread budget and its request handles, so its
/// address cannot change during a generated activation. Generated code uses atomic accesses for
/// both fields it touches: `requested` is written from other threads, while `remaining` is
/// owner-thread-only but still atomic so native loads/stores and Rust accesses obey one memory
/// model. Header and reserved fields are immutable after construction.
#[repr(C)]
pub(crate) struct InlinePollState {
    abi_version: u32,
    struct_size: u32,
    quantum: u32,
    remaining: AtomicU32,
    requested: AtomicU32,
    reserved_0: u32,
    reserved_1: u64,
}

pub(crate) const INLINE_POLL_STATE_ABI_VERSION_OFFSET: i32 =
    std::mem::offset_of!(InlinePollState, abi_version) as i32;
pub(crate) const INLINE_POLL_STATE_STRUCT_SIZE_OFFSET: i32 =
    std::mem::offset_of!(InlinePollState, struct_size) as i32;
pub(crate) const INLINE_POLL_STATE_QUANTUM_OFFSET: i32 =
    std::mem::offset_of!(InlinePollState, quantum) as i32;
pub(crate) const INLINE_POLL_STATE_REMAINING_OFFSET: i32 =
    std::mem::offset_of!(InlinePollState, remaining) as i32;
pub(crate) const INLINE_POLL_STATE_REQUESTED_OFFSET: i32 =
    std::mem::offset_of!(InlinePollState, requested) as i32;
pub(crate) const INLINE_POLL_STATE_RESERVED_0_OFFSET: i32 =
    std::mem::offset_of!(InlinePollState, reserved_0) as i32;
pub(crate) const INLINE_POLL_STATE_RESERVED_1_OFFSET: i32 =
    std::mem::offset_of!(InlinePollState, reserved_1) as i32;
pub(crate) const INLINE_POLL_STATE_SIZE: u32 = size_of::<InlinePollState>() as u32;

const _: () = {
    assert!(size_of::<AtomicU32>() == size_of::<u32>());
    assert!(align_of::<AtomicU32>() == align_of::<u32>());
    assert!(size_of::<InlinePollState>() == 32);
    assert!(align_of::<InlinePollState>() == align_of::<u64>());
};

impl InlinePollState {
    fn new(quantum: NonZeroU32) -> Self {
        Self {
            abi_version: INLINE_POLL_STATE_ABI_VERSION,
            struct_size: INLINE_POLL_STATE_SIZE,
            quantum: quantum.get(),
            remaining: AtomicU32::new(quantum.get()),
            requested: AtomicU32::new(0),
            reserved_0: 0,
            reserved_1: 0,
        }
    }

    fn validate(&self) -> bool {
        let remaining = self.remaining.load(Ordering::SeqCst);
        let requested = self.requested.load(Ordering::Acquire);
        self.abi_version == INLINE_POLL_STATE_ABI_VERSION
            && self.struct_size == INLINE_POLL_STATE_SIZE
            && self.quantum != 0
            && remaining != 0
            && remaining <= self.quantum
            && requested <= 1
            && self.reserved_0 == 0
            && self.reserved_1 == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HotnessThresholds {
    pub(crate) calls: NonZeroU32,
    pub(crate) backedges: NonZeroU32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HotnessDecision {
    Cold,
    BecameHot,
    AlreadyHot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JitPolicyError {
    WrongOwnerThread,
    InvalidInlinePollState,
}

/// Owner-thread counters for one bytecode function. Both counters saturate, so long-running code
/// cannot wrap back to cold. A function becomes hot once when either configured threshold is met.
pub(crate) struct FunctionHotness {
    owner: ThreadId,
    thresholds: HotnessThresholds,
    calls: u32,
    backedges: u32,
    is_hot: bool,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl FunctionHotness {
    pub(crate) fn new(thresholds: HotnessThresholds) -> Self {
        Self {
            owner: thread::current().id(),
            thresholds,
            calls: 0,
            backedges: 0,
            is_hot: false,
            _not_send_or_sync: PhantomData,
        }
    }

    pub(crate) fn record_call(&mut self) -> Result<HotnessDecision, JitPolicyError> {
        self.ensure_owner()?;
        self.calls = self.calls.saturating_add(1);
        Ok(self.update_hotness())
    }

    pub(crate) fn record_backedge(&mut self) -> Result<HotnessDecision, JitPolicyError> {
        self.ensure_owner()?;
        self.backedges = self.backedges.saturating_add(1);
        Ok(self.update_hotness())
    }

    pub(crate) const fn calls(&self) -> u32 {
        self.calls
    }

    pub(crate) const fn backedges(&self) -> u32 {
        self.backedges
    }

    pub(crate) const fn is_hot(&self) -> bool {
        self.is_hot
    }

    fn update_hotness(&mut self) -> HotnessDecision {
        if self.is_hot {
            return HotnessDecision::AlreadyHot;
        }
        if self.calls >= self.thresholds.calls.get()
            || self.backedges >= self.thresholds.backedges.get()
        {
            self.is_hot = true;
            HotnessDecision::BecameHot
        } else {
            HotnessDecision::Cold
        }
    }

    fn ensure_owner(&self) -> Result<(), JitPolicyError> {
        if self.owner != thread::current().id() {
            return Err(JitPolicyError::WrongOwnerThread);
        }
        Ok(())
    }
}

/// Cross-thread request handle. It owns no runtime, activation, code pointer, or moving heap
/// reference; it can only set an atomic bit consumed by the owner thread at an explicit poll.
#[derive(Clone)]
pub(crate) struct InterruptRequestHandle {
    state: Arc<InlinePollState>,
}

impl InterruptRequestHandle {
    pub(crate) fn request(&self) {
        self.state.requested.store(1, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct InterruptPoll {
    pub(crate) externally_requested: bool,
    pub(crate) quantum_expired: bool,
}

impl InterruptPoll {
    pub(crate) const fn is_due(self) -> bool {
        self.externally_requested || self.quantum_expired
    }
}

/// Owner-thread work-unit budget. Quantum expiry depends only on the exact number of consumed work
/// units, never wall-clock time, making identical bytecode/input schedules poll at identical
/// boundaries. External requests are reported independently so a simultaneous expiry is not lost.
pub(crate) struct DeterministicInterruptBudget {
    owner: ThreadId,
    state: Arc<InlinePollState>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl DeterministicInterruptBudget {
    pub(crate) fn new(quantum: NonZeroU32) -> (Self, InterruptRequestHandle) {
        let state = Arc::new(InlinePollState::new(quantum));
        let handle = InterruptRequestHandle { state: Arc::clone(&state) };
        (
            Self {
                owner: thread::current().id(),
                state,
                _not_send_or_sync: PhantomData,
            },
            handle,
        )
    }

    pub(crate) fn poll_after_work(
        &mut self,
        work_units: u32,
    ) -> Result<InterruptPoll, JitPolicyError> {
        self.ensure_owner()?;
        if !self.state.validate() {
            return Err(JitPolicyError::InvalidInlinePollState);
        }

        let externally_requested = self.state.requested.swap(0, Ordering::AcqRel) != 0;
        let remaining = self.state.remaining.load(Ordering::SeqCst);
        let quantum = self.state.quantum;
        let quantum_expired = if work_units < remaining {
            self.state
                .remaining
                .store(remaining - work_units, Ordering::SeqCst);
            false
        } else {
            let after_first_expiry = work_units - remaining;
            let remainder = after_first_expiry % quantum;
            let next_remaining = if remainder == 0 {
                quantum
            } else {
                quantum - remainder
            };
            self.state.remaining.store(next_remaining, Ordering::SeqCst);
            true
        };

        Ok(InterruptPoll { externally_requested, quantum_expired })
    }

    pub(crate) fn remaining(&self) -> u32 {
        self.state.remaining.load(Ordering::SeqCst)
    }

    pub(crate) fn inline_state_ptr(&self) -> *const InlinePollState {
        Arc::as_ptr(&self.state)
    }

    pub(crate) fn validate_inline_state_ptr(&self, state: *const InlinePollState) -> bool {
        ptr::eq(state, self.inline_state_ptr()) && self.state.validate()
    }

    #[cfg(test)]
    pub(crate) fn set_remaining_for_test(&self, remaining: u32) {
        self.state.remaining.store(remaining, Ordering::SeqCst);
    }

    fn ensure_owner(&self) -> Result<(), JitPolicyError> {
        if self.owner != thread::current().id() {
            return Err(JitPolicyError::WrongOwnerThread);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonzero(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    #[test]
    fn inline_poll_state_has_stable_c_layout_and_arc_address() {
        assert_eq!(INLINE_POLL_STATE_ABI_VERSION_OFFSET, 0);
        assert_eq!(INLINE_POLL_STATE_STRUCT_SIZE_OFFSET, 4);
        assert_eq!(INLINE_POLL_STATE_QUANTUM_OFFSET, 8);
        assert_eq!(INLINE_POLL_STATE_REMAINING_OFFSET, 12);
        assert_eq!(INLINE_POLL_STATE_REQUESTED_OFFSET, 16);
        assert_eq!(INLINE_POLL_STATE_RESERVED_0_OFFSET, 20);
        assert_eq!(INLINE_POLL_STATE_RESERVED_1_OFFSET, 24);
        assert_eq!(INLINE_POLL_STATE_SIZE, 32);

        let (mut budget, request) = DeterministicInterruptBudget::new(nonzero(7));
        let address = budget.inline_state_ptr();
        let cloned = request.clone();
        std::thread::spawn(move || cloned.request()).join().unwrap();
        assert_eq!(budget.inline_state_ptr(), address);
        assert!(budget.poll_after_work(1).unwrap().externally_requested);
        assert_eq!(budget.inline_state_ptr(), address);
    }

    #[test]
    fn malformed_zero_remaining_is_rejected_without_arithmetic() {
        let (mut budget, _) = DeterministicInterruptBudget::new(nonzero(7));
        budget.set_remaining_for_test(0);
        assert_eq!(budget.poll_after_work(1), Err(JitPolicyError::InvalidInlinePollState));
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn call_and_backedge_counters_become_hot_once_and_saturate() {
        let thresholds = HotnessThresholds { calls: nonzero(3), backedges: nonzero(2) };
        let mut calls = FunctionHotness::new(thresholds);
        assert_eq!(calls.record_call(), Ok(HotnessDecision::Cold));
        assert_eq!(calls.record_call(), Ok(HotnessDecision::Cold));
        assert_eq!(calls.record_call(), Ok(HotnessDecision::BecameHot));
        assert_eq!(calls.record_call(), Ok(HotnessDecision::AlreadyHot));
        assert!(calls.is_hot());
        assert_eq!(calls.calls(), 4);

        let mut backedges = FunctionHotness::new(thresholds);
        assert_eq!(backedges.record_backedge(), Ok(HotnessDecision::Cold));
        assert_eq!(backedges.record_backedge(), Ok(HotnessDecision::BecameHot));
        assert_eq!(backedges.backedges(), 2);

        backedges.calls = u32::MAX;
        backedges.backedges = u32::MAX;
        backedges.record_call().unwrap();
        backedges.record_backedge().unwrap();
        assert_eq!(backedges.calls(), u32::MAX);
        assert_eq!(backedges.backedges(), u32::MAX);
    }

    #[test]
    fn interrupt_quantum_is_deterministic_across_chunk_sizes() {
        let (mut one_by_one, _) = DeterministicInterruptBudget::new(nonzero(5));
        let mut expiries = Vec::new();
        for unit in 1..=12 {
            if one_by_one.poll_after_work(1).unwrap().quantum_expired {
                expiries.push(unit);
            }
        }
        assert_eq!(expiries, [5, 10]);
        assert_eq!(one_by_one.remaining(), 3);

        let (mut chunked, _) = DeterministicInterruptBudget::new(nonzero(5));
        assert!(!chunked.poll_after_work(4).unwrap().quantum_expired);
        assert!(chunked.poll_after_work(6).unwrap().quantum_expired);
        assert_eq!(chunked.remaining(), 5);
        assert!(!chunked.poll_after_work(2).unwrap().quantum_expired);
        assert_eq!(chunked.remaining(), 3);
    }

    #[test]
    fn external_request_and_quantum_expiry_are_both_reported_and_consumed() {
        let (mut budget, handle) = DeterministicInterruptBudget::new(nonzero(3));
        std::thread::spawn(move || handle.request()).join().unwrap();
        let poll = budget.poll_after_work(3).unwrap();
        assert_eq!(poll, InterruptPoll { externally_requested: true, quantum_expired: true });
        assert!(poll.is_due());

        assert_eq!(budget.poll_after_work(0).unwrap(), InterruptPoll::default());
    }
}

//! Deterministic hotness and interrupt policy primitives.
//!
//! These counters are infrastructure only: the interpreter is still the sole product execution
//! path, and reaching a threshold does not compile or dispatch generated code.

use std::{
    marker::PhantomData,
    num::NonZeroU32,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, ThreadId},
};

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
    requested: Arc<AtomicBool>,
}

impl InterruptRequestHandle {
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Release);
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
    quantum: NonZeroU32,
    remaining: u32,
    requested: Arc<AtomicBool>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl DeterministicInterruptBudget {
    pub(crate) fn new(quantum: NonZeroU32) -> (Self, InterruptRequestHandle) {
        let requested = Arc::new(AtomicBool::new(false));
        let handle = InterruptRequestHandle { requested: Arc::clone(&requested) };
        (
            Self {
                owner: thread::current().id(),
                quantum,
                remaining: quantum.get(),
                requested,
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

        let externally_requested = self.requested.swap(false, Ordering::AcqRel);
        let quantum_expired = if work_units < self.remaining {
            self.remaining -= work_units;
            false
        } else {
            let after_first_expiry = work_units - self.remaining;
            let remainder = after_first_expiry % self.quantum.get();
            self.remaining = if remainder == 0 {
                self.quantum.get()
            } else {
                self.quantum.get() - remainder
            };
            true
        };

        Ok(InterruptPoll { externally_requested, quantum_expired })
    }

    pub(crate) const fn remaining(&self) -> u32 {
        self.remaining
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

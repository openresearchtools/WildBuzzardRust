use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::event::{LinuxShutdownReport, LinuxStopReason, LinuxWindowEvent};
use crate::queue::{EventQueue, PushError};

const WAKE_IDLE: u8 = 0;
const WAKE_PENDING: u8 = 1;
const WAKE_CLOSED: u8 = 2;

/// Owner-thread lifecycle. The first transition out of `Running` is final.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShellLifecycle {
    Running,
    Stopping(LinuxStopReason),
    Exited(LinuxShutdownReport),
}

impl ShellLifecycle {
    pub(crate) const fn new() -> Self {
        Self::Running
    }

    pub(crate) const fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    /// Records the first stop cause. Later causes cannot replace it.
    pub(crate) fn begin_stopping(&mut self, reason: LinuxStopReason) -> bool {
        if self.is_running() {
            *self = Self::Stopping(reason);
            true
        } else {
            false
        }
    }

    pub(crate) const fn stop_reason(self) -> Option<LinuxStopReason> {
        match self {
            Self::Running => None,
            Self::Stopping(reason) => Some(reason),
            Self::Exited(report) => Some(report.reason),
        }
    }

    pub(crate) fn finish(&mut self, report: LinuxShutdownReport) -> bool {
        if matches!(self, Self::Stopping(_)) {
            *self = Self::Exited(report);
            true
        } else {
            false
        }
    }

    pub(crate) const fn report(self) -> Option<LinuxShutdownReport> {
        match self {
            Self::Exited(report) => Some(report),
            Self::Running | Self::Stopping(_) => None,
        }
    }
}

/// Pure owner-thread state machine coupling lifecycle and ordinary admission.
pub(crate) struct ShellState {
    lifecycle: ShellLifecycle,
    ordinary: EventQueue,
}

impl ShellState {
    pub(crate) fn new(event_capacity: usize) -> Self {
        Self {
            lifecycle: ShellLifecycle::new(),
            ordinary: EventQueue::new(event_capacity),
        }
    }

    pub(crate) const fn is_running(&self) -> bool {
        self.lifecycle.is_running()
    }

    pub(crate) fn push(&mut self, event: LinuxWindowEvent) -> Result<(), PushError> {
        self.ordinary.push(event)
    }

    pub(crate) fn pop(&mut self) -> Option<LinuxWindowEvent> {
        self.ordinary.pop()
    }

    /// Records the first reason and atomically seals ordinary admission.
    pub(crate) fn begin_stopping(&mut self, reason: LinuxStopReason) -> bool {
        if self.lifecycle.begin_stopping(reason) {
            self.ordinary.seal();
            true
        } else {
            false
        }
    }

    pub(crate) const fn stop_reason(&self) -> Option<LinuxStopReason> {
        self.lifecycle.stop_reason()
    }

    pub(crate) fn finish(&mut self, report: LinuxShutdownReport) -> bool {
        self.lifecycle.finish(report)
    }

    pub(crate) const fn report(&self) -> Option<LinuxShutdownReport> {
        self.lifecycle.report()
    }

    pub(crate) const fn coalesced(&self) -> u64 {
        self.ordinary.coalesced()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WakeAdmission {
    Admitted,
    AlreadyPending,
    Closed,
}

/// Atomic admission gate shared by the shell owner and every wake handle.
pub(crate) struct WakeGate {
    state: AtomicU8,
}

impl WakeGate {
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicU8::new(WAKE_IDLE),
        }
    }

    pub(crate) fn admit(&self) -> WakeAdmission {
        loop {
            match self.state.load(Ordering::Acquire) {
                WAKE_IDLE => {
                    if self
                        .state
                        .compare_exchange(
                            WAKE_IDLE,
                            WAKE_PENDING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return WakeAdmission::Admitted;
                    }
                }
                WAKE_PENDING => return WakeAdmission::AlreadyPending,
                WAKE_CLOSED => return WakeAdmission::Closed,
                _ => {
                    // Only this module writes the atomic. Treat any impossible
                    // value as permanently closed rather than reopening it.
                    self.close();
                    return WakeAdmission::Closed;
                }
            }
        }
    }

    /// Consumes one pending receipt. A closed gate never returns to idle.
    pub(crate) fn acknowledge(&self) -> bool {
        self.state
            .compare_exchange(WAKE_PENDING, WAKE_IDLE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn close(&self) {
        self.state.store(WAKE_CLOSED, Ordering::Release);
    }
}

/// Exactly one owner closes the shared gate on every drop or exit path.
pub(crate) struct WakeOwner {
    gate: Arc<WakeGate>,
}

impl WakeOwner {
    pub(crate) fn new() -> Self {
        Self {
            gate: Arc::new(WakeGate::new()),
        }
    }

    pub(crate) fn gate(&self) -> Arc<WakeGate> {
        Arc::clone(&self.gate)
    }

    pub(crate) fn close(&self) {
        self.gate.close();
    }
}

impl Drop for WakeOwner {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::{ShellLifecycle, ShellState, WakeAdmission, WakeGate, WakeOwner};
    use crate::event::{LinuxBackend, LinuxShutdownReport, LinuxStopReason, LinuxWindowEvent};
    use crate::queue::PushError;
    use wild_buzzard_platform::{
        PhysicalSize, PixelFormat, ScaleFactor, SurfaceDescriptor, SurfaceIdAllocator,
        SurfaceNamespace, SurfaceRole,
    };

    fn report(reason: LinuxStopReason) -> LinuxShutdownReport {
        LinuxShutdownReport {
            reason,
            delivered_events: 3,
            coalesced_events: 1,
            ignored_native_events: 2,
        }
    }

    fn surface() -> SurfaceDescriptor {
        let mut allocator = SurfaceIdAllocator::new(SurfaceNamespace::new(19).unwrap());
        SurfaceDescriptor {
            id: allocator.allocate().unwrap(),
            size: PhysicalSize::new(800, 600).unwrap(),
            scale: ScaleFactor::new(1.0).unwrap(),
            format: PixelFormat::Rgba8Srgb,
            role: SurfaceRole::Window,
        }
    }

    #[test]
    fn lifecycle_is_one_way_and_preserves_first_stop_reason() {
        let mut lifecycle = ShellLifecycle::new();
        assert!(lifecycle.is_running());
        assert!(lifecycle.begin_stopping(LinuxStopReason::Requested));
        assert!(!lifecycle.begin_stopping(LinuxStopReason::BackendExited));
        assert_eq!(lifecycle.stop_reason(), Some(LinuxStopReason::Requested));

        let final_report = report(LinuxStopReason::Requested);
        assert!(lifecycle.finish(final_report));
        assert_eq!(lifecycle.report(), Some(final_report));
        assert!(!lifecycle.begin_stopping(LinuxStopReason::BackendExited));
        assert!(!lifecycle.finish(report(LinuxStopReason::BackendExited)));
    }

    #[test]
    fn startup_resumed_stop_suppresses_ready() {
        let descriptor = surface();
        let mut state = ShellState::new(2);
        state.push(LinuxWindowEvent::Resumed).unwrap();

        assert_eq!(state.pop(), Some(LinuxWindowEvent::Resumed));
        assert!(state.begin_stopping(LinuxStopReason::Requested));

        assert_eq!(
            state.push(LinuxWindowEvent::Ready {
                backend: LinuxBackend::Wayland,
                desired_surface: descriptor,
            }),
            Err(PushError::Sealed)
        );
        assert_eq!(state.pop(), None);
    }

    #[test]
    fn stopping_after_first_batch_event_suppresses_pending_and_subsequent_events() {
        let descriptor = surface();
        let mut state = ShellState::new(3);
        state.push(LinuxWindowEvent::Resumed).unwrap();
        state
            .push(LinuxWindowEvent::RedrawRequested {
                surface: descriptor.id,
            })
            .unwrap();

        assert_eq!(state.pop(), Some(LinuxWindowEvent::Resumed));
        assert!(state.begin_stopping(LinuxStopReason::Requested));

        assert_eq!(state.pop(), None);
        assert_eq!(
            state.push(LinuxWindowEvent::Suspended),
            Err(PushError::Sealed)
        );
    }

    #[test]
    fn pending_receipt_cannot_reopen_a_closed_gate() {
        let gate = WakeGate::new();
        assert_eq!(gate.admit(), WakeAdmission::Admitted);
        gate.close();
        assert!(!gate.acknowledge());
        assert_eq!(gate.admit(), WakeAdmission::Closed);
    }

    #[test]
    fn queued_wake_then_owner_drop_is_closed() {
        let owner = WakeOwner::new();
        let gate = owner.gate();
        assert_eq!(gate.admit(), WakeAdmission::Admitted);
        drop(owner);
        assert!(!gate.acknowledge());
        assert_eq!(gate.admit(), WakeAdmission::Closed);
    }

    #[test]
    fn wake_admission_coalesces_and_reopens_only_on_valid_receipt() {
        let gate = WakeGate::new();
        assert_eq!(gate.admit(), WakeAdmission::Admitted);
        assert_eq!(gate.admit(), WakeAdmission::AlreadyPending);
        assert!(gate.acknowledge());
        assert_eq!(gate.admit(), WakeAdmission::Admitted);
    }

    #[test]
    fn concurrent_admission_has_exactly_one_winner() {
        const THREADS: usize = 8;
        let gate = Arc::new(WakeGate::new());
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut workers = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let gate = Arc::clone(&gate);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                gate.admit()
            }));
        }
        let admitted = workers
            .into_iter()
            .map(|worker| worker.join().expect("wake-admission worker panicked"))
            .filter(|admission| *admission == WakeAdmission::Admitted)
            .count();
        assert_eq!(admitted, 1);
        assert_eq!(gate.admit(), WakeAdmission::AlreadyPending);
    }

    #[test]
    fn close_racing_admission_is_permanent() {
        const ADMITTERS: usize = 8;
        let gate = Arc::new(WakeGate::new());
        let barrier = Arc::new(Barrier::new(ADMITTERS + 1));
        let mut workers = Vec::with_capacity(ADMITTERS);
        for _ in 0..ADMITTERS {
            let gate = Arc::clone(&gate);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                gate.admit()
            }));
        }
        barrier.wait();
        gate.close();
        for worker in workers {
            let _ = worker.join().expect("wake/close race worker panicked");
        }

        assert!(!gate.acknowledge());
        assert_eq!(gate.admit(), WakeAdmission::Closed);
    }
}

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;

use webrender_api::{
    Checkpoint, DocumentId, ExternalEvent, FramePublishId, FrameReadyParams, NotificationHandler,
    NotificationRequest, RenderNotifier,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NotificationWaitError {
    Timeout,
    TransactionDropped,
    Disconnected,
    Overflow,
    WrongCheckpoint,
    UnexpectedExternalEvent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StageSignal {
    Checkpoint(Checkpoint),
    UnexpectedExternalEvent,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameReadyEvidence {
    pub(crate) count: u64,
    pub(crate) document_id: DocumentId,
    pub(crate) publish_id: FramePublishId,
    pub(crate) present: bool,
    pub(crate) render: bool,
    pub(crate) tracked: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct FrameReadyState {
    count: u64,
    evidence: Option<FrameReadyEvidence>,
}

#[derive(Debug, Default)]
struct StageWakeSlots {
    frame_built: Option<SyncSender<StageSignal>>,
    frame_rendered: Option<SyncSender<StageSignal>>,
}

#[derive(Debug, Default)]
struct NotifierShared {
    shutdown: Mutex<bool>,
    shutdown_ready: Condvar,
    wake_count: AtomicU64,
    frame_ready: Mutex<FrameReadyState>,
    frame_ready_changed: Condvar,
    stage_wakes: Mutex<StageWakeSlots>,
    unexpected_external_event: AtomicBool,
    overflowed: AtomicBool,
}

/// Fixed-state renderer notification sink with no unbounded event queue.
#[derive(Clone, Debug, Default)]
pub(crate) struct WindowRenderNotifier {
    shared: Arc<NotifierShared>,
}

impl WindowRenderNotifier {
    pub(crate) fn wait_for_shutdown_until(
        &self,
        deadline: Instant,
    ) -> Result<(), NotificationWaitError> {
        let mut shutdown = self
            .shared
            .shutdown
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if self.saw_unexpected_external_event() {
                return Err(NotificationWaitError::UnexpectedExternalEvent);
            }
            if self.overflowed() {
                return Err(NotificationWaitError::Overflow);
            }
            if *shutdown {
                return Ok(());
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(NotificationWaitError::Timeout);
            };
            let result = self.shared.shutdown_ready.wait_timeout(shutdown, remaining);
            let (guard, timeout) = match result {
                Ok(value) => value,
                Err(poisoned) => poisoned.into_inner(),
            };
            shutdown = guard;
            if timeout.timed_out() && !*shutdown {
                return Err(NotificationWaitError::Timeout);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn wake_count(&self) -> u64 {
        self.shared.wake_count.load(Ordering::Acquire)
    }

    pub(crate) fn frame_ready_count(&self) -> u64 {
        self.shared
            .frame_ready
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .count
    }

    pub(crate) fn wait_for_frame_ready_after(
        &self,
        observed: u64,
        deadline: Instant,
    ) -> Result<FrameReadyEvidence, NotificationWaitError> {
        let mut state = self
            .shared
            .frame_ready
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if self.saw_unexpected_external_event() {
                return Err(NotificationWaitError::UnexpectedExternalEvent);
            }
            if self.shared.overflowed.load(Ordering::Acquire) {
                return Err(NotificationWaitError::Overflow);
            }
            if state.count > observed {
                return state.evidence.ok_or(NotificationWaitError::Overflow);
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(NotificationWaitError::Timeout)?;
            let result = self
                .shared
                .frame_ready_changed
                .wait_timeout(state, remaining);
            let (guard, timeout) = match result {
                Ok(value) => value,
                Err(poisoned) => poisoned.into_inner(),
            };
            state = guard;
            if timeout.timed_out() && state.count <= observed {
                return Err(NotificationWaitError::Timeout);
            }
        }
    }

    pub(crate) fn saw_unexpected_external_event(&self) -> bool {
        self.shared
            .unexpected_external_event
            .load(Ordering::Acquire)
    }

    pub(crate) fn overflowed(&self) -> bool {
        self.shared.overflowed.load(Ordering::Acquire)
    }

    fn increment_atomic(&self, counter: &AtomicU64) {
        if counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .is_err()
        {
            self.shared.overflowed.store(true, Ordering::Release);
            self.shared.frame_ready_changed.notify_all();
            self.shared.shutdown_ready.notify_all();
        }
    }

    fn register_stage_wake(&self, checkpoint: Checkpoint, sender: SyncSender<StageSignal>) {
        let mut slots = self
            .shared
            .stage_wakes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match checkpoint {
            Checkpoint::FrameBuilt => slots.frame_built = Some(sender),
            Checkpoint::FrameRendered => slots.frame_rendered = Some(sender),
            _ => self.shared.overflowed.store(true, Ordering::Release),
        }
    }
}

impl RenderNotifier for WindowRenderNotifier {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Clone::clone(self))
    }

    fn wake_up(&self, _composite_needed: bool) {
        self.increment_atomic(&self.shared.wake_count);
    }

    fn new_frame_ready(
        &self,
        document_id: DocumentId,
        publish_id: FramePublishId,
        params: &FrameReadyParams,
    ) {
        let mut state = self
            .shared
            .frame_ready
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(next) = state.count.checked_add(1) else {
            self.shared.overflowed.store(true, Ordering::Release);
            self.shared.frame_ready_changed.notify_all();
            return;
        };
        state.count = next;
        state.evidence = Some(FrameReadyEvidence {
            count: next,
            document_id,
            publish_id,
            present: params.present,
            render: params.render,
            tracked: params.tracked,
        });
        self.shared.frame_ready_changed.notify_all();
        drop(state);
        self.wake_up(params.render);
    }

    fn external_event(&self, _event: ExternalEvent) {
        self.shared
            .unexpected_external_event
            .store(true, Ordering::Release);
        let slots = self
            .shared
            .stage_wakes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for sender in [&slots.frame_built, &slots.frame_rendered]
            .into_iter()
            .flatten()
        {
            let _ = sender.try_send(StageSignal::UnexpectedExternalEvent);
        }
        self.shared.frame_ready_changed.notify_all();
        self.shared.shutdown_ready.notify_all();
    }

    fn shut_down(&self) {
        let mut shutdown = self
            .shared
            .shutdown
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *shutdown = true;
        self.shared.shutdown_ready.notify_all();
    }
}

struct StageHandler {
    sender: SyncSender<StageSignal>,
    overflowed: Arc<AtomicBool>,
}

impl NotificationHandler for StageHandler {
    fn notify(&self, checkpoint: Checkpoint) {
        if matches!(
            self.sender.try_send(StageSignal::Checkpoint(checkpoint)),
            Err(TrySendError::Full(_))
        ) {
            self.overflowed.store(true, Ordering::Release);
        }
    }
}

/// One capacity-one, single-use `WebRender` checkpoint wait.
///
/// Upstream `NotificationRequest` owns exactly one terminal notification. The
/// capacity bound remains defense in depth against a violated imported
/// invariant; it is not an admitted duplicate-delivery protocol.
pub(crate) struct WindowStageWaiter {
    receiver: Receiver<StageSignal>,
    overflowed: Arc<AtomicBool>,
    notifier: WindowRenderNotifier,
}

impl WindowStageWaiter {
    pub(crate) fn new(
        checkpoint: Checkpoint,
        notifier: &WindowRenderNotifier,
    ) -> (NotificationRequest, Self) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let wake_sender = sender.clone();
        let overflowed = Arc::new(AtomicBool::new(false));
        let request = NotificationRequest::new(
            checkpoint,
            Box::new(StageHandler {
                sender,
                overflowed: Arc::clone(&overflowed),
            }),
        );
        notifier.register_stage_wake(checkpoint, wake_sender);
        (
            request,
            Self {
                receiver,
                overflowed,
                notifier: Clone::clone(notifier),
            },
        )
    }

    pub(crate) fn wait_until(
        self,
        expected: Checkpoint,
        deadline: Instant,
    ) -> Result<(), NotificationWaitError> {
        if self.notifier.saw_unexpected_external_event() {
            return Err(NotificationWaitError::UnexpectedExternalEvent);
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(self.external_event_or(NotificationWaitError::Timeout));
        };
        // The sticky load must be adjacent to the blocking receive. An event
        // which preceded waiter registration has no sender to wake, while an
        // event after this load observes the registered capacity-one sender.
        if self.notifier.saw_unexpected_external_event() {
            return Err(NotificationWaitError::UnexpectedExternalEvent);
        }
        let signal = match self.receiver.recv_timeout(remaining) {
            Ok(signal) => signal,
            Err(RecvTimeoutError::Timeout) => {
                return Err(self.external_event_or(NotificationWaitError::Timeout));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(self.external_event_or(NotificationWaitError::Disconnected));
            }
        };
        if self.notifier.saw_unexpected_external_event()
            || signal == StageSignal::UnexpectedExternalEvent
        {
            return Err(NotificationWaitError::UnexpectedExternalEvent);
        }
        if self.overflowed.load(Ordering::Acquire) {
            return Err(NotificationWaitError::Overflow);
        }
        let StageSignal::Checkpoint(checkpoint) = signal else {
            unreachable!("unexpected external signal returned above")
        };
        if checkpoint == Checkpoint::TransactionDropped {
            return Err(NotificationWaitError::TransactionDropped);
        }
        if checkpoint == expected {
            Ok(())
        } else {
            Err(NotificationWaitError::WrongCheckpoint)
        }
    }

    fn external_event_or(&self, fallback: NotificationWaitError) -> NotificationWaitError {
        if self.notifier.saw_unexpected_external_event() {
            NotificationWaitError::UnexpectedExternalEvent
        } else {
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use webrender_api::{
        Checkpoint, DocumentId, ExternalEvent, FramePublishId, FrameReadyParams, IdNamespace,
        NotificationHandler, RenderNotifier,
    };

    use super::{
        NotificationWaitError, StageHandler, StageSignal, WindowRenderNotifier, WindowStageWaiter,
    };

    #[test]
    fn dropped_transaction_is_reported_without_waiting_for_deadline() {
        let notifier = WindowRenderNotifier::default();
        let (request, waiter) = WindowStageWaiter::new(Checkpoint::FrameBuilt, &notifier);
        drop(request);
        assert_eq!(
            waiter.wait_until(
                Checkpoint::FrameBuilt,
                Instant::now() + Duration::from_secs(1)
            ),
            Err(NotificationWaitError::TransactionDropped)
        );
    }

    #[test]
    fn capacity_one_checkpoint_defends_against_contract_violating_duplicate_delivery() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let overflowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler = StageHandler {
            sender,
            overflowed: std::sync::Arc::clone(&overflowed),
        };
        handler.notify(Checkpoint::FrameBuilt);
        handler.notify(Checkpoint::FrameBuilt);
        assert_eq!(
            receiver.recv().unwrap(),
            StageSignal::Checkpoint(Checkpoint::FrameBuilt)
        );
        assert!(overflowed.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn frame_ready_retains_exact_publish_identity_and_flags() {
        let notifier = WindowRenderNotifier::default();
        let observed = notifier.frame_ready_count();
        let document_id = DocumentId::new(IdNamespace(7), 11);
        let publish_id = FramePublishId(19);
        notifier.new_frame_ready(
            document_id,
            publish_id,
            &FrameReadyParams {
                present: true,
                render: true,
                scrolled: false,
                tracked: true,
            },
        );
        let evidence = notifier
            .wait_for_frame_ready_after(observed, Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(evidence.document_id, document_id);
        assert_eq!(evidence.count, observed + 1);
        assert_eq!(evidence.publish_id, publish_id);
        assert!(evidence.present);
        assert!(evidence.render);
        assert!(evidence.tracked);
        assert_eq!(notifier.wake_count(), 1);
    }

    #[test]
    fn wait_uses_the_supplied_shared_deadline() {
        let notifier = WindowRenderNotifier::default();
        let expired = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("one millisecond is representable");
        assert!(matches!(
            notifier.wait_for_frame_ready_after(notifier.frame_ready_count(), expired),
            Err(NotificationWaitError::Timeout)
        ));
    }

    #[test]
    fn disconnected_checkpoint_channel_is_not_reported_as_a_timeout() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        drop(sender);
        let waiter = WindowStageWaiter {
            receiver,
            overflowed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            notifier: WindowRenderNotifier::default(),
        };
        assert_eq!(
            waiter.wait_until(
                Checkpoint::FrameBuilt,
                Instant::now() + Duration::from_secs(1)
            ),
            Err(NotificationWaitError::Disconnected)
        );
    }

    #[test]
    fn unexpected_external_event_wakes_every_bounded_wait_path() {
        let notifier = WindowRenderNotifier::default();
        let (_request, waiter) = WindowStageWaiter::new(Checkpoint::FrameBuilt, &notifier);
        let observed = notifier.frame_ready_count();
        notifier.external_event(ExternalEvent::from_raw(1));
        let deadline = Instant::now() + Duration::from_secs(1);

        assert_eq!(
            waiter.wait_until(Checkpoint::FrameBuilt, deadline),
            Err(NotificationWaitError::UnexpectedExternalEvent)
        );
        assert!(matches!(
            notifier.wait_for_frame_ready_after(observed, deadline),
            Err(NotificationWaitError::UnexpectedExternalEvent)
        ));
        assert_eq!(
            notifier.wait_for_shutdown_until(deadline),
            Err(NotificationWaitError::UnexpectedExternalEvent)
        );
    }

    #[test]
    fn event_before_stage_waiter_registration_fails_immediately_and_distinctly() {
        let notifier = WindowRenderNotifier::default();
        notifier.external_event(ExternalEvent::from_raw(1));
        let (request, waiter) = WindowStageWaiter::new(Checkpoint::FrameBuilt, &notifier);
        let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = waiter.wait_until(
                Checkpoint::FrameBuilt,
                Instant::now() + Duration::from_secs(30),
            );
            result_sender
                .send(result)
                .expect("parent retains the result receiver");
        });

        let result = result_receiver.recv_timeout(Duration::from_secs(1));
        drop(request);
        worker.join().expect("stage waiter worker must not panic");
        assert_eq!(
            result.expect("sticky event must prevent the thirty-second block"),
            Err(NotificationWaitError::UnexpectedExternalEvent)
        );
    }
}

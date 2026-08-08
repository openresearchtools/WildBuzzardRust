use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use webrender_api::{
    Checkpoint, DocumentId, ExternalEvent, FramePublishId, FrameReadyParams, NotificationHandler,
    NotificationRequest, RenderNotifier,
};

use crate::error::{FrameStage, HeadlessError};

#[derive(Debug, Default)]
struct NotifierShared {
    shutdown: Mutex<bool>,
    shutdown_ready: Condvar,
    wake_count: AtomicU64,
    frame_ready_count: Mutex<u64>,
    frame_ready: Condvar,
    unexpected_external_event: AtomicBool,
}

/// Fixed-state renderer-thread notifications with no unbounded event queue.
#[derive(Clone, Debug, Default)]
pub(crate) struct HeadlessNotifier {
    shared: Arc<NotifierShared>,
}

impl HeadlessNotifier {
    pub(crate) fn wait_for_shutdown(&self, timeout: Duration) -> bool {
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return false;
        };
        let mut shutdown = match self.shared.shutdown.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        loop {
            if *shutdown {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let result = self.shared.shutdown_ready.wait_timeout(shutdown, remaining);
            let (guard, timeout_result) = match result {
                Ok(value) => value,
                Err(poisoned) => poisoned.into_inner(),
            };
            shutdown = guard;
            if timeout_result.timed_out() && !*shutdown {
                return false;
            }
        }
    }

    pub(crate) fn wake_count(&self) -> u64 {
        self.shared.wake_count.load(Ordering::Acquire)
    }

    pub(crate) fn frame_ready_count(&self) -> u64 {
        *self
            .shared
            .frame_ready_count
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn wait_for_frame_ready_after(
        &self,
        observed: u64,
        deadline: Instant,
        configured_timeout: Duration,
    ) -> Result<(), HeadlessError> {
        let mut count = self
            .shared
            .frame_ready_count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if *count != observed {
                return Ok(());
            }
            let remaining = deadline.checked_duration_since(Instant::now()).ok_or(
                HeadlessError::FrameTimeout {
                    stage: FrameStage::FrameReady,
                    timeout: configured_timeout,
                },
            )?;
            let result = self.shared.frame_ready.wait_timeout(count, remaining);
            let (guard, timeout_result) = match result {
                Ok(value) => value,
                Err(poisoned) => poisoned.into_inner(),
            };
            count = guard;
            if timeout_result.timed_out() && *count == observed {
                return Err(HeadlessError::FrameTimeout {
                    stage: FrameStage::FrameReady,
                    timeout: configured_timeout,
                });
            }
        }
    }

    pub(crate) fn saw_unexpected_external_event(&self) -> bool {
        self.shared
            .unexpected_external_event
            .load(Ordering::Acquire)
    }
}

impl RenderNotifier for HeadlessNotifier {
    fn clone(&self) -> Box<dyn RenderNotifier> {
        Box::new(Clone::clone(self))
    }

    fn wake_up(&self, _composite_needed: bool) {
        self.shared.wake_count.fetch_add(1, Ordering::AcqRel);
    }

    fn new_frame_ready(
        &self,
        _document_id: DocumentId,
        _publish_id: FramePublishId,
        params: &FrameReadyParams,
    ) {
        let mut count = self
            .shared
            .frame_ready_count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *count = count.wrapping_add(1);
        self.shared.frame_ready.notify_all();
        drop(count);
        self.wake_up(params.render);
    }

    fn external_event(&self, _event: ExternalEvent) {
        self.shared
            .unexpected_external_event
            .store(true, Ordering::Release);
    }

    fn shut_down(&self) {
        let mut shutdown = match self.shared.shutdown.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *shutdown = true;
        self.shared.shutdown_ready.notify_all();
    }
}

struct StageHandler {
    sender: SyncSender<Checkpoint>,
    overflowed: Arc<AtomicBool>,
}

impl NotificationHandler for StageHandler {
    fn notify(&self, checkpoint: Checkpoint) {
        if matches!(self.sender.try_send(checkpoint), Err(TrySendError::Full(_))) {
            self.overflowed.store(true, Ordering::Release);
        }
    }
}

/// One capacity-one, single-use `WebRender` checkpoint wait.
pub(crate) struct StageWaiter {
    receiver: Receiver<Checkpoint>,
    overflowed: Arc<AtomicBool>,
}

impl StageWaiter {
    pub(crate) fn new(checkpoint: Checkpoint) -> (NotificationRequest, Self) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let overflowed = Arc::new(AtomicBool::new(false));
        let request = NotificationRequest::new(
            checkpoint,
            Box::new(StageHandler {
                sender,
                overflowed: Arc::clone(&overflowed),
            }),
        );
        (
            request,
            Self {
                receiver,
                overflowed,
            },
        )
    }

    pub(crate) fn wait_until(
        self,
        expected: FrameStage,
        deadline: Instant,
        configured_timeout: Duration,
    ) -> Result<(), HeadlessError> {
        let remaining =
            deadline
                .checked_duration_since(Instant::now())
                .ok_or(HeadlessError::FrameTimeout {
                    stage: expected,
                    timeout: configured_timeout,
                })?;
        let checkpoint =
            self.receiver
                .recv_timeout(remaining)
                .map_err(|_| HeadlessError::FrameTimeout {
                    stage: expected,
                    timeout: configured_timeout,
                })?;
        if self.overflowed.load(Ordering::Acquire) {
            return Err(HeadlessError::NotificationOverflow);
        }
        if checkpoint == Checkpoint::TransactionDropped {
            return Err(HeadlessError::TransactionDropped { expected });
        }
        let matches_expected = matches!(
            (expected, checkpoint),
            (FrameStage::FrameBuilt, Checkpoint::FrameBuilt)
                | (FrameStage::FrameRendered, Checkpoint::FrameRendered)
        );
        if matches_expected {
            Ok(())
        } else {
            Err(HeadlessError::TransactionDropped { expected })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use webrender_api::{Checkpoint, NotificationHandler};

    use super::{HeadlessNotifier, StageHandler, StageWaiter};
    use crate::{FrameStage, HeadlessError};

    #[test]
    fn dropped_transaction_is_reported_without_waiting_for_timeout() {
        let (request, waiter) = StageWaiter::new(Checkpoint::FrameBuilt);
        drop(request);
        assert!(matches!(
            waiter.wait_until(
                FrameStage::FrameBuilt,
                Instant::now() + Duration::from_secs(1),
                Duration::from_secs(1)
            ),
            Err(HeadlessError::TransactionDropped {
                expected: FrameStage::FrameBuilt
            })
        ));
    }

    #[test]
    fn overflow_is_explicit() {
        let (request, waiter) = StageWaiter::new(Checkpoint::FrameBuilt);
        let handler = request;
        drop(handler);
        let _ = waiter;

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let overflowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stage_handler = StageHandler {
            sender,
            overflowed: std::sync::Arc::clone(&overflowed),
        };
        stage_handler.notify(Checkpoint::FrameBuilt);
        stage_handler.notify(Checkpoint::FrameBuilt);
        assert!(overflowed.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(receiver.recv().unwrap(), Checkpoint::FrameBuilt);
    }

    #[test]
    fn frame_ready_wait_observes_a_notification_that_precedes_the_wait() {
        let notifier = HeadlessNotifier::default();
        let observed = notifier.frame_ready_count();
        {
            let mut count = notifier.shared.frame_ready_count.lock().unwrap();
            *count = count.wrapping_add(1);
            notifier.shared.frame_ready.notify_all();
        }
        assert!(
            notifier
                .wait_for_frame_ready_after(
                    observed,
                    Instant::now() + Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .is_ok()
        );
    }

    #[test]
    fn frame_ready_wait_has_a_bounded_timeout() {
        let notifier = HeadlessNotifier::default();
        let observed = notifier.frame_ready_count();
        assert!(matches!(
            notifier.wait_for_frame_ready_after(observed, Instant::now(), Duration::from_secs(1),),
            Err(HeadlessError::FrameTimeout {
                stage: FrameStage::FrameReady,
                ..
            })
        ));
    }
}

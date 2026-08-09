use std::collections::VecDeque;

use wild_buzzard_platform::SurfaceId;

use crate::event::LinuxWindowEvent;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PushError {
    Saturated { capacity: usize },
    Sealed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoalesceKey {
    Resize(SurfaceId),
    Scale(SurfaceId),
    Redraw(SurfaceId),
    PointerMove(SurfaceId, u64),
}

pub(crate) struct EventQueue {
    ordinary: VecDeque<LinuxWindowEvent>,
    capacity: usize,
    sealed: bool,
    coalesced: u64,
}

impl EventQueue {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            ordinary: VecDeque::with_capacity(capacity),
            capacity,
            sealed: false,
            coalesced: 0,
        }
    }

    pub(crate) fn push(&mut self, event: LinuxWindowEvent) -> Result<(), PushError> {
        if self.sealed {
            return Err(PushError::Sealed);
        }
        if let Some(key) = coalesce_key(&event) {
            // Only the queue tail may be replaced. Any intervening event is an
            // ordering barrier: replacing an older entry across it would move
            // the new state ahead of a key, button, focus, or IME event.
            if let Some(existing) = self
                .ordinary
                .back_mut()
                .filter(|queued| coalesce_key(queued) == Some(key))
            {
                *existing = event;
                self.coalesced = self.coalesced.saturating_add(1);
                return Ok(());
            }
        }
        if self.ordinary.len() == self.capacity {
            return Err(PushError::Saturated {
                capacity: self.capacity,
            });
        }
        self.ordinary.push_back(event);
        Ok(())
    }

    pub(crate) fn pop(&mut self) -> Option<LinuxWindowEvent> {
        if self.sealed {
            None
        } else {
            self.ordinary.pop_front()
        }
    }

    /// Permanently rejects ordinary events and suppresses every queued one.
    pub(crate) fn seal(&mut self) {
        self.sealed = true;
        self.ordinary.clear();
    }

    pub(crate) const fn coalesced(&self) -> u64 {
        self.coalesced
    }
}

fn coalesce_key(event: &LinuxWindowEvent) -> Option<CoalesceKey> {
    match event {
        LinuxWindowEvent::Resized { surface, .. } => Some(CoalesceKey::Resize(*surface)),
        LinuxWindowEvent::ScaleFactorChanged { surface, .. } => Some(CoalesceKey::Scale(*surface)),
        LinuxWindowEvent::RedrawRequested { surface } => Some(CoalesceKey::Redraw(*surface)),
        LinuxWindowEvent::Input {
            event: wild_buzzard_platform::InputEvent::Pointer(pointer),
            ..
        } if pointer.phase == wild_buzzard_platform::PointerPhase::Move => Some(
            CoalesceKey::PointerMove(pointer.metadata.surface, pointer.pointer.get()),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{EventQueue, PushError};
    use crate::event::LinuxWindowEvent;
    use wild_buzzard_platform::{
        PhysicalSize, PixelFormat, ScaleFactor, SurfaceDescriptor, SurfaceIdAllocator,
        SurfaceNamespace, SurfaceRole,
    };

    fn surface() -> SurfaceDescriptor {
        let mut allocator = SurfaceIdAllocator::new(SurfaceNamespace::new(7).unwrap());
        SurfaceDescriptor {
            id: allocator.allocate().unwrap(),
            size: PhysicalSize::new(800, 600).unwrap(),
            scale: ScaleFactor::new(1.0).unwrap(),
            format: PixelFormat::Rgba8Srgb,
            role: SurfaceRole::Window,
        }
    }

    #[test]
    fn replaceable_events_coalesce_without_consuming_capacity() {
        let surface = surface();
        let mut queue = EventQueue::new(1);
        queue
            .push(LinuxWindowEvent::RedrawRequested {
                surface: surface.id,
            })
            .unwrap();
        queue
            .push(LinuxWindowEvent::RedrawRequested {
                surface: surface.id,
            })
            .unwrap();
        assert_eq!(queue.coalesced(), 1);
        assert!(matches!(
            queue.pop(),
            Some(LinuxWindowEvent::RedrawRequested { .. })
        ));
    }

    #[test]
    fn sealed_queue_suppresses_pending_and_subsequent_events() {
        let surface = surface();
        let mut queue = EventQueue::new(2);
        queue.push(LinuxWindowEvent::Resumed).unwrap();
        queue
            .push(LinuxWindowEvent::CloseRequested {
                surface: surface.id,
            })
            .unwrap();
        queue.seal();
        assert_eq!(queue.pop(), None);
        assert_eq!(
            queue.push(LinuxWindowEvent::WakeRequested),
            Err(PushError::Sealed)
        );
    }

    #[test]
    fn ordinary_capacity_remains_a_hard_bound() {
        let mut queue = EventQueue::new(1);
        queue.push(LinuxWindowEvent::Resumed).unwrap();
        assert_eq!(
            queue.push(LinuxWindowEvent::Suspended),
            Err(PushError::Saturated { capacity: 1 })
        );
    }

    #[test]
    fn non_coalescible_event_is_an_ordering_barrier() {
        let surface = surface();
        let mut queue = EventQueue::new(3);
        queue
            .push(LinuxWindowEvent::RedrawRequested {
                surface: surface.id,
            })
            .unwrap();
        queue.push(LinuxWindowEvent::Resumed).unwrap();
        queue
            .push(LinuxWindowEvent::RedrawRequested {
                surface: surface.id,
            })
            .unwrap();

        assert_eq!(queue.coalesced(), 0);
        assert!(matches!(
            queue.pop(),
            Some(LinuxWindowEvent::RedrawRequested { .. })
        ));
        assert_eq!(queue.pop(), Some(LinuxWindowEvent::Resumed));
        assert!(matches!(
            queue.pop(),
            Some(LinuxWindowEvent::RedrawRequested { .. })
        ));
    }
}

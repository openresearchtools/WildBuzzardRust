//! Pointer-free, typed generational handles.
//!
//! A [`Handle`] is an identity token only: it never grants access to a value.
//! The matching [`Arena`] validates both its slot and generation before access.
//! Handles are therefore safe to move between threads even when the referenced
//! value is not; access remains governed by the arena's ownership and locking.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::num::NonZeroU32;

/// The maximum number of addressable slots in one arena.
///
/// Slot indices cover the inclusive range `0..=u32::MAX`, which contains one
/// more value than `u32::MAX`. The count is represented as `u64` so this
/// invariant remains expressible even when compiling tooling for a 32-bit host.
pub const MAX_SLOT_COUNT: u64 = u32::MAX as u64 + 1;

/// Returns the next zero-based slot index for an existing slot count.
///
/// This boundary helper keeps the inclusive `u32` index range directly
/// testable without attempting an enormous allocation.
pub const fn next_slot_index(slot_count: u64) -> Result<u32, InsertError> {
    if slot_count < MAX_SLOT_COUNT {
        Ok(slot_count as u32)
    } else {
        Err(InsertError::CapacityExhausted)
    }
}

/// An untyped, pointer-free representation of a generational handle.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RawHandle {
    slot: u32,
    generation: NonZeroU32,
}

impl RawHandle {
    /// Creates a raw handle, rejecting generation zero because it is reserved.
    pub fn new(slot: u32, generation: u32) -> Result<Self, InvalidHandle> {
        let generation = NonZeroU32::new(generation).ok_or(InvalidHandle::ZeroGeneration)?;
        Ok(Self { slot, generation })
    }

    /// Returns the zero-based arena slot.
    #[must_use]
    pub const fn slot(self) -> u32 {
        self.slot
    }

    /// Returns the non-zero generation value.
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }
}

/// A typed identity for a value stored in an [`Arena`].
///
/// `Handle<T>` is `Send + Sync` for every `T` because it contains no reference
/// to `T`. It must be resolved through the correct arena before use.
pub struct Handle<T: ?Sized> {
    raw: RawHandle,
    marker: PhantomData<fn() -> T>,
}

impl<T: ?Sized> Handle<T> {
    /// Creates a typed handle from validated integer parts.
    pub fn from_parts(slot: u32, generation: u32) -> Result<Self, InvalidHandle> {
        Ok(Self::from_raw(RawHandle::new(slot, generation)?))
    }

    /// Creates a typed handle from a raw identity.
    #[must_use]
    pub const fn from_raw(raw: RawHandle) -> Self {
        Self {
            raw,
            marker: PhantomData,
        }
    }

    /// Returns the pointer-free raw identity.
    #[must_use]
    pub const fn into_raw(self) -> RawHandle {
        self.raw
    }

    /// Returns the zero-based arena slot.
    #[must_use]
    pub const fn slot(self) -> u32 {
        self.raw.slot()
    }

    /// Returns the non-zero generation value.
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.raw.generation()
    }
}

impl<T: ?Sized> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for Handle<T> {}

impl<T: ?Sized> fmt::Debug for Handle<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Handle")
            .field("slot", &self.raw.slot())
            .field("generation", &self.raw.generation())
            .finish()
    }
}

impl<T: ?Sized> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T: ?Sized> Eq for Handle<T> {}

impl<T: ?Sized> Hash for Handle<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

impl<T: ?Sized> PartialOrd for Handle<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: ?Sized> Ord for Handle<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

/// A malformed raw handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidHandle {
    /// Generation zero is reserved and never issued by an arena.
    ZeroGeneration,
}

impl fmt::Display for InvalidHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroGeneration => formatter.write_str("handle generation must be non-zero"),
        }
    }
}

impl Error for InvalidHandle {}

/// Failure to allocate another arena slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertError {
    /// Every representable slot has been allocated or permanently retired.
    CapacityExhausted,
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("generational arena capacity exhausted")
    }
}

impl Error for InsertError {}

#[derive(Debug)]
struct Slot<T> {
    generation: NonZeroU32,
    value: Option<T>,
    retired: bool,
}

/// A compact owner of values addressed by typed generational handles.
///
/// Removing a value invalidates its handle before the slot can be reused. If a
/// slot reaches generation `u32::MAX`, it is retired rather than wrapping and
/// allowing an ancient handle to become valid again.
#[derive(Debug)]
pub struct Arena<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    len: usize,
}

impl<T> Arena<T> {
    /// Creates an empty arena.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
        }
    }

    /// Returns the number of live values.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the arena has no live values.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Inserts a value and returns its typed identity.
    pub fn try_insert(&mut self, value: T) -> Result<Handle<T>, InsertError> {
        if let Some(slot_index) = self.free.pop() {
            let slot = &mut self.slots[slot_index as usize];
            debug_assert!(slot.value.is_none());
            debug_assert!(!slot.retired);
            slot.value = Some(value);
            self.len += 1;
            return Ok(Handle::from_raw(RawHandle {
                slot: slot_index,
                generation: slot.generation,
            }));
        }

        let slot_count =
            u64::try_from(self.slots.len()).map_err(|_| InsertError::CapacityExhausted)?;
        let slot_index = next_slot_index(slot_count)?;
        let generation = NonZeroU32::MIN;
        self.slots.push(Slot {
            generation,
            value: Some(value),
            retired: false,
        });
        self.len += 1;
        Ok(Handle::from_raw(RawHandle {
            slot: slot_index,
            generation,
        }))
    }

    /// Returns a shared reference only when the slot and generation are live.
    #[must_use]
    pub fn get(&self, handle: Handle<T>) -> Option<&T> {
        let slot = self.slots.get(handle.slot() as usize)?;
        (slot.generation.get() == handle.generation())
            .then_some(slot.value.as_ref())
            .flatten()
    }

    /// Returns an exclusive reference only when the slot and generation are live.
    #[must_use]
    pub fn get_mut(&mut self, handle: Handle<T>) -> Option<&mut T> {
        let slot = self.slots.get_mut(handle.slot() as usize)?;
        (slot.generation.get() == handle.generation())
            .then_some(slot.value.as_mut())
            .flatten()
    }

    /// Returns whether a handle currently resolves in this arena.
    #[must_use]
    pub fn contains(&self, handle: Handle<T>) -> bool {
        self.get(handle).is_some()
    }

    /// Removes a value, invalidating the supplied handle before slot reuse.
    pub fn remove(&mut self, handle: Handle<T>) -> Option<T> {
        let slot = self.slots.get_mut(handle.slot() as usize)?;
        if slot.generation.get() != handle.generation() {
            return None;
        }

        let value = slot.value.take()?;
        self.len -= 1;
        if slot.generation.get() == u32::MAX {
            slot.retired = true;
        } else {
            slot.generation = NonZeroU32::new(slot.generation.get() + 1)
                .expect("a non-maximum generation increments to a non-zero value");
            self.free.push(handle.slot());
        }
        Some(value)
    }

    /// Invalidates and removes every live value, returning the removed count.
    pub fn clear(&mut self) -> usize {
        let live_handles: Vec<_> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(slot_index, slot)| {
                slot.value.as_ref()?;
                let slot_index = u32::try_from(slot_index).ok()?;
                Some(Handle::from_raw(RawHandle {
                    slot: slot_index,
                    generation: slot.generation,
                }))
            })
            .collect();
        let removed = live_handles.len();
        for handle in live_handles {
            let _ = self.remove(handle);
        }
        removed
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{Arena, Handle, InvalidHandle, MAX_SLOT_COUNT, RawHandle, next_slot_index};
    use std::cell::Cell;

    #[test]
    fn stale_handle_never_resolves_after_slot_reuse() {
        let mut arena = Arena::new();
        let first = arena.try_insert(String::from("first")).unwrap();

        assert_eq!(arena.remove(first), Some(String::from("first")));
        assert_eq!(arena.get(first), None);

        let second = arena.try_insert(String::from("second")).unwrap();
        assert_eq!(first.slot(), second.slot());
        assert_ne!(first.generation(), second.generation());
        assert_eq!(arena.get(first), None);
        assert_eq!(arena.get(second).map(String::as_str), Some("second"));
    }

    #[test]
    fn clear_invalidates_every_issued_handle() {
        let mut arena = Arena::new();
        let first = arena.try_insert(1).unwrap();
        let second = arena.try_insert(2).unwrap();

        assert_eq!(arena.clear(), 2);
        assert!(arena.is_empty());
        assert!(!arena.contains(first));
        assert!(!arena.contains(second));
    }

    #[test]
    fn generation_zero_is_rejected() {
        assert_eq!(RawHandle::new(4, 0), Err(InvalidHandle::ZeroGeneration));
        assert_eq!(
            Handle::<String>::from_parts(4, 0),
            Err(InvalidHandle::ZeroGeneration)
        );
    }

    #[test]
    fn complete_u32_slot_index_range_is_representable() {
        assert_eq!(next_slot_index(0), Ok(0));
        assert_eq!(next_slot_index(MAX_SLOT_COUNT - 1), Ok(u32::MAX));
        assert_eq!(
            next_slot_index(MAX_SLOT_COUNT),
            Err(super::InsertError::CapacityExhausted)
        );
    }

    #[test]
    fn handles_are_thread_safe_identity_tokens() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Handle<Cell<u8>>>();
    }

    #[test]
    fn mutable_access_requires_the_arena() {
        let mut arena = Arena::new();
        let handle = arena.try_insert(4_u32).unwrap();
        *arena.get_mut(handle).unwrap() += 1;
        assert_eq!(arena.get(handle), Some(&5));
        assert_eq!(arena.len(), 1);
    }
}

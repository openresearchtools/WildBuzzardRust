use crate::{IdentityKind, WasmError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Key {
    pub(crate) slot: u32,
    pub(crate) generation: u64,
}

struct Slot<T> {
    generation: u64,
    reserved: bool,
    value: Option<T>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Reservation(Key);

pub(crate) struct Registry<T> {
    slots: Vec<Slot<T>>,
    active: usize,
}

impl<T> Registry<T> {
    pub(crate) fn new() -> Self {
        Self {
            slots: Vec::new(),
            active: 0,
        }
    }

    pub(crate) fn active(&self) -> usize {
        self.active
    }

    #[cfg(test)]
    pub(crate) fn insert(
        &mut self,
        value: T,
        maximum: usize,
        kind: IdentityKind,
    ) -> Result<Key, WasmError> {
        let reservation = self.reserve(maximum, kind)?;
        self.commit(reservation, value)
    }

    pub(crate) fn reserve(
        &mut self,
        maximum: usize,
        kind: IdentityKind,
    ) -> Result<Reservation, WasmError> {
        if self.active >= maximum {
            return Err(WasmError::CapacityExceeded { kind, maximum });
        }

        if let Some((slot, entry)) = self.slots.iter_mut().enumerate().find(|(_, entry)| {
            entry.value.is_none() && !entry.reserved && entry.generation != u64::MAX
        }) {
            let slot = u32::try_from(slot).map_err(|_| WasmError::IdentitySpaceExhausted)?;
            entry.reserved = true;
            self.active += 1;
            return Ok(Reservation(Key {
                slot,
                generation: entry.generation,
            }));
        }

        let slot =
            u32::try_from(self.slots.len()).map_err(|_| WasmError::IdentitySpaceExhausted)?;
        self.slots
            .try_reserve(1)
            .map_err(|_| WasmError::HostAllocationFailed)?;
        self.slots.push(Slot {
            generation: 1,
            reserved: true,
            value: None,
        });
        self.active += 1;
        Ok(Reservation(Key {
            slot,
            generation: 1,
        }))
    }

    pub(crate) fn commit(&mut self, reservation: Reservation, value: T) -> Result<Key, WasmError> {
        let key = reservation.0;
        let Some(entry) = self.slots.get_mut(key.slot as usize) else {
            return Err(WasmError::InternalInvariant {
                detail: "reserved registry slot disappeared",
            });
        };
        if entry.generation != key.generation || !entry.reserved || entry.value.is_some() {
            return Err(WasmError::InternalInvariant {
                detail: "registry reservation changed before commit",
            });
        }
        entry.reserved = false;
        entry.value = Some(value);
        Ok(key)
    }

    pub(crate) fn cancel(&mut self, reservation: Reservation) {
        let key = reservation.0;
        let Some(entry) = self.slots.get_mut(key.slot as usize) else {
            return;
        };
        if entry.generation == key.generation && entry.reserved && entry.value.is_none() {
            entry.reserved = false;
            self.active -= 1;
            entry.generation = entry.generation.saturating_add(1);
        }
    }

    pub(crate) fn get(&self, key: Key) -> Option<&T> {
        let entry = self.slots.get(key.slot as usize)?;
        (entry.generation == key.generation)
            .then_some(entry.value.as_ref())
            .flatten()
    }

    pub(crate) fn get_mut(&mut self, key: Key) -> Option<&mut T> {
        let entry = self.slots.get_mut(key.slot as usize)?;
        (entry.generation == key.generation)
            .then_some(entry.value.as_mut())
            .flatten()
    }

    pub(crate) fn remove(&mut self, key: Key) -> Option<T> {
        let entry = self.slots.get_mut(key.slot as usize)?;
        if entry.generation != key.generation {
            return None;
        }
        let value = entry.value.take()?;
        self.active -= 1;
        entry.reserved = false;
        entry.generation = entry.generation.saturating_add(1);
        Some(value)
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = Key> + '_ {
        self.slots.iter().enumerate().filter_map(|(slot, entry)| {
            entry.value.as_ref()?;
            Some(Key {
                slot: u32::try_from(slot).ok()?,
                generation: entry.generation,
            })
        })
    }

    pub(crate) fn invalidate_all(&mut self) {
        for entry in &mut self.slots {
            if entry.value.take().is_some() || entry.reserved {
                entry.reserved = false;
                entry.generation = entry.generation.saturating_add(1);
            }
        }
        self.active = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{Key, Registry, Slot};
    use crate::IdentityKind;

    #[test]
    fn exhausted_generation_is_never_reused() {
        let mut registry = Registry {
            slots: vec![Slot {
                generation: u64::MAX,
                reserved: false,
                value: Some(1_u8),
            }],
            active: 1,
        };
        let retired = Key {
            slot: 0,
            generation: u64::MAX,
        };
        assert_eq!(registry.remove(retired), Some(1));
        let replacement = registry.insert(2, 1, IdentityKind::Module).unwrap();
        assert_eq!(replacement.slot, 1);
        assert!(registry.get(retired).is_none());
    }
}

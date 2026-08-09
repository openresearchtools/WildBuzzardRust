use std::fmt;

use crate::registry::Key;

macro_rules! opaque_identity {
    ($name:ident, $label:literal) => {
        #[doc = concat!("An opaque, owner- and generation-checked ", $label, " identity.")]
        #[derive(Clone, Copy, Eq, Hash, PartialEq)]
        pub struct $name {
            owner: u64,
            slot: u32,
            generation: u64,
        }

        impl $name {
            pub(crate) fn new(owner: u64, key: Key) -> Self {
                Self {
                    owner,
                    slot: key.slot,
                    generation: key.generation,
                }
            }

            pub(crate) fn owner(self) -> u64 {
                self.owner
            }

            pub(crate) fn key(self) -> Key {
                Key {
                    slot: self.slot,
                    generation: self.generation,
                }
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct($label)
                    .field("slot", &self.slot)
                    .field("generation", &self.generation)
                    .finish_non_exhaustive()
            }
        }
    };
}

opaque_identity!(ModuleId, "ModuleId");
opaque_identity!(StoreId, "StoreId");
opaque_identity!(InstanceId, "InstanceId");

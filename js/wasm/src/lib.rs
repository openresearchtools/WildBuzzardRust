#![forbid(unsafe_code)]
#![doc = include_str!("../README.md")]

mod error;
mod identity;
mod limits;
mod policy;
mod registry;
mod runtime;

pub use error::{IdentityKind, WasmError};
pub use identity::{InstanceId, ModuleId, StoreId};
pub use limits::{WASM_PAGE_BYTES, WasmLimits};
pub use policy::{
    CapabilityPolicy, INITIAL_CAPABILITY_POLICY, INITIAL_PROPOSAL_POLICY, ProposalPolicy,
};
pub use runtime::{CleanupReport, InterruptHandle, LiveCounts, WasmProcess};

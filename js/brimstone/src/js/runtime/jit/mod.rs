//! Contained baseline-JIT infrastructure prototype.
//!
//! This module is feature-gated, Linux x86-64 only, and is not connected to VM hot-function
//! dispatch. In particular, no untrusted page or DOM binding can enter generated code. One exact,
//! rooted function binding may execute a bounded guarded native local CFG, including polled loops,
//! and may continue a separately proven side exit through an ordinary Brimstone VM frame. Native
//! shadow frames are linked into the moving-GC root walker only for the one audited allocating
//! helper. Native entry, return provenance, helper calls, and VM continuation all fail closed.

// This feature-gated infrastructure is intentionally detached from product dispatch, so its
// internal entry points have no non-test caller yet.
#![allow(dead_code)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
compile_error!("the baseline_jit prototype only supports x86_64-unknown-linux-gnu");

pub(crate) mod abi;
pub(crate) mod code_cache;
pub(crate) mod compiler;
pub(crate) mod continuation;
pub(crate) mod hotness;

pub(crate) const PRODUCT_DISPATCH_ENABLED: bool = false;
const _: () = assert!(!PRODUCT_DISPATCH_ENABLED);

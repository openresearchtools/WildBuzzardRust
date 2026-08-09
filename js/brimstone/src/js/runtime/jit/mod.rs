//! Contained baseline-JIT infrastructure prototype.
//!
//! This module is feature-gated, Linux x86-64 only, and is not connected to VM hot-function
//! dispatch. In particular, no untrusted page or DOM binding can enter generated code. Side exits
//! are validated ABI records only: there is no interpreter-resume integration. Likewise, the
//! shadow-frame schema is not linked into Brimstone's GC root walker and is not GC-visible.

// This feature-gated infrastructure is intentionally detached from product dispatch, so its
// internal entry points have no non-test caller yet.
#![allow(dead_code)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
compile_error!("the baseline_jit prototype only supports x86_64-unknown-linux-gnu");

pub(crate) mod abi;
pub(crate) mod code_cache;
pub(crate) mod compiler;
pub(crate) mod hotness;

pub(crate) const PRODUCT_DISPATCH_ENABLED: bool = false;
const _: () = assert!(!PRODUCT_DISPATCH_ENABLED);

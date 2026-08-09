//! Contained baseline-JIT infrastructure prototype.
//!
//! This module is feature-gated and Linux x86-64 only. The ordinary VM call path owns a bounded
//! hot-dispatch hook, but compile-time product admission remains false; only `cfg(test)` may enable
//! it, so no untrusted page or DOM binding can enter generated code. One exact rooted function
//! binding may execute a bounded guarded native local CFG, including inline-polled loops, and may
//! continue a separately proven side exit through an ordinary Brimstone VM frame. Native shadow
//! frames are linked into the moving-GC root walker only for the audited allocating helper. Native
//! entry, return provenance, helper calls, and VM continuation all fail closed.

// Most feature-gated proof helpers remain intentionally private to this module.
#![allow(dead_code)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
compile_error!("the baseline_jit prototype only supports x86_64-unknown-linux-gnu");

pub(crate) mod abi;
pub(crate) mod code_cache;
pub(crate) mod compiler;
pub(crate) mod continuation;
pub(crate) mod dispatch;
pub(crate) mod hotness;

pub(crate) const PRODUCT_DISPATCH_ENABLED: bool = false;
const _: () = assert!(!PRODUCT_DISPATCH_ENABLED);

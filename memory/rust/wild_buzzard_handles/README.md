# wild_buzzard_handles

This crate owns pointer-free, typed generational identities. A handle can cross a thread boundary,
but it cannot dereference anything; only its owning `Arena<T>` can resolve it. Removed slots advance
their generation, and slots are permanently retired instead of wrapping generation `u32::MAX`.
All `u32` slot indices, including `u32::MAX`, are representable; the corresponding maximum slot
count is `u32::MAX + 1` and has a directly tested boundary helper.

The crate contains no `unsafe`, allocator replacement, operating-system integration, telemetry, or
provider code. `Arena<T>` inherits Rust's normal `Send`/`Sync` behavior from `T`; `Handle<T>` is
always `Send + Sync` because it is only an integer identity.

Firefox ESR153 reference paths inspected at
`c19b7e89270787889495688244ec6ee8e79288a1`:

- `xpcom/base/nsISupports.idl`
- `xpcom/components/nsIServiceManager.idl`
- `xpcom/rust/xpcom/src/refptr.rs`
- `xpcom/tests/SizeTest01.cpp`
- `xpcom/tests/SizeTest02.cpp`
- `xpcom/tests/RegFactory.cpp`

The full history of `xpcom/rust/xpcom/src/refptr.rs` was inspected, including the refcount-type and
thread-bound pointer changes. This crate intentionally does not port `RefPtr`, `QueryInterface`, raw
out-parameters, intrusive reference counts, or COM ABI compatibility.

# wild_buzzard_services

This crate replaces process-local XPCOM pointer lookup with typed service contracts, explicit
non-zero namespaces, generational identities, and `Arc`-returning registries. Wire identities carry
the service kind, namespace, slot, and generation; converting them back to a typed identity checks
the service kind first.

Stable `ServiceKind` values come from the orchestrator-owned checked-in contract registry. There is
no mutable global runtime registry. An integrating process constructs a caller-owned
`ServiceContractRegistry`, which rejects duplicate kind assignments before service traffic begins.

`ServiceSpec::Interface` must be `Send + Sync + 'static`. A successfully resolved `Arc` remains
valid if another thread unregisters the identity; subsequent resolutions fail as stale. No registry
method calls service code while holding its lock.

Firefox ESR153 reference paths inspected at
`c19b7e89270787889495688244ec6ee8e79288a1`:

- `xpcom/base/nsISupports.idl`
- `xpcom/components/nsIServiceManager.idl`
- `xpcom/rust/xpcom/src/refptr.rs`
- `xpcom/tests/RegFactory.cpp`
- `xpcom/tests/TestShutdown.cpp`

History for the service-manager IDL and Rust `RefPtr` implementation was inspected. Wild Buzzard
does not preserve contract-string lookup, `QueryInterface`, intrusive reference counts, raw
out-parameters, COM ABI compatibility, or implicit service construction. Factories and lifecycle
coordination remain future integration work.

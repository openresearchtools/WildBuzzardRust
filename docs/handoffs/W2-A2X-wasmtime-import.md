# W2-A2X Wasmtime source import

Task: admit the exact selected Wasmtime source and core WebAssembly conformance input without
activating non-web capabilities.

Owner: Agent 2, JavaScript and WebAssembly runtime.

Status: complete source-admission slice; browser integration remains blocked.

Upstream source:

- Wasmtime repository: `https://github.com/bytecodealliance/wasmtime.git`
- release: `v47.0.3`
- revision: `5554cc1a651da536af2cc46c7324bdc085b162e3`
- tree: `c48fdb3d3530ac038f149f17d9e35f0a554ec0ec`
- core specification suite: `https://github.com/WebAssembly/testsuite` at
  `0dc0343c9876267d99a7577ed4fc2289406a7869`, tree
  `c1c4b8d1bdc915a5a8dfe413a83e600cdfb0f9f7`

Wild Buzzard paths changed:

- `js/wasmtime/`
- `docs/upstream-components.toml`
- `docs/import-status.md`
- `docs/architecture/javascript-wasm-runtime.md`
- `docs/program-status.toml`
- `AGENTS.md`

Contract added or changed: the exact Wasmtime superproject source and core Wasm spec suite are now
available locally, but neither is a root-workspace dependency. Product integration must use
`wasmtime` with defaults disabled and only `std,runtime,cranelift,gc,gc-drc,threads` until another
feature passes a separate gate. WASI and ambient host capabilities are forbidden for page content.

Tests run and results:

- External Git-index verification matched all 6,859 superproject blobs and all 296 materialized
  spec-suite blobs by mode, blob ID, and path.
- `cargo metadata --locked` passed for `x86_64-unknown-linux-gnu`.
- The selected graph contains 23 in-tree and 59 registry packages with zero Git dependencies.
- A locked external `cargo check` passed.
- A binary empty Wasm module compiled and instantiated through the imported source without WAT or
  WASI.
- A repository-wide `git diff --check` reports whitespace already present in exact upstream files;
  the Wild Buzzard-authored note and documentation pass their scoped checks. Do not normalize the
  imported blobs inside this provenance commit.

Parity evidence: the core specification-suite source is pinned locally. The suite has not yet been
run against a browser adapter, so this is not Wasm or JavaScript `WebAssembly` API conformance.

Known behavioral differences: there is no Wild Buzzard `WebAssembly` JavaScript API, streaming
compile path, ArrayBuffer/SharedArrayBuffer bridge, CSP/isolation policy, cross-heap collector
contract, debugger integration, cache, or browser error mapping. DRC cannot collect cycles; the
copying collector is not functional; threads/shared memory remain separately gated.

Unsafe or FFI introduced: no new first-party unsafe code. The selected upstream build compiles its
existing C unwind-registration helper. Executable-memory, signal, unwind, sandbox, and AppImage
closure review remain required before product admission.

Licenses and provenance: Wasmtime packages declare Apache-2.0 with LLVM exception. The materialized
core spec suite is Apache-2.0. The release commit has a PGP signature, but its public key was not
available locally, so the signature was present but not locally verified. Full hashes, gitlink
disposition, archive hashes, dependency inventory, and refresh rules are in
`js/wasmtime/WILDBUZZARD_UPSTREAM.md`.

Provider or network implications: WASI, WASI HTTP, sockets, filesystem hosts, CLI, and server layers
are not enabled. The 59 locked registry-package sources remain unvendored and therefore remain
network/reproducibility work for an offline AppImage build.

Blocked on: browser-owned adapter and JS API, Brimstone/Wasmtime stable-ID rooting, hard resource
limits, interruption and teardown, selected-proposal spec execution, JS API WPT, minimal-feature
upstream test guards, and AppImage dependency/native-boundary closure.

Recommended next action: create a first-party `js/wasm` adapter with one process-scoped Engine,
resource-accounted Stores, binary-only module validation/compile tests, no WASI linker, generation-
checked wrapper IDs, and explicit disabled-feature tests before exposing any JavaScript object.

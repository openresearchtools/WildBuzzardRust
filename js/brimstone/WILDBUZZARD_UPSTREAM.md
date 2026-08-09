# Brimstone upstream snapshot

This directory was imported as an exact source snapshot of Brimstone and is now the editable
Wild Buzzard adaptation tree. Git commit `4063038` preserves the original byte-identical import at
`b544eff`; a later freshness refresh applies the exact one-commit upstream delta to the current pin,
and other commits carry the separately reviewable local patches listed below. It is the pinned
baseline for the JavaScript execution-engine workstream, but it is not yet wired into the root
workspace or accepted as a production-safe browser runtime.

- Upstream repository: <https://github.com/Hans-Halverson/brimstone>
- Upstream default branch at import: `master`
- Commit: `bfb720f0afb8b2b28b27c22ee7091deb7d16b082`
- Commit date: `2026-08-09T02:56:40+00:00`
- Git tree: `6c1221d72cff1dcec5fdb6cd10d329abb935f126`
- Upstream tracked files: 1,555
- License: MIT (`LICENSE`)
- Import method: `git archive` of the original pin, followed by the exact reviewed upstream delta
- Original exact-import Wild Buzzard commit: `4063038` (`b544eff`)
- Current upstream refresh: exact `b544eff..bfb720f` one-commit/four-path delta, independently
  byte-checked against a clean non-shallow upstream checkout which is now detached at the pin
- Local adaptation patches: see below and `docs/upstream-components.toml`

The original exact-import commit contains 1,554 byte-identical upstream files plus this note. The
current source baseline has 1,555 upstream files; its refresh adds primitive-receiver prototype
cache handling and the matching integration test. Build outputs, the upstream Git repository,
generated test indices, and the separately cloned Test262 suite remain excluded. Use
`git diff 4063038 -- js/brimstone` together with this note to audit the upstream refresh and all
local adaptation.

Current local adaptation:

- exactly-once, thread-affine `OwnedContext` plus lifetime-branded moving-GC root scopes;
- hidden explicit unsafe aliases for legacy raw context, handle, and heap-pointer APIs;
- corrected `HeapInfo` initialization, destruction, and ownership transfer during heap resize; and
- a drop-free scope-tree option bit which avoids leaking an `Rc<Options>` into its bump arena;
- exhaustive opcode metadata and a bounded defense-in-depth verifier for trusted compiler output;
- versioned, lifetime-branded generated-code ABI schemas plus deterministic hotness/interrupt
  primitives; and
- an off-by-default Linux x86-64 W^X cache and non-allocating boxed Cranelift proof using exact
  local Cranelift `0.134.3` source from Wasmtime v47.0.3.

The `baseline_jit` feature remains outside product dispatch. It has no generated helper calls,
native safepoints, GC-linked shadow frames, interpreter-resume side exits, backedge execution, or
untrusted-bytecode contract. Its shared dependency resolution aligns to the selected Wasmtime lock
and advances compatible Brimstone packages to bumpalo 3.20.2, libc 0.2.185, log 0.4.28, and
smallvec 1.15.1.

Upstream explicitly labels Brimstone as a work in progress and not ready for production. In
particular, Wild Buzzard must not expose the remaining raw context, handle, or heap-pointer APIs
across a browser boundary. W2-A2H is a conditional GO for contained JIT infrastructure work only;
untrusted content remains blocked on the deeper internal lifetime migration, limits, interruption,
host bindings, fuzzing/Miri where applicable, and all gates in `AGENTS.md`.

# Brimstone upstream snapshot

This directory was imported as an exact source snapshot of Brimstone and is now the editable
Wild Buzzard adaptation tree. Git commit `4063038` preserves the byte-identical upstream baseline;
later commits carry the separately reviewable local patches listed below. It is the pinned baseline
for the JavaScript execution-engine workstream, but it is not yet wired into the root workspace or
accepted as a production-safe browser runtime.

- Upstream repository: <https://github.com/Hans-Halverson/brimstone>
- Upstream default branch at import: `master`
- Commit: `b544eff181ef6a72639f26a89b6aca1f8d6e6b50`
- Commit date: `2026-08-09T01:42:39+00:00`
- Git tree: `9c2c7b675799b0a05aa11ec10fe5203fda4df339`
- Upstream tracked files: 1,554
- License: MIT (`LICENSE`)
- Import method: `git archive` of the pinned commit
- Exact-import Wild Buzzard commit: `4063038`
- Local adaptation patches: see below and `docs/upstream-components.toml`

The exact-import commit contains 1,554 byte-identical upstream files plus this note. Build outputs,
the upstream Git repository, generated test indices, and the separately cloned Test262 suite remain
excluded. Use `git diff 4063038 -- js/brimstone` to audit all local adaptation.

Current local adaptation:

- exactly-once, thread-affine `OwnedContext` plus lifetime-branded moving-GC root scopes;
- hidden explicit unsafe aliases for legacy raw context, handle, and heap-pointer APIs;
- corrected `HeapInfo` initialization, destruction, and ownership transfer during heap resize; and
- a drop-free scope-tree option bit which avoids leaking an `Rc<Options>` into its bump arena.

Upstream explicitly labels Brimstone as a work in progress and not ready for production. In
particular, Wild Buzzard must not expose the remaining raw context, handle, or heap-pointer APIs
across a browser boundary. W2-A2H is a conditional GO for contained JIT infrastructure work only;
untrusted content remains blocked on the deeper internal lifetime migration, limits, interruption,
host bindings, fuzzing/Miri where applicable, and all gates in `AGENTS.md`.

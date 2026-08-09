# Brimstone upstream snapshot

This directory contains an exact source snapshot of Brimstone for evaluation and later Wild
Buzzard adaptation. It is the pinned baseline for the JavaScript execution-engine workstream; it
is not yet wired into the root workspace or accepted as a production-safe browser runtime.

- Upstream repository: <https://github.com/Hans-Halverson/brimstone>
- Upstream default branch at import: `master`
- Commit: `b544eff181ef6a72639f26a89b6aca1f8d6e6b50`
- Commit date: `2026-08-09T01:42:39+00:00`
- Git tree: `9c2c7b675799b0a05aa11ec10fe5203fda4df339`
- Upstream tracked files: 1,554
- License: MIT (`LICENSE`)
- Import method: `git archive` of the pinned commit
- Local patches in this snapshot: none

Every file other than this note must compare byte-for-byte with the pinned upstream Git tree.
Build outputs, the Git repository, generated test indices, and the separately cloned Test262 suite
are intentionally excluded. Adaptation, ownership hardening, garbage-collector changes, browser
host bindings, and JIT work must be made in separately reviewable changes with updated provenance.

Upstream explicitly labels Brimstone as a work in progress and not ready for production. In
particular, Wild Buzzard must not expose its current safe-looking raw `Context`, handle, or heap
pointer APIs across a browser boundary until their ownership, rooting, aliasing, and teardown
contracts have been hardened and tested.

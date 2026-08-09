# W3-A3S atomic script mutation transaction

- Task: Add an engine-neutral, bounded transaction for applying a future JavaScript task's DOM
  mutations to one exact live document state.
- Owner: Agent 3 — Web platform, DOM, Stylo, and layout; accepted after orchestrator static review
  and locked component gates.
- Status: Complete for the transaction boundary. This is not a Brimstone binding, event loop,
  mutation-observer implementation, live style invalidation, script loader, or DOM/WPT parity
  result.
- Firefox reference: ESR153 `c19b7e89270787889495688244ec6ee8e79288a1`, especially
  `dom/base/nsINode.cpp`, `dom/base/Element.cpp`, `dom/base/CharacterData.cpp`, and the Node,
  Element, and CharacterData Web Platform Tests. Relevant history included
  `0ea6615be592`, `f080ca89a276`, and `5febf7f9e1b9` for pre-insertion and `insertBefore`
  behavior. Firefox remains behavioral reference only; no C++ implementation was copied.
- Wild Buzzard paths changed: `dom/src/bindings.rs` and
  `dom/tests/script_mutation.rs` only.

## Contract

`Document::apply_script_mutations` requires an exact `DocumentVersion` and one nonempty
`ScriptMutationBatch`. Commands can create HTML elements or text, append or insert nodes, set or
remove null-namespace HTML attributes, set character data, and remove children. Existing
`NodeId`s are checked against the exact arena. Newly created nodes use dense zero-based
`CreatedNodeToken`s which can refer only to an earlier successful create command in the same
batch.

The caller can narrow, but never enlarge, fixed process caps of 4,096 commands, 2,048 created
nodes, 1 MiB per UTF-8 string, and 4 MiB aggregate input string data. Every name, value, and text
field is charged with checked arithmetic. Errors identify the command index and preserve distinct
version, revision-exhaustion, limit, token, DOM, and finalization failures.

The implementation clones the arena only inside the method, retaining the exact document and
node identities while preventing a same-identity `Document` clone from escaping. All commands
run against that private copy. Any failure discards it without changing the caller's tree,
revision, or node-slot allocation. Success validates and snapshots the final state, replaces the
original exactly once, advances its revision exactly once regardless of command count, and
returns the one snapshot plus a deterministic token-to-`NodeId` mapping. An absent
`removeAttribute` remains a successful DOM no-op inside an accepted batch and still participates
in that one batch commit.

`NodeId` remains a lookup identity, not a garbage-collector root. The existing
`RootedNodeHandle`, `DomRootProvider`, and `DomRootTrace` contracts are unchanged. No runtime or
concrete JavaScript heap dependency was added.

The private arena-copy design is deliberately a correctness seam, not the eventual site-scale
mutation algorithm. Live browser integration must replace whole-arena copying with journaled or
otherwise transactional incremental mutation before claiming normal-page performance, while
preserving this public atomicity and version contract.

## Frozen source and test evidence

Final SHA-256 identities:

| Path | SHA-256 |
| --- | --- |
| `dom/src/bindings.rs` | `b7236e3893e973fac22491653c1778e65749a4c3e824ec2e1f749c5b42748b05` |
| `dom/tests/script_mutation.rs` | `5dcf986a20f4dd049a46f1d071b80b7b556a09462f454066958f4cbc9616b6ae` |

All build output stayed under
`/home/user/Documents/wildbuzzardbuilds/w3-a3s-script-mutations`. Exact-file formatting and
`git diff --check` passed. Locked Linux x86-64 package check and strict all-target Clippy passed
without warnings. The focused transaction suite passed 11 tests; the complete DOM package passed
24 tests (4 unit, 9 prior integration, and 11 transaction tests) with no failure or ignored test.
The locked release build and warning-denied no-dependency rustdoc gate also passed.

Coverage includes mixed successful mutations and exact order/mapping, stale and foreign versions,
foreign and unknown nodes, forward/gapped/duplicate tokens, cycles and hierarchy failures,
mid-batch rollback with no leaked arena slot, every resource limit, empty/zero-limit behavior,
absent-attribute removal, revision `u64::MAX` failure and `u64::MAX - 1` success, final-snapshot
failure, and preservation of the rooting traits.

No unsafe code, FFI, dependency, license, network endpoint, provider integration, telemetry, or
branding change was introduced.

## Next boundary

The next Web-platform/browser slice should retain a live document on the engine worker, apply this
transaction only to the current navigation and exact document version, then recompute Stylo,
layout, shaped text, and one composed frame before atomically publishing it. Brimstone host
bindings must later translate rooted wrapper operations into these commands without storing raw
DOM pointers in the moving JavaScript heap.

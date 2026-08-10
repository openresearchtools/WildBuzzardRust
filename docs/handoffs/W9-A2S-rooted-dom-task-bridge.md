# W9-A2S rooted Brimstone/DOM task bridge

- Task: connect the bounded Brimstone classic-script admission to the first real, rooted DOM
  operations across one browser-owned task and its explicit microtask checkpoint.
- Owners: Agent 2 runtime seam plus Agent 3 DOM adapter; workspace membership and lock integration
  were applied by the main orchestrator.
- Status: bounded trusted integration gate. This is **not** WebIDL/DOM parity, product JIT
  admission, or permission to execute untrusted pages.
- Supported target: Linux x86-64 only.

## Result

`BrowserScriptRealm<'realm>` remains constructible only through
`OwnedContext::with_browser_script_realm`'s higher-ranked callback. Its new host-aware classic and
checkpoint methods borrow one caller-owned `BrowserHostTask` synchronously. A private function
table erases that borrow only inside the exact `ContextCell`; an RAII guard validates and clears
the data identity before either the host borrow or realm brand can end. `OwnedContext` aborts rather
than destroy a context with an active erased host borrow. A separate per-call RAII seal rejects
synchronous host reentry before a second erased `&mut` can be reconstructed.

The JavaScript heap receives no DOM pointer, Rust pointer, `NodeId`, moving-GC handle, or raw
context. The internal frozen `__wildBuzzardDom` proof object returns only positive integers that
round-trip exactly through `Number`. Each integer combines a process-wide never-reused forty-bit
task generation and a bounded thirteen-bit root-table index. A concrete `RootedDomTask` retains the
corresponding `Arc`-owned `RootedDomNode`; tokens from a different task, unallocated indices,
retired documents, foreign documents, and unknown arena slots fail closed.

The internal proof surface is deliberately small:

- current document-node root and checked document-slot lookup;
- create HTML element and text nodes;
- append a rooted child;
- set a null-namespace HTML attribute;
- set character data on a text node.

Every accepted mutation is immediately applied as a one-command `ScriptMutationBatch` against the
task's exact current `DocumentVersion`. Successful calls update the task version and root newly
created nodes before returning to JavaScript. This preserves the synchronous successful prefix: a
later DOM rejection or JavaScript throw does not roll back earlier mutations. `finish_phase`
returns only scalar before/after version, command, and creation evidence; it does not defer DOM
publication.

One `RootedDomTask` remains alive across these observable phases:

1. synchronous classic-script evaluation and DOM calls;
2. return of the primary script outcome to the embedding for error reporting;
3. a caller-selected explicit host-aware microtask checkpoint;
4. synchronous DOM calls made by promise jobs using the same rooted task tokens.

No promise job runs in step 2. A thrown job preserves its prior DOM calls, is reduced to the
existing pointer-free value summary, and discards the remaining Brimstone jobs because the wider
HTML host error continuation does not yet exist.

## Failure and resource behavior

The bridge reuses the W8-A2R opcode, managed-allocation, recursion, job, wall-time, and external
interrupt policy. Browser admission remains present for the whole host-aware phase, so the
off-by-default baseline dispatcher cannot be selected and every report continues to state that JIT
execution was disabled.

The DOM adapter additionally applies the fixed `ScriptMutationLimits` across the complete browser
task, including lookup calls, creations, and all copied UTF-8 string bytes. Root-table growth,
command-vector creation, the final host UTF-8 result, and adapter-owned string copies use fallible
reservation. Valid surrogate pairs are converted to scalar UTF-8; lone surrogates are currently
rejected rather than silently corrupted. Allocation, task/document/version staleness,
cancellation, task-generation mismatch, and private lifecycle failures are non-catchable host
failures. Invalid arguments, unavailable nodes, and DOM operation rejections currently throw
`TypeError` as an explicit temporary stand-in for generated WebIDL and `DOMException`.

External interrupt or an ordinary runtime resource failure retires the DOM task and clears queued
Brimstone jobs, while already completed synchronous DOM calls remain published. An unexpected
engine or host panic unwinds and clears the erased host slot, retires the host task, then sets the
permanent browser-runtime poison. After poison is set, result construction touches only scalar
admission state; later classic/checkpoint calls do not inspect VM, GC, host, or task queues.
Cancellation is polled before erased-host installation or first-time binding publication. After
that poll, `validate_phase` checks the exact task/document version before any script instruction or
queued job can run, including phases whose JavaScript makes no DOM call.

## Firefox ESR153 evidence

Reference checkout: `firefox/` at `c19b7e89270787889495688244ec6ee8e79288a1`, inspected read-only
and never used as a build input.

- `dom/base/nsINode.cpp:3057-3451` and `dom/base/nsINode.h:2443-2469`: append validation,
  synchronous reparenting, and adoption behavior.
- `dom/base/Document.cpp:9023-9082,9154-9159` and `dom/webidl/Document.webidl:64-85`:
  `createElement`/`createTextNode` conversion and synchronous creation.
- `dom/base/Element.cpp:1787-1811,1912-1959` and `dom/webidl/Element.webidl:42-56`:
  attribute conversion, canonicalization, and reactions.
- `dom/base/CharacterData.cpp:105-145` and `dom/base/FragmentOrElement.cpp:1227-1245`:
  character data versus element `textContent` behavior.
- `dom/bindings/Codegen.py:9503-9515` and `dom/base/CustomElementRegistry.h`: synchronous
  `[CEReactions]` ordering.
- `dom/script/ScriptLoader.cpp:3211-3240,3831-3836`,
  `dom/script/JSExecutionUtils.cpp:31-41`, and
  `xpcom/base/CycleCollectedJSContext.cpp:823-1024,1174-1283`: primary classic-script error
  handling before the explicit microtask checkpoint and dying-global job cancellation.
- WPTs under `testing/web-platform/tests/dom/nodes/` and
  `testing/web-platform/tests/html/semantics/scripting-1/the-script-element/microtasks/`, including
  `evaluation-order-1.html` and `evaluation-order-1-throw.js`, plus
  `html/webappapis/scripting/event-loops/microtask_after_script.html`.

The adopted ordering invariant is `synchronous mutation -> primary classic-script error reporting
-> explicit checkpoint`. Module evaluation has different promise-based reporting and is outside
this gate.

## Changed source

- `js/brimstone/src/js/runtime/browser_host.rs`
- `js/brimstone/src/js/runtime/browser_script.rs`
- `js/brimstone/src/js/runtime/context.rs`
- `js/brimstone/src/js/runtime/mod.rs`
- `dom/src/bindings.rs`
- `dom/tests/script_mutation.rs`
- `dom/README.md`
- `dom/script_bridge/Cargo.toml`
- `dom/script_bridge/src/lib.rs`
- `dom/script_bridge/tests/rooted_task.rs`

The orchestrator-owned integration also excludes `dom/script_bridge` from the root workspace, adds
it to the independently locked `js/brimstone` workspace, and records only the two local path
packages in `js/brimstone/Cargo.lock`. No external dependency was added.

## Verification

All output is under `/home/user/Documents/wildbuzzardbuilds/w9-a2s-dom-bridge/`.

Accepted locked gates on Rust 1.95.0:

- full default Brimstone workspace: 42 `brimstone_core`, 11 parser/snapshot, one bridge unit, and
  five bridge integration tests passed, together with all zero-test and rustdoc targets;
- combined `baseline_jit,alloc_error,gc_stress_test,handle_stats`: 154 `brimstone_core`, one bridge
  unit, and seven bridge integration tests passed, including forced moving GC and fixed-heap OOM;
- root DOM package: four unit, nine mutation, and twelve script-mutation tests passed;
- strict `-D warnings` Clippy passed for `brimstone_core`, the bridge, and the DOM package;
- explicit edition-2024 `rustfmt --check` for every changed Rust source and `git diff --check`
  passed;
- default ASan/LSan (`detect_leaks=1`) passed 40 `brimstone_core`, one bridge unit, and five bridge
  integration tests with no sanitizer diagnostic.

The combined bridge test asserts zero native-JIT entries for both classic-script and checkpoint
reports. Independent hostile review returned **GO** after the dispatch-reentry, token-field,
install-failure queue, early-cancellation, and pre-execution task-validation regressions passed.

Frozen SHA-256 source identities:

```text
cacf5d4b11fb0870cde067cb3cbb213d3d539135cbbb2083e7b7f65bbbfe99ab  js/brimstone/src/js/runtime/browser_host.rs
973690e9ce1ab2026f28ef99617476d599b4db7d78fda055edeb15c610745702  js/brimstone/src/js/runtime/browser_script.rs
617b5b8226f71025098c060c190cf75fa39ca7cb479b018c5e6f3a163809a13f  js/brimstone/src/js/runtime/context.rs
48f94e58ab0ac21ba6ca703381e2aa452d887b3deb01afd7c7a7e695dac7832c  js/brimstone/src/js/runtime/mod.rs
6aad6122fff08c92c9f132d8d5ad7b6483f67f18a6d9dd6cf0217fdd0f1be1d7  dom/src/bindings.rs
5150b5bf8dbf412b889f6fe761fd597fe9b09bbc2f81ce492bb92b64bc17b76f  dom/tests/script_mutation.rs
f0abb6da0b91eec89c22e5dbed3aadb36825b73acba6f5ed06cf8b2435397f43  dom/README.md
69fac7bab6e57cae92e3bd27f70ba33c8dafd83679438d5e8399e7499df73fbd  dom/script_bridge/Cargo.toml
acbef628c2bbea61a058427bab8c81af9800cd943b369f60b4669332a91f1a58  dom/script_bridge/src/lib.rs
66d7413f4cee97188b433de655ed8700c22726e98ff3c202edce7b0acf34ffb4  dom/script_bridge/tests/rooted_task.rs
```

## Explicit non-goals and remaining blockers

- `__wildBuzzardDom` is an internal frozen proof object, not a public web API. There is no WebIDL
  generation, wrapper/prototype identity, overload resolution, `DOMException`, security wrapper,
  named property behavior, or complete DOM string conversion.
- Cross-document append still rejects instead of adopting. HTML/XML name validation, namespaces,
  `Element.textContent`, custom-element reactions, mutation observers, ranges, shadow DOM, events,
  live collections, and style/layout/frame invalidation are absent.
- Exact revision drift cancels this bounded task. Firefox uses responsible-global/current-document
  lifetime and can keep references to some detached old documents usable; this stricter rule is
  safety evidence, not parity.
- `ScriptMutationBatch` still clones the full arena, and Brimstone's pre-existing concat-string
  flattening uses infallibly growing system `Vec` storage before the bridge can perform its own
  fallible UTF-8 reservation. System-allocator exhaustion at either point is not recoverable
  through the current APIs. Replace the DOM clone with a journaled/fallible live mutation store and
  migrate string flattening to explicitly fallible storage before untrusted or normal-volume page
  use.
- The Brimstone raw-context/lifetime migration, general host wrapper tracing, HTML event loop,
  MutationObserver delivery, browser interrupt/resource policy, Test262/WPT breadth, fuzzing,
  sanitizers for the final integrated adapter, and general browser process integration remain open.

Accordingly the result is a bounded **GO** only for this trusted contained bridge and remains a
product/untrusted-page **NO-GO**.

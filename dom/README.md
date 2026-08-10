# Wild Buzzard DOM nucleus

`wild_buzzard_dom` is first-party Rust code. It owns nodes in a per-document arena and exposes
stable, document-scoped `NodeId` values. A node keeps the same ID while connected, reparented, or
detached; slots are not reused. Cross-document handles are rejected because automatic adoption
cannot preserve that contract without an explicit migration API.

Mutations validate the complete prospective child list before changing either parent. The current
document rules allow comments, at most one doctype before at most one document element, and no
text child. Elements accept elements, text, and comments. Text, comments, and doctypes are leaves.
Every successful creation or mutation advances a checked (non-wrapping) revision.

Layout consumes `DocumentSnapshot`, an owned preorder snapshot with cloned node data and an index;
it never borrows the mutable arena. `bindings.rs` defines the future DOM/JS contract:
`NodeId` is a lookup key, not a GC root, and only an embedding-provided `RootedNodeHandle` may keep
a document alive and participate in tracing.

Preorder collection, descendant text collection, and snapshot enumeration are iterative, so a
deep script-created tree does not consume the native call stack before layout applies its checked
depth policy.

`script_bridge/` is the first independently testable Brimstone adapter. One browser-owned task
retains `Arc`-backed `RootedDomNode` values across classic-script completion, host error reporting,
and the caller's later explicit microtask checkpoint. Brimstone receives only exact numeric tokens
containing a never-reused task generation and a bounded root-table index; it never stores a DOM or
Rust pointer in the moving JavaScript heap. Each accepted host call publishes synchronously through
a one-command `ScriptMutationBatch` against the task's exact `DocumentVersion`, so a later DOM or
JavaScript exception does not roll back the successful prefix. The adapter supports the current
document node, checked arena-slot lookup, HTML element/text creation, append, null-namespace HTML
attribute mutation, and character-data mutation.

## ESR153 and standards references inspected

Pinned reference: `firefox/` at `c19b7e89270787889495688244ec6ee8e79288a1` (read-only, never a
dependency).

- `dom/base/nsINode.h` and `dom/base/nsINode.cpp`, especially pre-insertion validity,
  `ReplaceOrInsertBefore`, removal, ancestry, and preorder queries.
- `dom/base/Document.h`, `dom/base/Document.cpp`, and `dom/base/DocumentType.{h,cpp}` for document
  child ownership and doctype state.
- `dom/base/Element.{h,cpp}` for namespace/name-based attribute replacement and removal.
- `dom/base/CharacterData.{h,cpp}` for text/comment data mutation.
- `testing/web-platform/tests/dom/nodes/Node-appendChild.html`.
- `testing/web-platform/tests/dom/nodes/Node-insertBefore.html`.
- `testing/web-platform/tests/dom/nodes/Node-removeChild.html`.
- `testing/web-platform/tests/dom/nodes/append-on-Document.html`.
- `testing/web-platform/tests/dom/nodes/Document-doctype.html`.
- `testing/web-platform/tests/dom/nodes/Element-setAttribute.html` and
  `Element-removeAttribute.html`.
- `testing/web-platform/tests/dom/nodes/Node-textContent.html`.
- `dom/webidl/Document.webidl`, `dom/webidl/Element.webidl`, and binding generation in
  `dom/bindings/Codegen.py` for synchronous WebIDL conversion and reaction ordering.
- `dom/script/ScriptLoader.cpp`, `dom/script/JSExecutionUtils.cpp`, and
  `xpcom/base/CycleCollectedJSContext.cpp` for classic-script error reporting before the explicit
  microtask checkpoint and for dying-global job cancellation.
- `testing/web-platform/tests/html/semantics/scripting-1/the-script-element/microtasks/` and
  `testing/web-platform/tests/html/webappapis/scripting/event-loops/` for script/error/microtask
  ordering.

History inspected with `git log --follow` and `git log -S`, including Firefox changes
`0ea6615be592` (spec-aligned pre-insertion checks), `1e553e2a8f09` (replace-children work), and
`1bcbdc1cd50f` (hardening against already-parented rebinding). Assertions were reimplemented in
new Rust tests; no Firefox source is copied or loaded.

## Wave-one tests

`tests/dom_mutation.rs` covers stable detached/reparented IDs, atomic cycle rejection, document
shape, insert/replace/sibling order, replacing with earlier and later same-parent siblings,
ordered attributes and document-order queries, cross-document rejection, snapshot isolation,
revision movement, and full invariant validation. It also exercises preorder, text, and snapshot
collection through a 1024-element chain.

## Explicit gaps

- No `DocumentFragment`, shadow tree, ranges, mutation observers, custom elements, events, or live
  collections.
- No cross-document adoption/import. It currently returns `WrongDocument` instead of silently
  changing ownership.
- No node destruction or reusable generational slots; detached nodes remain owned by the document.
- No WebIDL-generated surface or JS wrapper implementation. The rooting capability is only the
  reviewed host contract plus an internal `__wildBuzzardDom` proof object; it is not a public web
  API or a claim of DOMException/WebIDL parity.
- Attribute syntax validation and XML namespace constraints are not yet complete Web DOM behavior.
- Query selectors, class-name collections, tree scopes, slots, and composed-tree order are absent.
- The bridge's `setText` mutates character data only; it is not `Element.textContent` replacement.
- Cross-document append rejects rather than adopting, custom-element reactions and mutation
  observers are absent, and lone-surrogate DOM strings remain rejected at this contained seam.
- Exact revision drift and responsible-document retirement cancel the bounded task. Firefox keeps
  some retained detached-document references usable, so this stricter rule is not parity evidence.
- The arena-copy transaction is synchronous and correctness-first; it is not yet suitable for
  normal-site mutation volume, and system-allocator exhaustion inside full-arena cloning remains a
  product NO-GO until the DOM journal/storage layer becomes explicitly fallible.
- Brimstone concat-string flattening can also grow system `Vec` storage infallibly before the
  adapter's exact fallible UTF-8 reservation. That engine-level allocation path remains a separate
  product NO-GO.

This package is integrated into the root workspace. A later shared engine facade will own
documents; the DOM crate does not need or permit a dependency on `firefox/`.

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
  reviewed host contract for that future adapter.
- Attribute syntax validation and XML namespace constraints are not yet complete Web DOM behavior.
- Query selectors, class-name collections, tree scopes, slots, and composed-tree order are absent.

This package is integrated into the root workspace. A later shared engine facade will own
documents; the DOM crate does not need or permit a dependency on `firefox/`.

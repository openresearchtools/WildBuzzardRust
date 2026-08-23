# W9-A3Q/C3: sealed parser-to-rooted-DOM publication typestate

- Owner: main orchestrator, DOM/JavaScript integration boundary
- Working-tree base: `8eefa01f`
- Firefox ESR153 reference: `c19b7e89270787889495688244ec6ee8e79288a1`
- Status: accepted after independent hostile C3 review and R2 correction confirmation
- Product admission: **NO-GO for general or untrusted web content**

## Outcome

One exact Rust `Document` can alternate between `HtmlParser` and one `RootedDomTask` while one
Brimstone context, realm, installed host authority, document budget, global environment, and root
set survive every parser boundary.

C3 also seals publication ordering in safe Rust. A caller can no longer restore a parser document,
snapshot or publish it early, skip a required checkpoint, or return it to parsing through a direct
method.

## Ownership and lease identity

`RootedDomTask::lend_document_to_parser` replaces the hosted arena with a private placeholder and
returns the real `Document` plus one non-`Clone`, non-`Copy` `ParserDocumentLease`. The lease is
bound to:

- exact `ScriptDocument` `Arc` identity;
- exact rooted-task generation and monotone sequence;
- exact before-version and document identity; and
- the mirrored set of all retained rooted `NodeId` values.

Restoration additionally consumes the parser's opaque `ParserInsertedScript`, requiring its exact
document version and one-based ordinal to match the lease sequence. Final restoration consumes the
whole `ParseOutput`, including its private completion version and completed-boundary count.
Identity, residency, sequence, monotone version, quiescence, and every root are verified before
swapping the real arena into the host. Cross-document, stale, wrong-version, duplicate,
nonquiescent, missing-root, replayed-boundary, and ignored-boundary attempts fail closed.

## Sealed phase protocol

The public safe path is now:

```text
ParserLent
  -> RestoredParserDocument
  -> perform_pre_checkpoint
  -> PreparedParserDocument
       -> execute_classic -> ExecutedParserDocument -> perform_post_checkpoint
       -> skip (no fabricated post checkpoint)
  -> CompletedParserDocument
  -> lend_back_to_parser
  -> ParserLent

final ParserLent
  -> RestoredParserCompletion
  -> perform_final_checkpoint
  -> PublishedParserDocument
```

Only `PreparedParserDocument` exposes the internal live snapshot used for script preparation, and
only after a complete pre-checkpoint with a completed host phase. Only
`CompletedParserDocument` can return the arena to parsing. An admitted classic always requires a
complete post-checkpoint; a classified skipped candidate is accounted in the cumulative budget
without inventing a post phase. Only `PublishedParserDocument` makes final public snapshot access
available.

`ScriptDocument::snapshot`, `current_version`, external mutation, root acquisition, and new task
entry reject throughout parser ownership and all unpublished host phases. Direct hosted realm
calls made outside the current typestate transition are rejected by the rooted task's phase
admission and retire the authority.

## Drop, unwind, and terminal failure

- Dropping any boundary typestate before `lend_back_to_parser` swaps the real document back to the
  parser placeholder, retires the rooted task and document, and publishes nothing.
- Dropping `RestoredParserCompletion` before a successful final checkpoint permanently retires the
  hosted document; there is no public snapshot.
- A poisoned authority/document mutex during mandatory recovery aborts instead of returning an
  unproved mutable owner.
- Host, checkpoint, classic-script, skipped-accounting, cancellation, deadline, resource, and
  runtime failures retain their typed disposition and cannot reopen a lease.
- `mem::forget` can leak an unpublished guard but cannot create publication or a second mutable
  owner through safe APIs.

No `unsafe` code or native boundary was added. The parser remains independent of the bridge and
Brimstone; the bridge now has a normal path dependency on the parser because the nonforgeable
boundary and completion types are part of its production-safe lease API.

## Executed proof

The integration test parses two explicit scripts under one
`with_hosted_document_script_budget` callback. Script one creates and roots DOM nodes and schedules
microtask mutations. The parser resumes, creates intervening markup, and script two observes the
same realm, globals, and roots before mutating both script-created and parser-created nodes. A
final checkpoint publishes the original `DocumentId` at the exact final revision.

Negative tests cover cross-document and wrong-version restore, nonpristine lending, boundary guard
drop, omitted final checkpoint, stale task tokens, external version drift, retired documents,
recoverable throws, and successful-prefix preservation.

## Verification

- Debug: 1 library + 11 integration tests passed.
- Release: 1 library + 11 integration tests passed.
- Strict all-target Clippy passed with `-D warnings`.
- Warning-denied rustdoc passed after correcting the typestate links.
- Exact-file rustfmt completed and all tests were rerun.
- Independent hostile review passed 10/10 probes in debug and release; eight safe-API
  replay/forgery probes failed to compile as required.
- R1's only finding was three test-only `drop_non_drop` diagnostics. Lexical scopes corrected
  them without changing the intentional restored-guard drop, and independent R2 confirmation
  passed format, 1+11 default tests, and strict all-target Clippy.

Evidence targets are under:

```text
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a3pq-c2-bridge/
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a3pq-c3-r1-review/
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a3pq-c3-r2-review/
```

## Explicit exclusions

The bridge does not classify scripts, fetch external resources, implement modules, provide WebIDL
`Window`/`Document`, schedule browser tasks, bind navigation generations, enforce CSP/principals,
or trigger style/layout/rendering. It proves one contained ownership/order boundary; it does not
claim that the live application or YouTube renders.

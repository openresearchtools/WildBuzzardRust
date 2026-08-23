# W9-A3P/C3: parser-inserted script boundary authority

- Owner: main orchestrator, parser/DOM integration lane
- Working-tree base: `8eefa01f` (`Integrate bounded stylesheet navigation fetch`)
- Firefox ESR153 reference: `c19b7e89270787889495688244ec6ee8e79288a1`
- Status: accepted after independent hostile C3 review and R2 correction confirmation
- Product admission: **NO-GO for general or untrusted web content**

## Outcome

`HtmlParser` can synchronously suspend immediately after an explicit parser-inserted `</script>`
end tag and before applying the following token. The browser handler receives the exact live
`Document`, exact script `NodeId`, boundary `DocumentVersion`, closing span, and an immutable
snapshot of execution-affecting start-tag state.

Each successfully returned handler call consumes a nonforgeable, one-based boundary ordinal.
`ParseOutput` privately retains the exact finished document version and completed-boundary count;
the DOM lease must consume that whole output before final publication. Ignoring or losing a closed
script boundary therefore cannot be converted into a publishable completion.

This is an ordering and ownership seam. It does not execute JavaScript, fetch scripts, or make the
current static engine path dynamic.

## Corrected start-tag contract

The first hostile review rejected the original boundary because it read execution attributes from
the live element after the pre-script checkpoint. Firefox freezes those inputs when the start tag
is inserted. C2 now records `ParserScriptStartTag` at insertion with:

- opening span and the first applicable prior `<base href>`;
- `src`, `type`, `language`, `charset`, `crossorigin`, `integrity`, `nonce`,
  `referrerpolicy`, `fetchpriority`, and `blocking`; and
- presence bits for `async`, `defer`, and `nomodule`.

Its custom `Debug` output reports only presence and booleans, so source URLs, integrity values,
and nonces are not copied into diagnostics.

Inline source is deliberately not in that snapshot. The browser must perform the pre-script
microtask checkpoint first and then read text from the exact live script node. Therefore:

```text
start tag       freeze execution attributes/base
end tag         suspend parser; following tokens remain unapplied
pre checkpoint  may mutate the live script text
preparation     reads live text + frozen start-tag execution state
```

DOM mutation during the pre-checkpoint can change inline text but cannot replace the already
frozen script kind, URL, nonce, integrity, flags, or base.

The first applicable `<base href>` is selected from the live document in tree order when each
script start tag is inserted. A preceding completed script can therefore add or change the first
base observed by a later script, while mutation during that later script's own pre-checkpoint
cannot alter its already frozen base.

## Malformed EOF behavior

An EOF-unclosed `<script>` remains in the DOM with its text and records a tree-builder
`EofInScript` diagnostic. It is malformed and never produces `ParserInsertedScript`; no script
handler runs. This corrects the first implementation, which incorrectly synthesized an execution
boundary at EOF.

## Lifecycle and failure behavior

The parser lifecycle remains:

```text
Active -> ScriptHandlerActive -> Active -> ... -> Finished
                              \-> ScriptHandlerAborted
```

- Handler success resumes with the next token.
- Handler error returns the exact typed error and permanently seals later parser entry.
- Handler unwind leaves a non-reusable aborted state.
- No following markup is applied after handler failure or unwind.
- Empty and split explicit script boundaries are delivered once in DOM order.
- The handler cannot re-enter the parser because it receives only `&mut Document`.

`feed` and `finish` retain their infallible no-op handler behavior. `from_pristine_document`
preserves one caller-owned untouched arena and rejects any prior detached or connected mutation.

## Verification

All authoritative commands used the Data-drive Podman wrapper, disabled networking, read-only
source mounts for builds, Rust 1.95, and Cargo/target/temp trees below
`/run/media/user/Data/Repositories/wildbuzzardbuilds/`.

- Debug: 9 tokenizer + 18 tree-builder tests passed.
- Release: 9 tokenizer + 18 tree-builder tests passed.
- Strict all-target Clippy passed with `-D warnings`.
- Warning-denied rustdoc passed.
- Exact-file rustfmt completed and tests were rerun afterward.
- Independent hostile review passed 10/10 debug and release probes, including earlier-script base
  mutation and ignored-boundary publication rejection; eight forgery/replay probes failed to
  compile as required.
- The sole R1 finding was three test-only `drop_non_drop` Clippy diagnostics. Lexical scopes
  replaced those explicit closure drops without changing the intentional restored-guard drop;
  independent R2 confirmation and the full default/strict-Clippy gates passed.

Regressions cover every two-chunk boundary, bytewise input, head/body scripts, live text mutation,
all frozen execution fields, redacted debug output, first-base selection, explicit empty scripts,
malformed EOF nonexecution, handler error/unwind, and caller-owned document identity.

Evidence targets are under:

```text
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a3pq-c2-parser/
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a3pq-c3-r1-review/
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a3pq-c3-r2-review/
```

## Explicit exclusions

This slice does not implement external scripts, modules, import maps, `document.write`, dynamic
scripts, event handlers, workers, timers, navigation integration, or rerender scheduling. It does
not claim JavaScript-heavy-site behavior, YouTube rendering, browser security readiness, or
Firefox parity. Those require the corrected lease/typestate and coordinator gates plus engine and
event-loop integration.

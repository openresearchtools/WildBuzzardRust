# W9-A2T: parser-blocking inline classic-script loop design

- Status: **DESIGN READY — conditional GO for a contained proof**
- Product admission: **NO-GO for ordinary web content**
- Audit base: `8190c418f3b251cd1ea5971dffe4cbf61f4bbeeb`
- Firefox reference: ESR153 commit `c19b7e89270787889495688244ec6ee8e79288a1`

## Decision

The existing rooted Brimstone/DOM task can become a real parser-script integration proof only if
HTML parsing genuinely suspends at every parser-inserted inline classic script. The same DOM task
and the same Brimstone realm must survive every script in that document. Parsing the complete
document first, creating a VM per script, or creating a VM per tab would establish neither browser
ordering nor acceptable resource ownership.

General-web script execution remains prohibited. The browser does not yet retain and enforce the
headers, CSP, sandbox, principal, Trusted Types, and process/realm isolation required to run
untrusted fetched JavaScript. The first gate must be explicit, disabled by default, and restricted
to deterministic numeric-loopback/test documents.

## Acyclic architecture

```text
dom <- parser
dom + brimstone <- dom/script_bridge
parser + script_bridge + brimstone + layout/render <- wild_buzzard_engine
```

The parser never depends on Brimstone. Brimstone never depends on the concrete DOM. The browser
engine owns the thread-affine realm and coordinates parsing, script execution, microtasks,
cancellation, and final rendering. The generic `ManualTaskQueue` is a `Send` closure queue and is
not the owner-thread HTML/JavaScript event loop.

The proposed browser-owned state is conceptually:

```rust
struct DocumentTaskKey {
    navigation: NavigationId,
    document: DocumentId,
    parser_task: NonZeroU64,
}

struct ParserScriptBoundary {
    ordinal: u32,
    element: NodeId,
    source_span: Range<usize>,
    end_span: Range<usize>,
}

enum ParserProgress {
    Script {
        boundary: ParserScriptBoundary,
        continuation: SuspendedHtmlParser,
    },
    Complete(ParserCompletion),
}

struct ScriptedDocumentTask {
    key: DocumentTaskKey,
    state: DocumentTaskState,
    parser: SuspendedHtmlParser,
    document: ScriptDocument,
    host_task: RootedDomTask,
    realm: BrowserScriptRealm,
    budget: DocumentScriptBudget,
    dirty_version: Option<DocumentVersion>,
}

enum DocumentTaskState {
    Parsing,
    PreScriptCheckpoint,
    PreparingScript,
    RunningClassic,
    ReportingPrimaryError,
    PostScriptCheckpoint,
    ResumingParser,
    FinalRendering,
    Complete,
    Cancelled,
    Failed,
    Poisoned,
}
```

## Exact ownership seam

`HtmlParser`, `LiveDocumentPage`, and `ScriptDocument` currently own incompatible `Document`
forms. The bridge therefore needs a sealed parser lease rather than ordinary externally visible
version drift:

```rust
RootedDomTask::lend_document_to_parser() -> ParserDocumentLease
ParserDocumentLease::with_document_mut(...)
RootedDomTask::restore_after_parser(ParserDocumentLease) -> ParserAdvance
RootedDomTask::accept_parser_advance(ParserAdvance)
```

The proof binds navigation generation, document identity, parser-task identity, host-task
generation, lease sequence, and exact before/after versions. Restore revalidates every retained
rooted `NodeId`. Only this sealed advance permits the parser to change the task's expected version;
ordinary external drift must continue to fail closed.

One `RootedDomTask` spans the entire parser task. A new task per script would invalidate DOM tokens
held in JavaScript globals and reset cumulative host budgets. One `BrowserScriptRealm` likewise
survives the document so globals and queued work have the correct lifetime.

## Parser/script sequence

At each parser-inserted `</script>` boundary:

1. Suspend tree construction before applying any following token.
2. Restore the exact leased document into `ScriptDocument`.
3. Validate navigation generation, document identity, cancellation, and absolute deadline.
4. Run the pre-script microtask checkpoint.
5. Freeze source text and relevant attributes from the now-current live script element.
6. Classify the script.
7. Execute an admitted nonempty classic synchronously in the existing realm.
8. Record a recoverable primary parse/compile/runtime error before the next checkpoint.
9. Run the post-script microtask checkpoint.
10. Lend the exact document back to the suspended parser and resume.
11. Repeat with the same realm, rooted host task, and cumulative budget.
12. After parser completion, snapshot the final version and run style, layout, composition, and
    rendering before publishing the navigation.

Source is frozen after the pre-checkpoint because that checkpoint can mutate the script element.
An excluded external/module/unsupported script gets the pre-checkpoint but no fabricated execution
checkpoint. An admitted empty classic gets the empty-script checkpoint required by Firefox
behavior.

No intermediate paint is required inside this initial parser task, but the first published frame
must represent the final post-script document version. Later event tasks and dynamic DOM changes
need a separate coalesced rerender queue.

## Initial admission

Admit only parser-inserted inline classic scripts:

- no `src` attribute, including an empty `src`;
- no `nomodule`;
- `type` absent or exactly empty, with exact ASCII-case-insensitive `text/javascript` as an
  optional explicitly tested addition;
- inline `async` and `defer` have no scheduling effect; and
- one script-enabled document and one initial Brimstone realm in the worker.

Explicitly exclude and diagnose external scripts, modules, import maps, dynamically inserted
scripts, `document.write`, timers, event handlers, MutationObservers, custom elements, workers,
real Window/WebIDL bindings, CSP/sandbox/principals, and simultaneous script-enabled browsing
contexts. Parser script escaped-state cases not yet tokenized correctly also remain excluded.
Product JIT admission remains off; this gate must record zero native JIT entries.

Unsupported constructs produce bounded deterministic records. They must never be silently treated
as executed or as parity.

## Cancellation, errors, and budgets

Navigation cancellation must interrupt running JavaScript, not merely wait for a script boundary:

```rust
struct NavigationExecutionControl {
    cancellation: CancellationToken,
    script_interrupt: ScriptInterruptHandle,
}
```

Cancelling an active navigation signals both handles. Check the one absolute deadline before and
after every parser, script, checkpoint, and render phase; derive each Brimstone wall-time allowance
from the remaining deadline.

Thrown values and recoverable parse/analysis/compile errors are recorded, followed by the
post-script checkpoint and parser continuation. Cancellation, deadline, opcode/allocation/job
limits, host or identity/version failure, runtime busy/poison/panic, failed lease restore, and a
thrown microtask job are terminal for the candidate navigation. Microtask throws are terminal in
this first gate because Brimstone clears the remaining queue and Wild Buzzard has no browser error
event behavior to continue accurately. A poisoned or panicked engine retires the scripted lane.

In addition to existing per-admission limits, start deterministic testing with cumulative caps of
64 script candidates, 4 MiB inline source, 10 million opcodes, 64 MiB managed allocation requests,
10,000 jobs, 128 diagnostics, and a 64 MiB JS heap. Preflight the heap minimum needed for realm
initialization; do not silently weaken the maximum.

## Ownership and implementation order

Agent 3 owns the resumable parser contract in `parser/src/tree_builder.rs`, `parser/src/lib.rs`,
parser tests, and parser documentation. Agents 2 and 3 jointly review the sealed lease in
`dom/script_bridge/src/lib.rs`, its tests, and DOM documentation. Agent 6 owns new
`browser/wild_buzzard_engine/src/script_loop.rs` plus navigation/pipeline/dynamic/error/lib
integration, engine tests, and documentation. The orchestrator owns manifests, lock/toolchain
changes, this handoff, final integration, review, commits, and cleanup.

Brimstone should need no modification for this gate: the rooted host, explicit checkpoint,
interrupt handle, and result contracts exist. Any discovered deficiency is confined to
`js/brimstone/src/js/runtime/browser_script.rs`, `browser_host.rs`, and their tests, with Agent 2
ownership. The selected Brimstone/Wasmtime baseline also requires the browser-engine integration
to remain compatible with Rust 1.94.

Implement in this order:

1. Freeze and independently review the resumable parser/continuation boundary.
2. Add and hostile-test the sealed parser lease and cumulative rooted-host task behavior.
3. Add the disabled-by-default browser coordinator and dual cancellation/deadline control.
4. Prove final-version rerender and stale-navigation suppression on deterministic loopback input.
5. Only after separate security/isolation gates consider broadening script admission.

## Required evidence

Parser tests cover immediate suspension before following markup, byte-by-byte and relevant chunk
splits, multiple/empty/head/body scripts, malformed endings and raw-text characters, final DOM
equivalence after all resumes, wrong-document/cross-paired/duplicate/stale continuations, and
bounded retained tokens/spans.

Bridge tests prove a script-1 token remains valid after parser continuation and in script 2;
sealed parser advances work while ordinary drift fails; foreign/duplicate/generation-mismatched or
cancelled restores fail; removal of a rooted node is detected; and all host budgets remain
cumulative.

Browser tests prove script-1 mutations and promise jobs are visible to script 2 in the required
order; neither script sees future parser markup; primary errors precede the post-script checkpoint;
script classification is exact; cancellation interrupts an infinite loop; superseding navigation
cannot publish; every budget fails typed; the first frame matches the final version and visibly
contains script mutations; poison retires the worker; and exactly one `OwnedContext` is created.

Behavioral references include WPT `microtask_after_script.html`,
`microtasks/evaluation-order-1.html`, `emptyish-script-elements.html`, execution-timing `001.html`,
the empty/whitespace type cases, and inline-classic `nomodule`. These assertions are derived tests,
not a claim that browser WPTs can run before Window/Document/event bindings exist.

## Stop conditions and cleanup

Reject an implementation that parses the full document before scripts, creates a new host task or
realm per script/tab, admits general-network code, checks cancellation only between scripts,
resumes before the post-script checkpoint, accepts arbitrary version drift, publishes a pre-script
frame, reverses the dependency direction, or claims browser event-loop/security/JavaScript parity.

The design audit edited no live source, created no worktree, ran no build, and retained zero bytes
of external artifacts.

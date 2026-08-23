# W9-A6M / W9-A6N / W9-A6N-C1 / W9-A6N-C2 — embedded authenticated WebDriver Classic

Date frozen: 2026-08-23 (Europe/London)

Original W9 automation work base observed at task start:
`56f09d3aa71b38575ce1c73aa18540719dd79527`.

Actual shared integration/freeze `HEAD` observed after concurrent lane
integration and again before the final C2 diff/hash checks:
`5739fa22359919b86a4bda4771fd6ac367592884` (`feat: capture browser
compositor frames`, committed 2026-08-23T16:38:21+01:00).

Read-only Firefox reference:
`firefox/` at ESR153 pin
`c19b7e89270787889495688244ec6ee8e79288a1`.

W9-A6N supersedes the W9-A6M hostile-NO-GO portions of this handoff.
W9-A6N-C1 then superseded the four neutral-review findings concerning ingress
disconnect revocation, post-completion response delivery, the shell-local lock
delta, and the advertised page-load timeout. The supported five-command slice
remains unchanged. W9-A6N-C2 supersedes C1's response-delivery scheduling and
presentation-commit race claims; C1 evidence remains historical and is not used
as C2 race evidence.

W9-A6N-C1's exact corrections are:

- `AutomationIngress` now takes and synchronously revokes the exact active
  authority and clears generation/session identity in the command-channel
  `Disconnected` branch itself, before returning the error.
- Dispatcher-to-worker response delivery has shared Pending/Delivered/Abandoned
  state. The dispatcher publishes the exact returned session authority and the
  worker must acknowledge a complete bounded socket write. EOF, absolute
  timeout, shutdown, channel loss, unwind, or guard drop revokes both request
  and returned-session authority even after command completion.
- `browser/wild_buzzard_shell/Cargo.lock` contains only the automation feature
  closure relative to the observed repository lock; no pre-existing package
  version was replaced or upgraded.
- New Session advertises `timeouts.pageLoad` as the configured bounded request
  deadline: 30,000 ms by default and at most 120,000 ms. It no longer advertises
  unsupported 300,000 ms semantics.

W9-A6N-C2's exact corrections are:

- A completed handler response remains a dispatcher scheduling barrier while
  terminal socket delivery is Pending. The dispatcher cannot dequeue or admit
  a later session command until the worker reports Delivered or Abandoned.
  Abandoned synchronously revokes and tears down the exact published session
  before the dispatcher can continue. Publishing an already-cancelled session
  authority fails closed.
- Every command that can mutate established browser automation state is
  admitted against both its request lifetime and the exact active-session
  authority in one pointer-ordered dual-lock transition. New Session has no
  prior session authority; publishing its returned authority is nevertheless
  rejected if it was already cancelled.
- Native presentation now carries an exact tab/navigation/document/frame-lease/
  scene-revision permit. Request cancellation, exact-session revocation, and
  native submission serialize on the permit's explicit terminal state. If
  cancellation wins, no draw/submission or shell presentation state mutation
  runs. If native submission wins, the successful swap and shell acceptance are
  recorded once; no impossible post-swap rollback is claimed, and the revoked
  session cannot perform later owner mutation.
- The deadline handoff is stated precisely: header/auth/body admission consumes
  the connection worker's absolute `Instant`; `DispatchLifetime` is constructed
  only after authenticated header parsing, with that same `Instant`, so no
  inner budget is created.

## Outcome

The real Wild Buzzard executable has a default-off embedded WebDriver Classic
endpoint. It reuses the Rust `testing/webdriver` protocol/server crate and does
not launch, wrap, or adapt geckodriver.

Implemented commands:

- Status.
- New Session, with at most one active automation session.
- Delete Session.
- Navigate To on the real active tab through `BrowserCommand::Navigate`.
- Get Current URL from canonical committed history.

Navigate returns only after the exact `NavigationId` is Ready, remains the
active tab's live navigation, has a committed URL, and is named by the matching
successful native composition receipt. Redirect completion therefore reports
the final committed URL. Editable address-bar draft state is never returned as
Current URL.

The `webdriver` feature is default-off. Without the feature, the automation
module, CLI options, and listener are absent. With the feature, no socket opens
unless the caller supplies both an explicit nonzero loopback `IP:PORT` and
exactly one admitted token source.

## Stable wire assignment

The orchestrator-owned registry records:

- protocol `9`: automation;
- kind `1`: `automation.command.v1`;
- kind `2`: `automation.result.v1`.

`docs/wire-registry.toml` contains the approved assignment. Neither
W9-A6N-C1 nor W9-A6N-C2 altered it. Its observed SHA-256 at this freeze is
`3cec01011cda23cba7412ab01968b9c80b390423b0be99c24369b84cb984d923`.

Every same-process command/result carries the protocol and kind plus a
nonzero, monotonic, never-reused `u64` request ID and session generation.
Exhaustion fails closed; neither identity wraps. Results must match the exact
protocol, kind, request, and generation. Session IDs are random 128-bit
UUID-v4-shaped lowercase hexadecimal values read from `/dev/urandom`, are
bounded to 64 bytes at ingress, and must exactly match the active authority.

The browser-owner queue has depth 16 and drains at most eight commands per
event-loop wake. `BrowserHandler` alone owns and mutates `BrowserSession`;
server and connection threads hold no synthetic browser state.

## One absolute request lifetime

Each fixed connection worker creates one monotonic absolute deadline `Instant`
when it begins a request. Header parsing, authentication, route validation, and
authenticated body admission consume that exact `Instant` directly.
`DispatchLifetime` is instantiated only after authenticated header parsing and
is given the same absolute `Instant`; it does not exist during pre-auth header
admission and it does not start a fresh budget. That lifetime is then shared by:

- authenticated body completion;
- dispatcher queue admission and wait;
- dispatcher prevalidation;
- the browser ingress;
- the event-loop owner command;
- pending navigation;
- exact compositor-receipt completion;
- response correlation.

No dispatcher, ingress, owner, or navigation stage starts a fresh inner budget.
The lifetime has Active, Completed, and Cancelled phases protected by one
poison-recovering mutex. Cancellation/expiry and completion arbitrate against
that same state.

The dispatcher rejects a cancelled or expired request before session
validation or handler entry. Browser-authority publication, Delete Session
mutation, and real navigation dispatch/pending-registration use
`run_if_active`: a short, local, nonblocking transition under the same
lifetime lock. Thus an external timeout cannot be reported while one of those
transitions is executing. Pending navigation repeatedly observes the same
lifetime and cannot complete from a late or stale compositor receipt after
cancellation.

The regression
`queued_stateful_request_expiring_behind_blocker_never_reaches_handler`
blocks the dispatcher with command A, queues stateful command B, lets B's
client lifetime expire, releases A, and proves B never reaches the handler or
mutates session state. A later fresh New Session succeeds.

`near_expiry_handler_inherits_outer_deadline_and_fresh_session_recovers`
admits a command near expiry, proves the handler sees the small remaining
outer budget rather than a fresh deadline, proves no late mutation, and then
opens a fresh session. The server unit
`cancellation_cannot_overtake_an_admitted_owner_transition` proves exact
lock arbitration. Shell owner regressions prove an expired queued session does
not publish authority and a cancelled navigation cannot complete from a late
composition.

Client disconnect is polled at most every 20 ms while waiting for a dispatcher
result. The worker reserves at most 20 ms, or half of the then-remaining budget
when smaller, for terminal response delivery *inside* the same absolute
deadline. Entering that reserve abandons dispatcher waiting and synchronously
revokes the request/session authorities; it does not create a fresh command
budget. This preserves bounded timeout-error delivery without allowing a timed
out command to execute later.

Every authenticated dispatch carries a shared response-delivery record. The
dispatcher publishes both pre-existing active authority and any newly returned
session authority into that record before transferring the result. The worker
owns an armed guard and disarms it only after the entire HTTP response is
written within the original absolute deadline. HTTP timeout, shutdown,
response-channel loss, peer EOF, write timeout/error, unwind, and guard drop
mark delivery Abandoned and call `revoke()`, which also cancels a lifetime that
already reached Completed. If abandonment wins before late publication, the
dispatcher synchronously revokes the subsequently published exact authority
and tears the session down.

After handler completion, Pending delivery is also a dispatcher scheduling
barrier. The dispatcher boundedly waits against the same absolute deadline and
shutdown flag before reading another queue entry. Delivered permits the next
entry. Abandoned first revokes the request and exact published session,
performs idempotent session teardown, and only then permits another dequeue.
An already-cancelled returned authority cannot be published. Consequently a
queued command B cannot reach session validation or the browser owner while a
completed New Session A still has an unresolved terminal socket delivery.

The deterministic regressions
`post_completion_client_eof_revokes_exact_session_and_fresh_session_recovers`
and
`post_completion_socket_timeout_revokes_exact_session_and_fresh_session_recovers`
both wait until the session lifetime is Completed, then independently force
peer EOF or an open-socket final-write timeout. Both prove exact cancellation,
exactly-once teardown, and successful fresh-session recovery. The server unit
`completed_response_delivery_requires_ack_and_revokes_late_publication` covers
abandon-before-publication, abandon-after-completion, and successful delivery
acknowledgement. C2 additionally adds
`pending_terminal_delivery_gates_queued_session_command_and_recovers` and
extends the exact EOF integration race so A is a completed New Session with
delivery Pending, B is already queued, A is Abandoned, B is rejected without a
handler/owner mutation, and a later fresh session recovers.

## Fail-closed, non-droppable revocation

Ingress and browser owner retain the same `DispatchLifetime` as the exact
generation/session authority. Teardown takes the ingress tuple and
synchronously calls `revoke()` on that shared authority before request
allocation, best-effort queueing, or wake delivery. A queued `Revoke` is
ordered cleanup evidence, not the security boundary.

Deterministic results:

- Queue full: the exact shared authority is already Cancelled before
  `try_send` can return Full. The owner observes cancellation, cancels any
  pending navigation, retires stale owner authority, and accepts a later fresh
  never-reused generation. No unbounded blocking occurs.
- Owner command channel disconnected: the exact `command_send.try_send`
  `Disconnected` branch takes and synchronously revokes active authority,
  clears ingress generation/session/authority identity, closes ingress, and
  returns without a wake or unbounded wait. Because that owner is gone,
  recovery is by a fresh server/owner pair; no stale authority survives.
- New Session result receiver disconnected: owner revokes the newly published
  exact authority and a later generation recovers.
- Normal Delete, shutdown, response-send failure, dispatcher disconnect,
  handler panic, teardown panic, and `Dispatcher::drop` all revoke first and
  clear state idempotently.

The queue/disconnect tests are:

- `queue_full_teardown_revokes_shared_authority_and_fresh_session_recovers`;
- `disconnected_revoke_still_cancels_exact_authority_and_closes_ingress`;
- `command_send_disconnected_branch_revokes_and_clears_exact_ingress_authority`;
- `disconnected_new_session_result_retires_owner_and_next_generation_recovers`;
- `expired_queued_session_never_mutates_owner_and_fresh_generation_recovers`;
- `cancelled_navigation_authorities_block_late_composition_and_allow_recovery`.

The dispatcher catches handler panic only long enough to revoke the current
request and active session authorities and attempt teardown, then resumes the
panic so `Listener::shutdown` reports dispatcher failure. Teardown state is
cleared before callbacks, and callback panic cannot make the authority usable
again. `Dispatcher::drop` independently revokes before any best-effort
teardown. The injected handler-plus-teardown panic regression proves immediate
authority cancellation, listener failure reporting, rejection of late
navigation, and recovery with a fresh server/session.

## Exact owner authority and presentation commit

C2 atomically admits established-session owner work against two independent
authorities: the command's one absolute request lifetime and the exact active
session lifetime. The implementation acquires their phase locks in stable
pointer order and executes only when the request remains Active and unexpired
and the exact session has not been Cancelled. Navigate dispatch and pending
publication, Delete Session stop/retirement, Get Current URL, unsupported
command rejection, and exact composition completion all use this dual
admission. Revoke is the only owner command admitted with the exact session
already Cancelled. Status is non-session state; New Session has no established
session to bind until it publishes its returned exact authority.

For an automation navigation awaiting presentation, the shell first inspects
and later atomically revalidates the UI-owned candidate tuple:

```text
(tab, NavigationId, EngineDocumentVersion,
 EnginePortFrameLeaseId, scene_revision)
```

`take_exact_presentation_scene` rejects any stale navigation, document, frame
lease, or scene revision without consuming the retained frame. The shell then
carries the same identity, request lifetime, and exact session authority in an
`AutomationPresentationPermit` through the real draw and native submission.
The permit has one explicit terminal state:

```text
Pending -> SubmissionInProgress -> NativeCommitted
Pending -> Cancelled | NotCommitted
```

At commit admission, the permit holds both lifetime phase locks and its outcome
lock. `begin_submission` transitions before any draw-transaction mutation.
Only a successful `submit_browser_frame` may transition to `NativeCommitted`,
and the same marker gates receipt exposure, composition increment, presented
identity/page/pointer/surface updates, and subsequent exact composition
completion. A cancellation or session revocation that wins before admission
never invokes the draw closure and exposes none of those effects. If native
submission wins, cancellation waits for that synchronous critical section,
then revokes the session and prevents later session mutation; the already
successful native swap is not and cannot honestly be rolled back.

Deterministic C2 regressions are:

- `cancellation_wins_at_the_pre_submission_barrier_without_shell_effects`:
  cancellation wins immediately before submission and proves zero receipt,
  composition increment, presented identity/page/pointer/surface, or later
  mutation; exact-session revocation exercises the same losing-admission path.
- `native_submission_wins_during_the_barrier_and_seals_late_mutation`:
  cancellation is released during the serialized native section, native commit
  wins once, shell state commits once, and the later cancellation prevents any
  further owner mutation without relabelling the submitted frame as rolled
  back.
- `exact_presentation_candidate_labels_are_revalidated_before_transfer`:
  a mismatched tuple neither consumes the frame nor changes retained-byte
  accounting; the exact tuple transfers once.
- `cancelled_navigation_authorities_block_late_composition_and_allow_recovery`:
  a cancelled request or exact session cannot complete a late composition, and
  a fresh generation recovers.

## Auth-before-body HTTP boundary

Authenticated automation uses a strict fixed-thread, one-request HTTP/1.1
implementation in the existing Rust WebDriver crate. The inherited Warp path
remains only for the legacy `start` entry point and is not the authenticated
browser boundary.

Admission order:

1. Read at most 16,384 header bytes without reading beyond `CRLF CRLF`.
2. Parse strict HTTP/1.1 origin-form headers.
3. Validate the bearer token.
4. Validate `Host` byte-for-byte against the actual bound socket.
5. If present, validate `Origin` byte-for-byte against the allowlist.
6. Match the route and validate method, framing, content type, and body size.
7. Drop and zeroize the authenticated raw header.
8. Only then allocate and read the exact bounded body.
9. Parse and `try_send` into the bounded dispatcher queue.

Duplicate Host, Authorization, Origin, Content-Type, Content-Length,
Transfer-Encoding, or Expect headers fail. Transfer-Encoding and Expect are
rejected. POST/PUT require Content-Length. Methods without bodies reject a
nonzero body. CORS-safelisted POST content types are rejected. Query and
fragment route forms are rejected. Responses carry `Connection: close`; no
keep-alive request loop exists.

Exact limits:

- request body: 65,536 bytes default; 1,048,576 hard maximum;
- dispatcher queue: 16 default; 64 hard maximum;
- one request deadline: 30 seconds default; 120 seconds hard maximum; zero
  rejected;
- browser owner deadline: at least 10 ms;
- header: 16,384 bytes;
- connection workers: `min(dispatch_queue + 1, 8)`, eight by default;
- accepted-socket queue per worker: zero-length rendezvous;
- busy accepted connection: immediate 503 when no worker accepts it;
- header/body/dispatch/client-disconnect polling: at most 20 ms;
- terminal response-write reserve: at most 20 ms and always carved from the
  same absolute deadline;
- nonblocking accept polling: 5 ms;
- owner queue: 16; owner drain: at most eight commands per wake.

New Session reports `timeouts.pageLoad` equal to that configured request
deadline in milliseconds: 30,000 by default and never above the 120,000 hard
maximum. Admission, queueing, browser work, composition correlation, and the
terminal response reserve all consume the same outer budget, so this is the
upper bound rather than a promise of a fresh navigation-only interval. Custom
WebDriver timeout capabilities remain rejected, Set Timeouts is unsupported,
and no 300-second page-load behavior is claimed. The advertised 30-second
script field is inert because script commands remain unsupported.

At most the fixed worker count can retain a 16-KiB authenticated-header buffer
or one bounded body each. The kernel TCP listen backlog remains
kernel-managed and is not claimed as an application queue bound. Once
accepted, a socket is either handed directly to an idle worker or answered
503; it is not queued in application memory.

`rejected_headers_return_before_any_declared_body_is_read` sends a declared
body but no body bytes for missing bearer, wrong Host, and wrong Origin and
proves immediate rejection with zero handler dispatch.
`connection_workers_and_header_body_admission_are_bounded` occupies all
workers with slow headers, proves an additional connection receives 503,
proves 408 header/body deadlines, and proves 431 at the header cap.
`bounded_dispatch_queue_rejects_excess_concurrency` proves queue admission is
bounded. These tests passed in the full matrix and in five consecutive
10-test adversarial repetitions.

## Atomic token-source admission and guaranteed erasure

The bearer token is exactly 64 lowercase hexadecimal bytes, representing 256
bits. The secret is never accepted in argv or environment. The CLI accepts a
token only from a Linux file descriptor number or an owner-only path.

Both flows use safe Rust APIs only:

- Path: one atomic `rustix::fs::open` with
  `RDONLY|CLOEXEC|NONBLOCK|NOFOLLOW`.
- FD: one safe reopen of `/proc/self/fd/<number>` with
  `RDONLY|CLOEXEC|NONBLOCK`; `NOFOLLOW` is inapplicable because that proc
  entry is the descriptor link being opened.
- Before any read, `rustix::fs::fstat` requires the opened object to be an
  exact regular file (including a regular memfd), owned by the current process
  user, with no group/other permission bits, and a bounded size of 64–66
  bytes.
- The reader consumes exactly the validated size, performs a one-byte EOF
  probe, then repeats `fstat` and requires identical device, inode, owner,
  mode, and size.
- The accepted contents are exactly 64 lowercase hex bytes, optionally
  followed by LF or CRLF.

FIFO/pipe token input is intentionally unsupported. FIFO, Unix socket, device,
symlink, wrong owner/mode/type, short, oversized, and malformed sources fail
closed. Opening a path cannot traverse a substituted final symlink; replacing
the directory entry after open cannot replace the already-open object.
Path-replacement regression runs 256 concurrent hardlink/symlink swaps and
remains bounded. FD failure tests leave the caller's original descriptor open.

The regular-file/memfd-only policy avoids unbounded pipe reads. Linux
`O_NONBLOCK` does not promise a latency bound for every pathological regular
filesystem implementation; this slice therefore makes no stronger claim than
atomic type/identity admission, an exact 66-byte read bound, and the tested
local regular-file/memfd policy.

Crate-wide `#![forbid(unsafe_code)]` remains unchanged in
`testing/webdriver/src/lib.rs`, `browser/wild_buzzard_shell/src/lib.rs`,
and `browser/wild_buzzard_shell/src/main.rs`. There is no unsafe block or
module in the WebDriver or shell implementation.

W9-A6N's only authorized path expansion was
`testing/webdriver/Cargo.toml`, solely to add exact direct
`zeroize = "=1.9.0"`. All token storage, token-read buffers, and the complete
raw authenticated header use heap-backed `SecretBytes<N>`, whose Drop invokes
the safe `Zeroize` implementation. Parsed headers borrow that storage and do
not copy Authorization. The header is dropped immediately after admission and
before body allocation. Drop behavior is directly observed in tests for
normal success, explicit erasure, parse failure, and panic/unwind. Error and
Debug formatting remain redacted; `BearerToken` has no Display implementation.

Dependency provenance:

- `zeroize 1.9.0`, RustCrypto utils, Apache-2.0 OR MIT,
  <https://github.com/RustCrypto/utils>, crate checksum
  `e13c156562582aa81c60cb29407084cdb54c4164760106ab78e6c5b0858cf64e`.
  It is pure Rust and requires Rust 1.85.
- `rustix 1.1.4`, Bytecode Alliance, Apache-2.0 WITH LLVM-exception OR
  Apache-2.0 OR MIT, <https://github.com/bytecodealliance/rustix>, crate
  checksum
  `b6fe4565b9518b83ef4f91bb47ce29620ca828bd32cb7e408f0062e9930ba190`.
  W9-A6N makes this existing locked transitive package an exact optional
  direct shell dependency for safe Linux descriptor/filesystem APIs.

The shell-local lock records both packages and records `zeroize` as a
WebDriver dependency.

W9-A6N-C1 regenerated and then minimized the shell-local lock offline. A
TOML-level comparison against `HEAD:browser/wild_buzzard_shell/Cargo.lock`
records 402 prior package records, 439 current records, 37 additions, zero
removals, and zero pre-existing version replacements. All 37 additions are
reachable from the `webdriver` feature tree. The only pre-existing records
whose dependency fields change are `chacha20` (version-disambiguated
`cpufeatures`), `futures-util` (activated `futures-sink`), `tokio` (activated
`signal-hook-registry`), `tracing` (activated `log`), and the shell package's
three optional direct dependencies. `font-types` remains exactly `0.12.2`;
the rejected unrelated `0.12.4` upgrade is absent. The default Cargo tree does
not activate the `webdriver` package. Exact default/feature trees, their delta,
and the machine-readable lock comparison are frozen under the C1 log tree.
C2 did not modify either manifest or the shell-local lock; its offline default,
WebDriver-feature, and inverse-zeroize Cargo trees reconfirm the current
closure under the C2 artifact directory.

## Supported behavior and nonclaims

Supported by this slice:

- one opt-in authenticated Classic session;
- Status before and after a session;
- real active-tab navigation;
- final redirected committed URL;
- exact native-composition correlation;
- exact Delete Session;
- stale tab/navigation/request/generation/session rejection;
- timeout, cancellation, response-loss, panic, and bounded shutdown paths.

Explicitly unsupported:

- Get Title;
- Get Page Source;
- Take Screenshot and element screenshot;
- WebDriver BiDi and `webSocketUrl`;
- element lookup/interaction;
- keyboard, pointer, wheel, and action input;
- scripts;
- cookies;
- prompts, permissions, and downloads;
- windows, tabs, frames, and context switching;
- print and every Classic command outside the five-command slice.

Known but unsupported commands return `unsupported operation` after exact
session validation and without synthetic state or browser side effects.
Unknown routes/methods retain WebDriver unknown-command/unknown-method errors.
A BiDi capability request fails New Session, and no BiDi capability or
`webSocketUrl` is advertised.

Screenshot remains honestly unsupported. This work performs no desktop
capture, diagnostic-pixel substitution, stale-frame result, or headless
rerender. It is not evidence of screenshot parity, WebDriver conformance,
Firefox ESR153 parity, JavaScript parity, YouTube parity, input/tab parity, VM
rendering qualification, or AppImage readiness.

## Final live-host truth

The final W9-A6N-C2 visible-display run is `live-wayland-c2-final` on the
physical x86_64 host's active `wayland-0` session. The container received the
host Wayland socket, `/dev/dri`, xkeyboard data, libxkbcommon, and
libwayland-egl. Renderer selection was not recorded, so this run does not
claim a particular GPU or prove hardware acceleration.

Exact test output:

```text
running 1 test
test real_browser_classic_flow_waits_for_exact_native_composition ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.60s
process exit: 0
```

The executable flow was Status; rejected BiDi capability; New Session without
`webSocketUrl`; Navigate through a real two-request loopback redirect; exact
final native composition; Get Current URL equal to the redirected final URL;
explicit unsupported Title/Screenshot/Source; Delete Session; Status.

Both browser stdout and stderr logs are zero bytes. The output directory
contains no `webdriver-token`; token cleanup completed. New Session also
asserted the corrected default `timeouts.pageLoad` value of 30,000 ms. The
test guard terminated the browser child only after the successful
Delete/Status flow, so this is not separate evidence of graceful visible
application exit. It is real executable/WebDriver/navigation/composition
evidence and is not a screenshot test. Unlike the Debian CPU-rendered YouTube
run supplied by the main orchestrator, this flow was rebuilt from the C2
sources and is C2 evidence; it uses the loopback redirect fixture, not YouTube.

## Verification environment and commands

All generated artifacts are under:

```text
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a6n-c2-webdriver-races
```

Every Podman command used only:

```text
/run/media/user/Data/Repositories/wildbuzzardbuilds/podman/podman-wildbuzzard
```

Wrapper SHA-256:
`f48ff6d31a8046db9139d2e9edcd02b4dfb64642fe6f3854a74e3475bf364b43`.
The wrapper supplies:

```sh
/usr/bin/podman \
  --root /run/media/user/Data/Repositories/wildbuzzardbuilds/podman/storage \
  --runroot /run/user/1000/wildbuzzard-podman "$@"
```

Command constants:

```sh
P=/run/media/user/Data/Repositories/wildbuzzardbuilds/podman/podman-wildbuzzard
R=/run/media/user/Data/Repositories/wildbuzzardrust
D=/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a6n-c2-webdriver-races
I=localhost/wildbuzzard-rust-tests:1.90-trixie-tools
T=x86_64-unknown-linux-gnu
```

For non-display commands, the repository was mounted read-only at
`/workspace`, the lane at `/data`, `CARGO_HOME=/data/cargo-home`,
`TMPDIR=/data/tmp`, and every `CARGO_TARGET_DIR` was below
`/data/targets/`. Shell/UI commands additionally used
`PYTHONPATH=/data/python/lib/python3.13/site-packages`. Cargo ran offline and
locked; every container used `--network none`.

Exact Cargo payloads:

```sh
cargo test --offline --manifest-path /data/webdriver-crate-harness/Cargo.toml \
  --locked --target $T

for attempt in 1 2 3 4 5; do
  cargo test --offline --manifest-path /data/webdriver-crate-harness/Cargo.toml \
    --locked --target $T --test authenticated_server
done

cargo test --offline --manifest-path browser/wild_buzzard_shell/Cargo.toml \
  --features webdriver --all-targets --locked --target $T

cargo test --offline --manifest-path browser/wild_buzzard_shell/Cargo.toml \
  --all-targets --locked --target $T

cargo test --offline --manifest-path browser/wild_buzzard_ui/Cargo.toml \
  --all-targets --locked --target $T

cargo clippy --offline --manifest-path /data/webdriver-server-clippy/Cargo.toml \
  --all-targets --no-deps --locked --target $T -- \
  -D warnings -W clippy::all -W clippy::pedantic

cargo clippy --offline --manifest-path browser/wild_buzzard_shell/Cargo.toml \
  --features webdriver --all-targets --no-deps --locked --target $T -- \
  -D warnings -W clippy::all -W clippy::pedantic

RUSTDOCFLAGS=-Dwarnings cargo doc --offline \
  --manifest-path /data/webdriver-server-clippy/Cargo.toml \
  --no-deps --document-private-items --locked --target $T

RUSTDOCFLAGS=-Dwarnings cargo doc --offline \
  --manifest-path browser/wild_buzzard_shell/Cargo.toml \
  --features webdriver --no-deps --document-private-items --locked --target $T

cargo build --offline --manifest-path browser/wild_buzzard_shell/Cargo.toml \
  --features webdriver --release --locked --target $T

cargo tree --offline --locked --target $T --edges normal,build --prefix none

cargo tree --offline --locked --target $T --features webdriver \
  --edges normal,build --prefix none
```

Focused C2 race payloads, using the same mounts/environment, were:

```sh
cargo test --offline --manifest-path /data/webdriver-crate-harness/Cargo.toml \
  --locked --target $T --lib \
  server::tests::pending_terminal_delivery_gates_queued_session_command_and_recovers \
  -- --exact

cargo test --offline --manifest-path /data/webdriver-crate-harness/Cargo.toml \
  --locked --target $T --test authenticated_server \
  post_completion_client_eof_revokes_exact_session_and_fresh_session_recovers \
  -- --exact

cargo test --offline --manifest-path browser/wild_buzzard_shell/Cargo.toml \
  --features webdriver --locked --target $T --lib \
  automation::tests::cancellation_wins_at_the_pre_submission_barrier_without_shell_effects \
  -- --exact

cargo test --offline --manifest-path browser/wild_buzzard_shell/Cargo.toml \
  --features webdriver --locked --target $T --lib \
  automation::tests::native_submission_wins_during_the_barrier_and_seals_late_mutation \
  -- --exact
```

The authenticated-server integration target and the pair of presentation-race
tests were each repeated five times with the same exact filters.

The Firefox-absent shell test and release build added the read-only bind:

```sh
-v $D/firefox-absent:/workspace/firefox:ro
```

Formatting and repository checks:

```sh
rustfmt --check --edition 2024 \
  testing/webdriver/src/server.rs \
  testing/webdriver/tests/authenticated_server.rs \
  browser/wild_buzzard_shell/src/automation.rs \
  browser/wild_buzzard_shell/src/lib.rs \
  browser/wild_buzzard_shell/src/main.rs \
  browser/wild_buzzard_shell/tests/webdriver_classic.rs \
  browser/wild_buzzard_ui/src/session.rs \
  browser/wild_buzzard_ui/tests/automation_session.rs

git diff --check
```

The final visible command was:

```sh
$P run --rm --network none --device /dev/dri --group-add keep-groups \
  -e CARGO_HOME=/data/cargo-home \
  -e CARGO_TARGET_DIR=/data/targets/shell-tests \
  -e TMPDIR=/data/tmp \
  -e PYTHONPATH=/data/python/lib/python3.13/site-packages \
  -e WILDBUZZARD_REAL_DISPLAY_TEST=1 \
  -e WILDBUZZARD_TEST_OUTPUT_DIR=/data/live-wayland-c2-final \
  -e WAYLAND_DISPLAY=wayland-0 -e XDG_RUNTIME_DIR=/run/user/1000 \
  -e WINIT_UNIX_BACKEND=wayland \
  -e XKB_CONFIG_ROOT=/usr/share/xkeyboard-config-2 \
  -v $R:/workspace:ro -v $D:/data:rw \
  -v /run/user/1000:/run/user/1000:rw \
  -v /usr/share/xkeyboard-config-2:/usr/share/xkeyboard-config-2:ro \
  -v /usr/lib/x86_64-linux-gnu/libxkbcommon.so.0.13.1:/usr/lib/x86_64-linux-gnu/libxkbcommon.so.0:ro \
  -v /usr/lib/x86_64-linux-gnu/libwayland-egl.so.1.24.0:/usr/lib/x86_64-linux-gnu/libwayland-egl.so.1:ro \
  -w /workspace $I sh -c 'export PATH=/usr/local/cargo/bin:$PATH; \
  cargo test --offline \
    --manifest-path browser/wild_buzzard_shell/Cargo.toml --features webdriver \
    --locked --target $T --test webdriver_classic \
    real_browser_classic_flow_waits_for_exact_native_composition \
    -- --ignored --exact --nocapture'
```

## Verification results

- C2 WebDriver crate harness: 220 unit tests and 12
  authenticated-server integration tests passed.
- Five complete authenticated-server adversarial repetitions: 60/60 passed.
- The focused completed-New-Session terminal-delivery gate and exact client-EOF
  integration race each passed independently. The exact pre-submission and
  during-submission races each passed, and five paired repetitions passed
  10/10.
- Shell with WebDriver feature: 46 library tests, 4 token/config executable
  tests, and 1 non-display Classic integration test passed; the live test
  remained ignored in the ordinary run.
- Shell default features: 34 tests passed and automation integration binaries
  ran zero tests, preserving default-off behavior.
- UI regression matrix: 38 unit + 2 automation + 38 browser-session + 4
  navigation-port tests passed (82/82).
- Focused owned WebDriver server/tests Clippy: exit 0 with
  `-D warnings -W clippy::all -W clippy::pedantic`. One non-denied warning
  came from the dependency build of untouched imported
  `testing/webdriver/src/lib.rs` (`unused #[macro_use]`), not the focused
  target.
- Feature-enabled shell all-target no-deps Clippy with the same strict flags:
  exit 0.
- Focused WebDriver and feature-enabled shell warning-denied no-deps rustdoc:
  exit 0.
- Exact-path rustfmt: exit 0 with no output.
- Final `git diff --check`: exit 0.
- Shell-local lock proof: 37 added package records, 0 removed records, 0
  pre-existing version replacements, and all additions reachable from the
  WebDriver feature tree; `font-types` remains 0.12.2.
- Firefox-absent release build: exit 0 in 10.60 s after the final C2 source
  change. One warning remains in
  untouched WebRender (`RenderTaskGraph::frame_id` unread).
- Physical Wayland exact live flow rebuilt from C2 source and exited 0: 1
  passed, 0 failed, 1 filtered; the flow itself finished in 0.60 s.

The imported whole WebDriver crate still has strict Clippy and rustdoc debt in
untouched files outside this task's writable scope. The prior full strict run
recorded 119 library and 125 library-test Clippy errors across imported
`actions.rs`, `capabilities.rs`, `command.rs`, `httpapi.rs`,
`response.rs`, and `test.rs`; whole-crate warning-denied rustdoc recorded
five inherited broken links in `error.rs`. W9-A6N-C2's focused owned paths and
the full shell target are green; this handoff does not relabel inherited
whole-crate failures as passes.

## Frozen artifacts and logs

Release executable:

```text
/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a6n-c2-webdriver-races/targets/release-firefox-absent/x86_64-unknown-linux-gnu/release/wild-buzzard
28,809,360 bytes
SHA-256 4d01adfb9862f32de352a9006f54773d3ef6fbacbb5239f51f270d232376fbff
```

Acceptance log hashes, relative to the W9-A6N-C2 artifact directory:

```text
84b840f62037fd58ae6cd622080b75856f01ca9d18580c6a593d999eab081c71  logs/focused-webdriver-terminal-gate.log
1c686e6b1768f28bc0c2e9affaef021454adc2736e6c7ccc109e96e1c94ce600  logs/focused-webdriver-client-eof.log
c876cf25f8ecd67e0e98e967d5953be4d6cd420768087bf5950e2bd8a4871293  logs/webdriver-full.log
e5253dea9b23ff7d90682aee73ef1f78dc814d38f43e8fe4a5c126c7cbbc1e05  logs/webdriver-authenticated-repeat-5.log
c164552c79ebf64d2a17f06ad911094df49a85efe1e455200b341f0b395ec02a  logs/focused-shell-pre-submission.log
16723b2bb630662b52aa0cb163cb19aca51e4deece9493c218ceabc895127e43  logs/focused-shell-during-submission.log
5177a02d413ed1248afaa9c0e6aebc9c8dc153b670fe828508fecf7358fb73f1  logs/shell-presentation-races-repeat-5.log
d54a45a9fd437231e7b99094f34244d0b3409c1cfbb16a1ff423b7f474742a5e  logs/shell-webdriver-full-final.log
6d08f931254cf9cee4f6b1762aeec9b8245a83997e090a651d5e66c9582fd837  logs/shell-default-full.log
a0ef544497217608581827d8acab8d9e73e0b0e23abcb89a1ff9e9b1fba00e93  logs/ui-full.log
7036b256489a96b8e5c6d47faec15ba26d6c75b18760a90a624616765a8141a5  logs/webdriver-server-focused-clippy.log
fd46542843d735b7d50754bdc28aca410342baa7080c57b099ecfe9ed74151a4  logs/shell-clippy-final.log
a6cef2e12e57616b9de8b9cc9f3aed25b6ee8c0c27bcf79671f793a26c8cfa4d  logs/webdriver-server-focused-rustdoc.log
6f4e43a9b88e30b0343fdf40231d8b4fae88a946af262a0aa8543b844d5f0c81  logs/shell-webdriver-rustdoc-final.log
ea362763ea9b90547b7e4aeb7f3dca299b1d61ca1630f498a396c2011c50fa8e  logs/shell-release-firefox-absent-final.log
3af19428dba7f9c93c83aab962c0a889dfc6a06fb6f856b063dd2ae66c98d506  logs/live-wayland-c2-final-harness.log
6c845be8eadd7bb3b96b3fdaa61dec89fd7a35d0889a7f626c22d77266667453  logs/cargo-tree-default.log
bf21ab662baa88afabce99bf1691144bccd7516f621087c90f5dc3447f5308b8  logs/cargo-tree-webdriver.log
953910ae97f0654bc4ef760eb93cb3fbb6c76271ffb807222a6180835174bfa1  logs/cargo-tree-zeroize-inverse.log
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  logs/rustfmt-check-final.log
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  logs/git-diff-check-final.log
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  live-wayland-c2-final/wild-buzzard.stdout.log
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  live-wayland-c2-final/wild-buzzard.stderr.log
```

Exploratory and pre-fix logs also remain under `logs/`; the files above are
the final acceptance evidence.

## Frozen source path/hash inventory

W9-A6N-C2 changed exactly these six implementation/test paths plus this
handoff:

```text
329663c7543dd0ae34bdd669b23607c90e5af3877676473762c2528e796845d8  testing/webdriver/src/server.rs
faa39f1b12c20b3c6f21c7bd64cf35fd3ea92851718e9cae07f765614f984cf8  testing/webdriver/tests/authenticated_server.rs
e49856a91340c1ed50545b89e4868a41a4ed8e65ee7342379cc3ca93bca3fb5d  browser/wild_buzzard_shell/src/automation.rs
53f979d4551bae6e32242c105f1ce3fa29fc072341239e3912e4a54492c42ad2  browser/wild_buzzard_shell/src/lib.rs
659951a0c113bfa984c2d7f8ff61631ae57495c980e525e6b8fb52aaa9046aca  browser/wild_buzzard_ui/src/session.rs
86d6719ab6c00de73c0e5e45bdc479f35fb16e279700a55e87b7b085d4f98f00  browser/wild_buzzard_ui/tests/automation_session.rs
```

The handoff excludes itself from this self-referential inventory; its
post-write hash belongs in the final agent report.

The current prerequisite/config/registry paths, unchanged by C2, freeze as:

```text
d415c8261317e2ee571604fc0d6f98788292277b9865703f7ca26cfc30f297a2  testing/webdriver/Cargo.toml
c1782012bff6f59f4d8581d8bf027cc643eda9ab07d5d56cdbf978fa4c10d65f  browser/wild_buzzard_shell/Cargo.toml
9af450ae809f77c4cbdcec9cc885bc0c6994206fff6974465a4680ada106f5d8  browser/wild_buzzard_shell/Cargo.lock
74e4f06390c3e29aa1c2d956ef3cbf87665f26e4295d57433ad1f59261003f9e  browser/wild_buzzard_shell/src/main.rs
f2d0bc5f7de544e0d822c5e6871bdcace19e8dab62143909d2e8a4d6647588de  browser/wild_buzzard_shell/tests/webdriver_classic.rs
3cec01011cda23cba7412ab01968b9c80b390423b0be99c24369b84cb984d923  docs/wire-registry.toml
```

Unsafe-policy invariant, also unchanged:

```text
b80989461de478fc803e2930dd62d7ba737feaee6c8733333efff8270722b809  testing/webdriver/src/lib.rs
```

No root manifest/lock, Firefox, graphics, engine, JS, general-navigation,
README, parity/status, AGENTS, or other undeclared path was edited by
W9-A6N-C2. Concurrent shared-worktree changes in those areas belong to other
owners. C2 used the two explicitly authorized UI/session paths listed above to
carry and test the exact presentation candidate identity.

## Remaining work

The next automation slices must securely integrate a compositor-owned capture
with the exact live receipt and prove that no desktop/headless/stale pixels can
be substituted; add real title and source from committed document state; then
add input, tabs/windows, elements, scripts, prompts, and the broader Classic
conformance matrix. None of that is exposed by this slice. BiDi needs a
separate authenticated bounded design and remains unadvertised until
implemented. Imported whole-WebDriver Clippy/rustdoc debt also remains outside
this slice.

No file was staged, committed, or pushed.

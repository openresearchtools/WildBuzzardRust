# W6-A6G Rust browser shell handoff

## Outcome

W6-A6G adds the first production-oriented, same-process Wild Buzzard browser vertical slice. The
new independently locked `browser/wild_buzzard_shell` executable connects the bounded browser
session, real static-page engine, canonical Rust text shaper, Linux Wayland/X11 event shell, native
EGL surface, and WebRender compositor. A successful page reaches the native window as its original
renderer-neutral `CompiledScene` and exact shaped-text inventory. It is not rasterized into a CPU
image, copied through a screenshot path, or relabelled after transfer.

This is a real executable integration checkpoint, not a browser-product or Firefox-parity claim.
Its visible chrome is a deliberately bounded first-party Rust surface for exercising tab,
address, status, page, resize, input, and shutdown ownership. It does not reproduce Firefox ESR's
complete design, controls, menus, services, accessibility, or interaction behavior. The next
product-UI program is recorded below and continues immediately after this checkpoint.

Wild Buzzard branding and application identity remain independent. No Firefox artwork, trademark,
application identifier, profile name, or branded default was copied.

## Source scope

New independently locked executable workspace:

- `browser/wild_buzzard_shell/Cargo.toml`
- `browser/wild_buzzard_shell/Cargo.lock`
- `browser/wild_buzzard_shell/src/lib.rs`
- `browser/wild_buzzard_shell/src/main.rs`

Extended integration owners:

- `browser/wild_buzzard_engine/src/{error,lib,navigation,pipeline}.rs`
- `browser/wild_buzzard_engine/tests/{navigation_facade,static_pipeline}.rs`
- `browser/wild_buzzard_ui/src/{engine,lib,session}.rs`
- `browser/wild_buzzard_ui/tests/{browser_session,navigation_engine_port}.rs`
- `widget/rust/wild_buzzard_linux/src/{config,event,lib,shell}.rs`

The root workspace manifest and lock were not edited by this lane. The new shell is independently
locked so it can be built and tested without changing root integration policy. All build artifacts
remain outside the repository under `/home/user/Documents/wildbuzzardbuilds/`; the dedicated final
acceptance targets are below `wildbuzzardbuilds/w6-a6g/`.

## End-to-end ownership contract

The integrated successful path is:

```text
LinuxWindowShell creates one exact native window/EGL owner
  -> BrowserHandler starts one presentation-only NavigationEnginePort
  -> BrowserSession admits an exact tab/context/navigation generation
  -> StaticPageEngine fetches, parses, styles, lays out, and shapes
  -> one PresentationScene owns CompiledScene + canonical ShapedSceneText[]
  -> one engine frame lease carries exact navigation/document/scene labels
  -> BrowserSession revalidates every label before consuming the lease
  -> final BrowserPageScene is constructed directly, with no parts callback
  -> LinuxWindowControl submits page update + independently revised chrome
  -> WebRender draws both into framebuffer zero on the exact EGL surface
  -> EGL accepts the exact monotonically sequenced swap
  -> receipt is retained for exact hit-test and page-state correlation
```

Only the event-loop owner thread can access the session, text shaper, compositor, or native window
control. The helper thread owns no browser state or payload queue; it can only request a coalesced
wake through `LinuxWakeHandle`. Polling is enabled while engine work, rerender work, or an
unfinished configured smoke is pending and disabled otherwise.

### Engine scene mode

`StaticPageEngine::new_for_presentation` is an explicit alternative to its retained headless/test
mode. Presentation operations:

- construct no headless EGL pbuffer renderer;
- retain the exact compiled display list and canonical shaped-text allocations together;
- assign a nonzero monotone `PresentationSceneRevision` within the owning static-page engine
  pipeline (it is not a process-global identity);
- carry the exact `DocumentVersion`, `PipelineKey`, item count, shaped-run count, and display-list
  bytes; and
- conservatively charge the scene, strings, glyphs, clusters, variation data, allocation/Arc
  overhead, and each unique selected font blob before worker/session admission.

Headless APIs fail on a presentation-only owner and presentation APIs fail on a headless owner.
`FrameOutputMetadata` and `EngineFrameDescriptor` preserve that distinction. A scene exposes no
RGBA8 byte slice, and an RGBA8 lease cannot be consumed as a scene. Presentation shutdown reports
no fabricated headless-renderer teardown.

The navigation executor retains the same transactional old/new document behavior used by the
headless path. Initial load, document mutation, and exact-version rerender produce the configured
output kind without introducing a second live heap or renderer.

### Atomic UI-to-graphics transfer

The UI boundary retains capability-neutral `EnginePresentationDescriptor` metadata for inspection,
but its scene remains behind the one-shot frame lease. `BrowserSession::take_presentation_scene`
preflights all of the following before removing the candidate or changing retained-frame
accounting:

- exact tab;
- exact retained live `NavigationId`;
- frame lease's navigation;
- exact live and frame `EngineDocumentVersion`;
- presentation descriptor document version; and
- exact engine-assigned scene revision.

A stale revision/document label, non-presentation output, or cross-tab navigation is rejected while
the original candidate and its accounting remain unchanged. The successful continuation consumes
`EnginePresentationLease` directly into the final `BrowserPageScene`. There is no public
`into_parts`, callback, or `map_scene` seam at the UI boundary that could return a detached scene
and silently discard its labels. The compositor revision is derived from the engine scene rather
than supplied independently by the shell.

### Linux browser-compositor mode

`LinuxPresentationMode` now selects either the retained direct diagnostic path or the browser
compositor. Browser mode owns `WebRenderPresentedWindow` behind the existing one-window lifecycle
and exposes only typed operations through the callback-scoped `LinuxWindowControl`:

- exact surface snapshot;
- atomic browser frame submission;
- receipt-bound browser hit testing;
- IME enable/cursor control; and
- validated native inner-size requests used by the live smoke.

Direct solid-color submission fails explicitly in browser mode, and browser operations fail
explicitly in direct mode. Existing direct-presentation behavior remains a separate diagnostic
contract.

Browser startup, native faults, renderer faults (including terminal hit-test faults), teardown, and
fail-closed retention map into typed Linux stages. A normal browser shutdown records ordered
WebRender backend, renderer, and native presenter evidence. It does not turn unknown native
destruction into affirmative evidence.

Frame rejection is retryable only when the presenter explicitly classifies it as nonterminal,
which is limited to a preaccept validation/composition rejection. A terminal or unclassified
control failure returns an executable error immediately; it is never retried, rerendered, or
relabeled. The top-level runner also rejects any terminal `BrowserPresentationFailed` native stop
reason defensively, so a lost compositor cannot produce an `Ok(BrowserRunReport)`.

The shell admits at most eight consecutive retry-safe preaccept rejections; the ninth fails the
executable. Only an exact successful composition resets that budget. A rejected submission which
consumed an `Install` waits for an exact retained-document rerender rather than replaying the
one-shot scene. A rejected `Retain` or `ClearToBlank` may request one bounded immediate redraw.
Deferred or rejected draws cannot advance smoke stages; every later successful exact receipt is
routed through the same central composition/resize stage hook.

## Browser shell behavior

The executable creates the native window first, then starts the engine using the exact initial
physical content extent below browser chrome. It draws first-party tab labels, close targets,
address text/selection/focus, optional loading/document-operation failure status, and the current
page scene through one WebRender composition.

Supported product interactions in this checkpoint are those already modeled by
`BrowserSession`:

- open, activate, and close tabs;
- editable UTF-8 address text and committed numeric-loopback HTTP navigation;
- back, forward, reload, stop, address focus, tab cycling, and numbered-tab keyboard commands;
- exact-primary tab/close/address/page pointer hit testing; and
- bounded IME preedit/commit and keyboard routing through the Linux event contract.

An input event is offered to the browser session exactly once before any pointer-down chrome hit
action. This advances the session's strict native sequence high-water mark even when a typed
tab/close/address/page action returns early; the fallthrough path reuses the one routing outcome
instead of submitting the event again. Physical key events never synthesize text. Committed text
comes only from `InputEvent::Text`. Focused regressions observe one text event inserting exactly one
scalar and prove an early-action pointer sequence is accounted before the action while replay fails
closed.

### Page replacement and tab switching

The shell keeps the old page visible while a same-tab replacement is requested, started, or
committed. It atomically installs only a successfully published newer scene. Switching to a tab
whose earlier one-shot scene was consumed clears the old tab's page before requesting an exact
rerender of that tab's retained document; it never displays one tab's scene under another tab's
chrome.

Engine scene revisions are monotone within the one owning static-page engine pipeline used by this
shell, but a candidate produced earlier for an inactive tab can arrive after a newer active-tab
scene was installed. The shell rejects a candidate whose revision does not exceed the last
installed page revision, consumes that obsolete one-shot lease, and requests an exact
retained-document rerender. This comparison is valid only inside that engine-owner domain; the
revision is not process-global. The shell never rolls compositor page state backward.

Every admitted presentation rerender is tracked by exact tab, navigation, document, and
`DocumentOperationId`. Completion is reconciled across active and inactive tabs. Rendered success,
explicit rejection, retained-frame budget suppression, and semantically stale no-frame completion
are terminal observations. A terminal failure or no-frame completion suppresses repeated requests
for only the same labels, preventing a redraw loop; a material surface transition, label change, or
successful install reopens admission. Tab/window/session close retires both pending and suppressed
authority so repeated open/fail/close cycles remain bounded.

### Resource bounds

The executable uses a shell-specific `SessionLimits` policy rather than the broader library
default:

- one window;
- 64 tabs total and per window, exactly matching `MAX_BROWSER_CHROME_TABS`;
- 64 closing contexts;
- 50 URL-only history entries per tab;
- 64 MiB aggregate history strings;
- 256 MiB aggregate retained frame/scene charge;
- the engine's 16 KiB canonical address limit; and
- at most 256 engine events per pump.

Canonical addresses and history are not truncated. Their visual projection is independently
bounded on UTF-8 boundaries: 32 bytes per tab label, 1,792 bytes for the address display, and 256
bytes for status. Across 64 tabs this is at most 4,096 UTF-8 bytes and 66 shaped texts, within the
compositor's fixed byte, run, glyph, and text-count ceilings. Selection endpoints are clamped only
for the shortened visual projection and remain valid UTF-8 boundaries; canonical editor selection
is unchanged.

Graphics navigation identities are process-monotone and never reused. Before every lookup or
insert, the shell derives the exact set of live session navigation IDs, rejects a non-live request,
and prunes terminal replacements. Old receipts and presented pixels copy the opaque identity by
value and need no map entry. The registry retains a defense-in-depth hard process cap of 4,096.
A regression performs 256 successful same-tab replacement navigations and requires the map to
remain exactly equal to the live-navigation set (one entry in that test).

### Resize behavior

This checkpoint does not pretend the fixed-viewport static engine can reflow. A material native
resize or scale transition marks the installed page stale. Exact duplicate geometry callbacks are
suppressed before presenter mutation or normalized delivery. Each callback admits at most one
native inner-size request; a second is rejected before a second native call. A `winit`
`Some(applied)` response is recorded callback-scoped and, after the handler returns, reconciled
through the same presenter/lifecycle/descriptor/normalized-event resize path as a native callback.
`None` fabricates no resize, while a later exact duplicate callback after a synchronous result is
suppressed. Explicit suspension and zero-sized surface suspension are tracked independently and
overlap idempotently; queued redraws cannot query or submit through the compositor until a valid
recovery transition. If the new chrome content extent differs from the engine's startup viewport
(including a tiny nonzero all-chrome extent), the shell atomically clears the page, continues
rendering chrome, and shows an explicit status limitation. When the exact startup extent returns,
it requests a fresh scene from the retained document and reinstalls only that new revision.

This is safe resize ownership evidence, not responsive layout support. Dynamic viewport updates,
style/layout reflow, device-pixel-ratio propagation, and scroll preservation remain next work.

### Deterministic shutdown

An admitted stop closes native wake admission. Inside `LinuxWindowShell::run`, winit's exiting
callback first shuts down the WebRender backend, renderer, and native presenter wrappers, then
releases the exact surface and delivers `Destroyed`. The handler's `Destroyed` path shuts down the
browser session/engine once and then the chrome text owner; `Stopped` follows with the native
report. Only after `LinuxWindowShell::run` returns does `run_browser` clear the helper's running
flag and join that payload-free thread. The returned `BrowserRunReport` retains native, engine, and
text shutdown evidence plus composition count and last exact receipt.

## Same-binary live smoke

`wild-buzzard --smoke --backend wayland|x11` is opt-in and requires
`WILDBUZZARD_REAL_DISPLAY_TEST=1`. The same executable starts a bounded numeric-loopback HTTP server
and exercises, in order:

1. load and install a nonempty shaped-text blue page in tab A;
2. open tab B and install a distinct orange page;
3. switch back to A without showing B under A, then rerender and reinstall A;
4. request a smaller native extent and present chrome with an explicitly blank page;
5. restore the original extent and rerender/reinstall A;
6. close B, continue pumping until the exact engine `ContextClosed` acknowledgement retires its
   tombstone, and submit a successful exact one-tab chrome composition;
7. leave the final result visible for 3 seconds; and
8. exit through normal ordered teardown.

The program has its own 20-second hard deadline in addition to the outer test timeout and requires
at least six successful exact compositions. An unfinished configured smoke keeps wake polling
admitted before and during its final hold, so an otherwise quiescent intermediate stage cannot
strand the watchdog. The receipt proves WebRender submission and EGL swap acceptance, not that the
desktop compositor displayed the buffer.

## Firefox ESR153 reference evidence

The reference remained the read-only detached checkout at
`c19b7e89270787889495688244ec6ee8e79288a1`. It was never edited and is not a build, test, or runtime
input.

Focused implementation inspection covered:

- `browser/base/content/browser.xhtml` for the top-level browser window, shared command sets,
  toolbox/browser-box composition, modal overlay, fullscreen hooks, and accessibility announcement;
- `browser/base/content/navigator-toolbox.inc.xhtml` for tabs, new/all-tabs controls, back/forward,
  combined reload/stop, URL bar, identity/permission indicators, overflow, customization, and
  titlebar structure;
- `browser/components/tabbrowser/content/tabbrowser.js` for tab creation, activation, load,
  removal, owner/successor selection, and current-browser routing; and
- the W5-A6F reference paths for URL-bar editing, history, key bindings, and close behavior recorded
  in `docs/handoffs/W5-A6F-browser-session.md`.

Focused tests inspected in this wave:

- `browser/components/tabbrowser/test/browser/tabs/browser_tabswitch_select.js` for URL-bar
  selection/focus restoration across tab switching and new-tab focus;
- `browser/components/tabbrowser/test/browser/tabs/browser_tabs_owner.js` for opener ownership and
  close selection; and
- the surrounding close, select, Ctrl-Tab, and beforeunload tests listed under
  `browser/components/tabbrowser/test/browser/tabs/`.

Recent path history was inspected for `browser.xhtml`, navigator toolbox, and tabbrowser. These
references inform observable ownership and the next parity plan only. No XUL/JavaScript source or
Mozilla artwork was translated into the Rust slice.

## Verification evidence

All Cargo resolution and build commands use offline mode, the locked local workspaces, explicit
`x86_64-unknown-linux-gnu`, and external targets under
`/home/user/Documents/wildbuzzardbuilds/`. No generated output belongs in the repository.

Frozen-tree focused matrix on 2026-08-09:

| Gate | Exact result |
| --- | --- |
| `rustfmt --edition 2024 --check` over every owned engine/UI/shell/widget Rust source and test file | PASS; no diff |
| `git diff --check` over the four owned source trees plus this handoff | PASS |
| `cargo test --manifest-path browser/wild_buzzard_engine/Cargo.toml --locked --offline --target x86_64-unknown-linux-gnu --all-targets` | PASS; 56 tests |
| engine `cargo clippy ... --all-targets -- -D warnings` | PASS |
| `cargo test --manifest-path browser/wild_buzzard_ui/Cargo.toml --locked --offline --target x86_64-unknown-linux-gnu --all-targets` | PASS; 56 tests (23 library, 30 session integration, 3 real-port integration) |
| UI `cargo clippy ... --all-targets -- -D warnings` | PASS |
| `cargo test --manifest-path widget/rust/wild_buzzard_linux/Cargo.toml --locked --offline --target x86_64-unknown-linux-gnu --all-targets` | PASS; 32 unit tests and 0 opt-in display tests without the live-test environment |
| widget `cargo clippy ... --all-targets -- -D warnings` | PASS |
| `cargo test --manifest-path browser/wild_buzzard_shell/Cargo.toml --locked --offline --target x86_64-unknown-linux-gnu --all-targets` | PASS; 25 library tests and 0 binary tests |
| shell `cargo clippy ... --all-targets -- -D warnings -D clippy::pedantic -D clippy::nursery` | PASS |

Independent frozen-tree acceptance used rustc/cargo 1.96.0 and the identical release artifact at
`/home/user/Documents/wildbuzzardbuilds/w6-a6g/shell/x86_64-unknown-linux-gnu/release/wild-buzzard`.
The binary is 22,384,592 bytes with SHA-256
`0e83e75a46274b438671a118aa563ac21576206a2dd02f4aaa424780dfae5986`.
Warning-denied rustdoc doctests and `cargo doc --no-deps` passed against that frozen source.

Both live runs used `XDG_SESSION_TYPE=wayland`, `WAYLAND_DISPLAY=wayland-0`, `DISPLAY=:0`, and the
exact command shape
`WILDBUZZARD_REAL_DISPLAY_TEST=1 timeout --signal=TERM --kill-after=5s 30s <binary> --smoke --backend wayland|x11`.

| Live gate | Observed exact result |
| --- | --- |
| Wayland same-binary smoke | PASS; exit 0; success marker at 12 compositions; final `Requested` shutdown at 58 compositions; 1600x1000 at scale 1.25; surface revision 3; page scene revision 4; page epoch 10; chrome epoch/backend publish ID 58 |
| X11 same-binary smoke | PASS; exit 0; success marker at 13 compositions; final `Requested` shutdown at 84 compositions; 2560x1600 at scale 2; surface revision 3; page scene revision 4; page epoch 11; chrome epoch/backend publish ID 84 |

The persisted Wayland log is
`/home/user/Documents/wildbuzzardbuilds/w6-a6g/wild-buzzard-wayland.log` (SHA-256
`ab9111b56311d4362ba9f555d2dc3b4f80243db1277eb0cfd042a49c6d814f6e`); the X11 log is
`/home/user/Documents/wildbuzzardbuilds/w6-a6g/wild-buzzard-x11.log` (SHA-256
`4f878c074652caec66437fb1ef1d6dcfb68d647d9d0000e05bd56dedbad1e6df`).
All attempted desktop screenshots were rejected because they were black or showed an unrelated
foreground application. They are not evidence that the desktop compositor displayed Wild
Buzzard's submitted buffer, and this handoff makes no such visibility claim.

## Explicit non-claims

W6-A6G does not claim Firefox UI, web-platform, security, privacy, performance, accessibility,
packaging, or general-browsing parity. In particular, this checkpoint does not provide:

- Firefox's primary toolbar design, complete controls, menus/panels, overflow behavior, vertical
  tabs, tab groups, pinned tabs, drag/drop, multiselect, or customization;
- a complete URL/search bar, search suggestions, identity/security UI, permission indicators, or
  site information;
- settings, profiles, policies, bookmarks, Places/library, full visit history, downloads,
  passwords, permissions/prompts, session restore, private windows, extension UI, printing, or
  developer tools;
- responsive viewport reflow, scrolling, page input dispatch, forms, selection, script/DOM event
  execution, right/middle pointer actions, context menus, middle-click tab behavior, animation,
  media, Canvas/WebGL/WebGPU, or general Internet navigation;
- multiple native browser windows, site-isolated content/GPU/network processes, sandboxing, crash
  recovery, device-loss recovery, frame pacing/damage, compositor acknowledgement, or AppImage
  dependency closure; or
- accessible chrome/page trees, Linux AT-SPI output, complete keyboard traversal, high contrast,
  zoom, localization, UI automation, or Firefox-compatible remote protocols.

The current numeric-loopback static-page restriction is a pipeline gate, not a browser-networking
product decision. The one-window same-process architecture is an integration proof, not the final
process model.

## Continuing Firefox-like Rust product-UI program

The durable Agent 6 lane continues immediately after this checkpoint. Each work item must use the
pinned ESR implementation and relevant browser tests as behavioral reference, implement new
first-party runtime/product code in Rust, preserve Wild Buzzard identity, and add recorded
conformance/regression evidence before claiming completion.

| Product area | ESR-shaped observable target | Current W6 state | Next acceptance evidence |
| --- | --- | --- | --- |
| Primary chrome and toolbars | Firefox-like tabs/titlebar, back/forward, stop/reload, URL bar, new/all-tabs, application/overflow menus, fullscreen/popup modes, focus/keyboard traversal | Simplified tabs/address/status only | Rust scene/state model, exact action routing, screenshots/reftests, keyboard and pointer browser tests on Wayland/X11 |
| Tab product behavior | Pinned/grouped/multiselected tabs, overflow/scroll, drag/drop, mute, opener/successor close policy, beforeunload, tab context menus | Basic bounded open/activate/adjacent-close | ESR tabbrowser behavior matrix, ordering/model tests, visual and live native interaction tests |
| URL/search and identity | URL parsing/display, autocomplete, search modes/providers, security/site identity, tracking and permission indicators, edit context actions | Bounded literal address editor and loopback URL submission | Provider-neutral Rust model, privacy policy, ESR URL-bar test adaptations, accessibility and keyboard evidence |
| Settings and policy | Firefox-like preferences categories, search, defaults, per-site controls, policy/managed state | Absent | Typed preference schemas/storage, UI navigation tests, restart persistence, policy provenance and safe defaults |
| Bookmarks and Library | Bookmark star/editing, folders/tags, sidebar/toolbar, Library, import/export | Absent | Rust Places-style ownership, transactional storage, migration/import tests, keyboard/a11y and large-data bounds |
| History | Visit recording, typed/linked transitions, searchable Library/sidebar, clear ranges, recently closed tabs/windows | URL-only per-tab session history | Privacy-aware history store, exact clearing semantics, restore/reopen tests, bounded long-profile evidence |
| Downloads | Downloads button/panel/library, progress, pause/cancel/retry/open, dangerous-file handling | Absent | Network/file capability boundary, atomic files, quarantine/safety policy, failure/restart UI tests |
| Permissions and prompts | Doorhangers/modal prompts, per-origin decisions, transient/persistent grants, blocked indicators, auth/cert/download dialogs | Absent | Origin-bound permission service, prompt arbitration, spoof resistance, denial/focus/automation tests |
| Sessions, profiles, and private windows | Startup restore, closed-tab/window recovery, crash recovery, profiles, private browsing separation | One ephemeral window; no persistence | Versioned session/profile storage, private-data isolation, crash/restart corpus, multiwindow native tests |
| Customization and themes | Movable/removable toolbar widgets, overflow, density, theme/color modes, reset/safe mode | Absent | Persisted constrained layout model, conflict/migration tests, visual reftests and reset recovery |
| Accessibility and automation | Complete accessible chrome/page trees, roles/names/states/actions, focus announcements, AT-SPI, browser automation | No accessible tree | Rust a11y ownership, Linux AT-SPI adapter, keyboard-only matrix, screen-reader inspection, WebDriver/BiDi-compatible automation plan |
| Localization and Linux integration | Fluent-backed strings, locale/RTL, mnemonic/access keys, zoom/scaling, Wayland/X11 clipboard/drag/drop/notifications | English literals and basic input/window path | First-party localization resources, RTL/reflow tests, scale/theme/backend matrices, packaged desktop integration |

No row is complete merely because a control is painted. Completion requires its state ownership,
keyboard/pointer/accessibility behavior, persistence/security boundaries where applicable, ESR
reference tests, and integrated native evidence.

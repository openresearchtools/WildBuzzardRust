# W9-A4V/W9-A4W-F3 verified renderer capability handoff

## Outcome

W9-A4W-F3 preserves the F2 corrections and closes the three R3 review issues
in the Linux presenter/widget slice. It does not claim affirmative hardware
acceleration, compositor acknowledgement, sandbox acceptance, or Firefox
parity.

The public acceleration values remain conservative:

- `Unverified`: EGL selected a non-slow candidate, but no affirmative typed
  renderer capability proves hardware acceleration;
- `Software`: EGL selected a slow configuration or pinned `WebRender` returned
  its exact typed `RendererError::SoftwareRasterizer` result;
- `Accelerated`: reserved for a future affirmative typed proof. No current
  constructor path produces it.

Wild Buzzard first-party presenter/widget code does not read `GL_VENDOR` or
`GL_RENDERER`, compare renderer/driver names, or branch on environment
overrides, VM identity, virgl/llvmpipe labels, or site-specific data. It
consumes only pinned WebRender's typed
`RendererError::SoftwareRasterizer` result.

That typed result is a negative classifier, not name-independent attestation.
The pinned imported WebRender implementation reads its `renderer_name`,
lowercases it, and returns `SoftwareRasterizer` when the string contains
`llvmpipe`, `softpipe`, or `software rasterizer`. Therefore renderer names are
consulted inside imported WebRender for this negative classification. Absence
of one of those matches remains `Unverified`; it never proves acceleration,
and neither imported nor first-party code in this slice supplies a positive
vendor-string hardware attestation. The Firefox reference checkout was not a
build input.

## F3 correction: strict renderer reset protection

Strict WebRender admission now requires both
`LinuxAccelerationClass::Accelerated` and
`LinuxResetProtection::LoseContextOnReset`, exactly matching the direct-path
policy. `Accelerated/Unavailable` produces the distinct typed
`ResetProtectionUnavailable` strict-policy rejection. The gate executes after
the startup renderer classification is available but before capability
publication, document creation, any frame, or any native swap; failure retires
the unpublished renderer/presenter ownership graph.

The existing adversarial `Unverified` and `Software` strict-policy tests remain
and a separate regression covers exact `Accelerated/Unavailable` rejection
plus its `AutomaticCompatible` admission. `Accelerated/LoseContextOnReset`
remains reserved for future affirmative typed proof. No current constructor
emits `Accelerated`.

## F2 correction 1: strict direct presentation

`PresentationContract` now has a private one-way publication state:

1. `Unpublished`;
2. `SoftwareCorrected` during the sole typed startup correction;
3. `DirectPublished`; or
4. `RendererPublished`.

The direct path must pass `publish_direct_startup` before every submission.
The widget shell additionally consumes an attached owner through
`LinuxPresentedWindow::into_direct_diagnostic` before it can publish `Ready`.
Under `StrictHardware`, `Unverified` and `Software` return the typed terminal
`VerifyRenderer/PolicyRejected` result. `failed_owned_startup` then performs
the same exact checked wrapper teardown used by other owned-startup failures,
returning either `WrappersReleased` or fail-closed
`RetainedAfterTeardownFailure` evidence for the exact surface and unchanged
capabilities. The shell releases the matching surface identity and returns
without a `Ready`, frame request, swap, or receipt.

`submit_direct` repeats the policy gate, so bypassing the shell cannot operate
an unadmitted strict owner. `AutomaticCompatible` direct mode remains
operational and publishes `Unverified` or `Software` exactly as selected; it
does not fabricate `Accelerated`.

## F2 correction 2: atomic software correction

The typed `SoftwareRasterizer` correction now validates every precondition
before mutation:

- the exact owner is live and active;
- publication state is exactly `Unpublished`;
- committed frame count is zero;
- last submitted sequence/receipt identity is absent; and
- acceleration state is exactly `Unverified`.

Only after all checks pass does it replace `Unverified` with `Software` while
preserving the verified reset-protection fact and advance to
`SoftwareCorrected`. A successful renderer constructor seals the exact final
capability through `RendererPublished` before the owner can escape startup.

Repeated correction, correction after direct publication, correction after a
committed direct receipt, correction after renderer publication, suspended
state, or contradictory capability state returns a typed failure and latches
the owner terminal without changing the prior capability. Existing immutable
receipts and shutdown/retention evidence therefore cannot be retroactively
relabelled.

The startup-only WebRender handshake remains:

1. attempt an `Unverified` EGL candidate with software-rasterizer rejection;
2. retain `Unverified` when construction succeeds;
3. on the exact typed software result, atomically bind `Software`;
4. under `AutomaticCompatible`, retry once with software admitted and no page
   frame/document replay;
5. under `StrictHardware`, reject the corrected software owner; and
6. stop on every generic error, OOM, context loss, maximum-texture failure,
   panic, malformed binding, second software result, or uncertain teardown.

The temporary outer `Rc<dyn Gl>` is still dropped before either failure
retirement or successful constructor continuation. Only the renderer/presenter
ownership graph survives a successful handoff.

## F2 correction 3: production profile-window startup identity

Production `ShellApplication` retains the exact bounded inventory of every
native `WindowId` created by the fixed four-profile ladder. Its selection state
is explicit: `Collecting` or `Selected(exact_id)`.

While the event loop is running, every native window event is classified
before attempting to borrow a selected presenter. An event observed in
`Collecting` fails closed as typed `EventBeforeSelection`, records the
authoritative shell error, initiates `SurfaceIdentityViolation`, and cannot
escape to the callback/event queue. It is not counted as an ignored or retired
event.

The deterministic test covers all production dispatch classes: resize, scale,
focus, keyboard, modifiers, cursor enter/move/leave, mouse button, mouse wheel,
touch, IME, redraw, close, destroyed, and the bounded `Other` class.

After publication only:

- the exact selected ID may enter normal dispatch and must agree with the
  presenter's owner predicate;
- an exact recorded nonselected attempt ID may be counted and ignored as
  retired;
- an unknown ID, duplicate ID, fifth attempt, missing/ambiguous selection,
  selected/retired owner mismatch, absent selected owner, or checked counter
  exhaustion fails closed.

This accounts for normal queued events from startup retry windows without
weakening production ownership for foreign identities.

## F2 correction 4: standard error chain

`LinuxShellError::source` now returns the wrapped
`LinuxProfileWindowIdentityError`. A regression follows the standard
`std::error::Error` chain and downcasts to the exact typed cause.

## Deterministic and build evidence

F2-generated state is under:

`/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a4w-f2`

F3-generated state is under:

`/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a4w-f3`

All cited Podman calls used only:

`/run/media/user/Data/Repositories/wildbuzzardbuilds/podman/podman-wildbuzzard`

That wrapper fixes graph root to the Data drive and run root to
`/run/user/1000/wildbuzzard-podman`.

Network provenance is not uniform across the complete review history. The
owner-recorded isolated gates used `--network none`. The orchestrator's first
independent widget rerun was locked/offline and passed 40/40, but its container
network namespace was left enabled and it wrote `orchestrator-target`.
The orchestrator then repeated both widget 40/40 and presenter 127 library plus
1 example with `--network none` successfully. F3 verification also uses
locked/offline Cargo with `--network none`, source mounted read-only, and
`CARGO_HOME`/`CARGO_TARGET_DIR` under the F3 Data-drive directory. No claim is
made that every historical container call had networking disabled.

F3 log paths below are relative to the F3 artifact directory. Explicitly
identified F2-only gates were not rerun because F3 changed no widget source or
public Rust API/documentation.

| Gate | Exact result | Log |
| --- | --- | --- |
| Focused strict renderer test | PASS: exact `Accelerated/Unavailable` typed prepublication rejection, 1/1 | `logs/focused-presenter-strict-reset.log` |
| Focused production window-identity test | PASS: 1/1 | `logs/focused-widget-profile-identity.log` |
| Presenter all-target tests | PASS: 128 library + 1 example test | `logs/presenter-all-target-tests.log` |
| Widget all-target tests | PASS: 40 library; binary has 0 tests; one explicit live wrapper ignored | `logs/widget-all-target-tests.log` |
| Presenter Clippy | PASS: all features/targets, no deps, `-D warnings`, `all`, `pedantic` | `logs/clippy-presenter-all-pedantic.log` |
| Widget project Clippy | F2 retained PASS: all features/targets, no deps, `-D warnings` | F2 `logs/clippy-widget-warnings-denied.log` |
| Widget extra pedantic diagnostic | F2 retained NOT GREEN: 21 inherited findings/categories | F2 `logs/clippy-widget-all-pedantic-diagnostic.log` |
| Presenter rustdoc | F2 retained PASS: all features, no deps, warnings denied | F2 `logs/rustdoc-presenter.log` |
| Widget rustdoc | F2 retained PASS: all features, no deps, warnings denied | F2 `logs/rustdoc-widget.log` |
| Presenter release, all targets | PASS; one inherited imported-WebRender `frame_id` warning | `logs/release-presenter.log` |
| Widget release, all targets | PASS; same inherited imported-WebRender warning | `logs/release-widget.log` |
| Explicit rustfmt check | PASS on all eight authorized Rust paths | `logs/rustfmt-check.log` |
| Full-worktree diff check | PASS | `logs/git-diff-check.log` |
| Scoped dependency/Firefox-input scan | PASS: no task manifest/lock change and no `firefox/` target input | `logs/dependency-firefox-input-scan.log` |

The widget pedantic diagnostic is not claimed as passing. Its 21 findings are
the same inherited category count recorded by W9-A4W (including unwritable
`config.rs`/`normalize.rs` findings and pre-existing public-doc/function-size
findings in `shell.rs`). The normal warnings-denied project gate is green.

## Release smoke artifacts

| Binary | Bytes | SHA-256 |
| --- | ---: | --- |
| `binaries/webrender-window-smoke` | 12,458,200 | `2aa626cd44203e53fb93c97519a52250570536ca15bc568bb622d2624a3aa5dd` |
| `binaries/wild-buzzard-real-display-smoke` | 9,807,448 | `9e19e87582948e7707a9ef604a09616843bcbf92da3979f81efbba4750877c56` |

These are the F3 release binaries. They were not transferred to or run in the
guests because no current constructor emits `Accelerated`, so the changed
strict-only value pair is unreachable in every current live profile. The F2
guest-verified binary hashes and live results remain recorded in the retained
matrix below. No VM XML or graphics-driver environment was changed for F3.

## Retained F2 live matrix (not rerun for F3)

F3 changes only strict admission of the future
`Accelerated/Unavailable` value pair. No current constructor emits
`Accelerated`, and every previously observed physical/VM profile was
`Unverified` or `Software`. The code change therefore cannot alter those live
current-profile paths. The prior live matrix remains the applicable
operational evidence; F3 adds deterministic adversarial coverage and makes no
new screenshot, compositor-visibility, or VM-pass claim.

All live logs named below remain under the F2 artifact directory.

The retained F2 guest-tested binaries were:

| Binary | F2 SHA-256 |
| --- | --- |
| `webrender-window-smoke` | `8f51542eaad3d977f7f0deac11b40b05ddbf2b6544c4ed1a4da0369c58ca3a10` |
| `wild-buzzard-real-display-smoke` | `c07d4e0ed52bd31e321cab3a2d215863469074e15aae5d5e79007a0bbe376519` |

### Physical host, Wayland

- WebRender: PASS, exit 0.
- Final capability: `Unverified/LoseContextOnReset`.
- One profile window, zero retired events, selected-owner resize/redraw, two
  browser frames, exact EGL swap, ordered shutdown.
- Direct diagnostic: PASS, exit 0; capability remains `Unverified`.
- No compositor acknowledgement or screenshot claim is made for this row.

Logs: `logs/physical-wayland-webrender.log` and
`logs/physical-wayland-direct.log`.

### Debian 13, ordinary virtio 3D-disabled, Wayland 1280x800

- Exact guest hashes matched the retained F2 hashes above.
- WebRender: PASS, exit 0.
- EGL began `Unverified/LoseContextOnReset`; exact typed
  `SoftwareRasterizer` corrected the unpublished owner to
  `Software/LoseContextOnReset`; one software-admitted retry produced two
  selected-owner frames.
- One profile window, zero retired events, no page replay.
- Direct diagnostic: PASS, exit 0.
- Seven captures for each run succeeded. Visual inspection confirms the
  WebRender browser surface/application menu and the direct blue diagnostic
  frame. This is external scanout evidence, not internal compositor
  acknowledgement.

Log: `logs/debian13-wayland-live.log`.

### Ubuntu 26.04, ordinary virgl/blob+memfd, Wayland

- Exact guest hashes matched the retained F2 hashes above.
- WebRender: PASS, exit 0.
- Attempts: `Unverified/LoseContextOnReset`, then
  `Unverified/Unavailable`.
- Two profile windows, three exact retired-attempt events, selected-owner
  resize/redraw, two frames; final evidence is `Unverified/Unavailable`.
- Direct diagnostic: PASS, exit 0.
- Libvirt captures report `Display output is not active`; this is operational
  internal frame/swap evidence, not qualifying visible scanout.

Log: `logs/ubuntu2604-wayland-live.log`.

### Ubuntu 26.04, ordinary virgl X11/Xwayland

- Exact guest hashes matched the retained F2 hashes above.
- Current F2 WebRender run: PASS, exit 0, two attempts, two exact retired
  events, two selected-owner frames, final `Unverified/Unavailable`.
- Current F2 direct run: PASS, exit 0.
- The direct X11 matrix row remains **FAIL/nonqualifying**. One current pass
  does not overturn the prior controlled 20-run result (9 pass, 11 fail): seven
  exact initial-client-extent mismatches and four immutable-receipt-versus-
  later-handler-size failures. Receipt identity/size invariants were not
  weakened, and no intermittent row is promoted on a single pass.
- Libvirt captures still lack active display output and are nonqualifying.

Log: `logs/ubuntu2604-x11-live.log`.

## Remaining blockers and limitations

- No current path has affirmative typed hardware acceleration proof. Physical
  and virgl frames remain operational `Unverified` evidence.
- Consequently `StrictHardware` correctly rejects every current direct and
  WebRender owner. A future affirmative typed proof is required to operate
  strict mode.
- Same-process compatible non-robust rendering is not release sandbox/process-
  isolation acceptance.
- `WebGL`, `WebGPU`, and accelerated canvas remain unavailable on compatible or
  software profiles.
- Ubuntu's present virgl topology lacks qualifying observable scanout.
- Ubuntu X11 direct remains intermittently nonqualifying as detailed above.
- The aggregate widget all/pedantic Clippy diagnostic has 21 inherited
  findings; the warnings-denied project gate passes.
- Imported WebRender retains one release-only unused-field warning outside
  this task's writable paths.
- This bounded slice does not establish full browser, JavaScript, media,
  YouTube, security, accessibility, performance, or AppImage parity.

No files were staged, committed, or pushed.

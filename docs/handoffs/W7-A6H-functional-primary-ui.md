# W7-A6H functional Rust primary-UI handoff

## Decision and boundary

W7-A6H is GO for one bounded, same-process Linux x86-64 browser-primary-UI slice. The canonical
Rust session now drives tabs, navigation controls, address editing, site identity, panels,
keyboard focus, receipt-bound pointer interaction, and popup scrolling through W7-A4S's first
WebRender primary-chrome scene. It is not a Firefox browser-UI parity claim and is not release
acceptance.

The behavioral baseline is the pinned read-only Firefox ESR153 checkout at
`c19b7e89270787889495688244ec6ee8e79288a1`. Firefox is reference material only and remains absent
from every build and runtime path. Wild Buzzard retains its own identity and artwork.

The implementation is first-party Rust in:

- `browser/wild_buzzard_ui/` for canonical session/UI state, actions, focus, and normalized input;
- `browser/wild_buzzard_shell/` for the one-pass A4 projection and presented interaction authority;
  and
- `widget/rust/wild_buzzard_linux/` for typed Linux events and the checked native-resize ordering
  needed by the functional live smoke.

A4 remains authoritative for geometry, scene construction, native presentation, and graphics hit
testing. This handoff records the integrated contract and its residual gaps.

## Canonical revision and action contract

Each live browser window owns one never-zero monotone `PrimaryUiRevision`. An immutable
`PrimaryUiSnapshot` contains exact window identity, direction, focus, controls, tabs, an optional
panel, and semantic nodes. Any presentation- or action-relevant state change advances the
revision, including tab/history/loading/address/focus changes and A4-resolved layout membership.

`PrimaryUiSnapshot::bind_action` produces an opaque binding only for an enabled semantic element.
It retains exact `{window, revision, source, action}`. Dispatch rechecks the current binding before
effect. Superseded bindings return `Stale`; missing, hidden, overflow-inaccessible, or unavailable
sources return `Disabled`; a valid no-op returns `NoChange`. Revision exhaustion is terminal and
never reuses authority.

The fixed control inventory is Back, Forward, ReloadStop, SiteIdentity, AddressBar, NewTab,
AllTabs, ApplicationMenu, and Overflow. Back/Forward derive from the active tab's exact history
index. ReloadStop exposes Stop only for an exact loading navigation and prevents stop replay while
cancellation is pending. NewTab observes both 64-tab per-window and 64-tab session limits.

Tab body and close bindings retain the exact never-reused tab ID. Opening, activating, closing,
successor selection, navigation, reload, stop, address/content focus, and address submission reuse
typed `BrowserSession` commands. The address editor keeps a bounded UTF-8 draft and selection per
tab, accepts committed text only through the normalized text/IME route, uses code-point-safe cursor
and deletion operations, submits on Enter, and reverts draft/preedit state on Escape.

Site identity deliberately does not invent security assurance: `NoPage`, exact numeric-loopback
HTTP, other insecure HTTP, and `Unverified` are the only classifications. HTTPS remains
`Unverified`; there is no TLS or certificate result in this lane. The site panel contains one
disabled informational row.

## Panels, overflow, focus, and shortcuts

At most one primary panel is open. Toggling its anchor or using Escape closes it; the exact
receipt-bound outside shield dismisses it. Opening any popup clears address preedit and moves
canonical focus out of the address editor. Focus is repaired if layout or panel membership removes
the current target.

- AllTabs retains every live tab in strip order, marks the active row, and exposes a bounded
  visible row window. Keyboard selection keeps the selected row visible.
- ApplicationMenu contains New Tab, Close Tab, Back, Forward, and Reload/Stop with availability
  inherited from canonical controls.
- Overflow contains A4's exact ordered relocated-control membership; relocated controls preserve
  their original identity and action.
- SiteIdentity exposes only its disabled informational row.

Zero popup capacity or loss of a visible anchor closes the panel and disables the unpaintable
entry point. `PrimaryUiLayout` rejects overlap, missing controls, illegal overflow membership,
spurious overflow anchors, and capacity above 64.

The normalized Linux key map implements the bounded behavior below:

- F6 and Shift+F6 move between page and address regions; ordinary Tab remains page input while page
  focus is active.
- Tab/Shift+Tab traverse enabled visible chrome stops. Left/Right traverse toolbar controls with
  RTL reversal. Open panels use Tab/Down and Shift+Tab/Up, Enter/Space, and Escape.
- Ctrl+Shift+Tab opens AllTabs, matching pinned ESR's `key_showAllTabs` binding.
- Ctrl+PageUp selects the previous tab; Ctrl+Tab/Ctrl+PageDown select the next tab.
- Ctrl+Shift+A is intentionally unclaimed because Firefox reserves it for Add-ons.
- Existing bounded shortcuts cover new/close tab, address focus, back/forward, reload/stop, and tab
  positions one through nine.

This is not Firefox's complete grouped toolbar navigator, urlbar result navigation, mnemonic/access
key model, tab-strip ARIA behavior, or extension/customization focus graph.

## One-pass A4 projection

Before building a frame, the shell asks A4 for a pure physical layout preview. It converts the
exact visible/overflow/hidden controls and popup capacity into canonical `PrimaryUiLayout`, installs
that membership, and takes the next immutable snapshot. The scene builder must resolve the same
preview exactly. A changed tab inventory or preview mismatch rejects before publication, so there
is no mismatched first frame followed by compositor feedback.

The scene contains typed control, tab, close, page, popup-row, and popup-dismiss identities. The
shell builds a bounded `PresentedUiAuthority` beside it. Only a successful exact
`BrowserFrameReceipt` publishes that authority.

## Exact pointer transaction and visual state

A pointer hit is a candidate, not authority. Admission binds the exact successful receipt,
surface, page, UI revision, seat, device, pointer ID, pointer kind, semantic target kind, and action
binding.

An exact primary `Down` begins capture and performs no action. The same exact contact must end with
primary `Up` and no remaining buttons over the same authoritative target; only then is the binding
invoked once. Mixed chords, drag, leave, cancel, stale receipt, surface change, device or pointer
kind change, disabled target, and unrelated authority replacement cancel capture. A dropped contact
cannot be replayed after scene reconstruction.

Hover and pressed states are canonical visual inputs. Their redraw may publish a successor receipt
without losing the exact live contact: the shell performs an explicit safe visual-receipt handoff
only for that bounded redraw. An unrelated redraw cancels capture. Painted disabled targets receive
`ConsumeDisabled`, so they consume interaction without action, focus mutation, or accidental page
fallthrough.

`DeviceEvent::Removed` now retires the normalizer's exact never-reused input-device identity,
delivers a typed non-coalesced `InputDeviceRemoved` event for the exact surface, and clears affected
pointer capture, scroll accumulation, and hit authority before any later action can escape.

## Popup scrolling

Receipt-bound scrolling is implemented for the focused AllTabs popup. Pixel deltas accumulate in
an exact `{receipt, surface, popup, seat, device}` context. Begin resets, Update accumulates, End
applies the final delta then clears, and Cancel discards. One row step is 40 physical pixels;
remainder survives AllTabs' own redraw receipt so a burst does not lose motion. Context, popup,
device, receipt, or authority drift resets fail-closed. Line and page units use bounded discrete
movement. Other panels do not claim scrolling.

## Linux resize ordering fixed by W7-A6H integration

The same-binary smoke exposed two real backend ordering differences in pinned winit 0.30.13:

- Wayland applies a stateless size in winit and returns `Some`, but the retained `wl_egl_window`
  cannot reach that extent until the presenter's checked resize/recreate transaction runs.
- X11 returns `None`; after a request, native EGL can observe the new extent before the matching
  winit `Resized` callback, while an older queued redraw still carries the prior Rust contract.

The presenter now distinguishes `Confirmed`, `ReadyForCheckedResize`, and `Pending`. Wayland's
ready result enters the canonical checked resize immediately. `None` and pending results persist an
exact requested-size guard across callbacks. Interstitial native redraw delivery is coalesced; it is
reissued once only after a material `Resized` has passed presenter verification and descriptor
publication. A same-size callback releases only when EGL is exactly confirmed and the observed size
equals the pending request. Unequal duplicates retain the guard. Stop, suspension, and destruction
discard the guard and deferred redraw; `WindowEvent::Destroyed` reaches that cleanup through
`begin_stop` before teardown.

This closes the captured X11 failure where native EGL observed 2560x1600 while the stale contract
still expected 2400x1480, and the Wayland deadline where 1440x880 remained pending from an initial
1600x1000 surface.

## Accessibility data boundary

Snapshots expose generic semantic nodes for page, tabs, closes, controls, and panel rows with exact
identity, role, name, enabled, selected, expanded, focused, and visible state. This is adapter-ready
Rust data only. There is no Linux AT-SPI tree, relation/action/event publication, screen-reader
integration, or accessibility parity claim.

## Firefox reference evidence

Focused implementation and tests were inspected in the read-only ESR checkout, including:

- `browser/base/content/browser.xhtml`, `navigator-toolbox.inc.xhtml`, `browser-sets.inc.xhtml`,
  `browser.js`, and `browser-toolbarKeyNav.js`;
- `browser/components/tabbrowser/content/tabbrowser.js`, `tabs.js`, `browser-allTabsMenu.js`, and
  `browser-allTabsMenu.inc.xhtml`;
- `browser/base/content/test/general/browser_documentnavigation.js`;
- `browser/base/content/test/keyboard/browser_toolbarKeyNav.js` and
  `browser_toolbarButtonKeyPress.js`;
- `browser/components/customizableui/test/browser_panel_toggle.js`,
  `browser_940307_panel_click_closure_handling.js`, `browser_hidden_widget_overflow.js`, and
  `browser_overflow_use_subviews.js`; and
- focused URL-bar and accessibility tests used to define gaps, not to claim implementation.

History inspection used `git log --follow`, `git log -S`, and relevant backout/reland chains to
understand grouped toolbar navigation, all-tabs subviews, and URL-bar integration invariants.

## Verification evidence

All W7 build artifacts are outside the repository under
`/home/user/Documents/wildbuzzardbuilds/w7-a6h/`. Toolchain: Cargo 1.96.0, rustc 1.96.0, target
`x86_64-unknown-linux-gnu`.

Frozen source gates:

- all W7-owned Rust files pass `rustfmt --edition 2024 --check`;
- widget `cargo test --locked --all-targets`: 34 passed, opt-in real-display target 0 run;
- integrated shell `cargo test --locked --all-targets`: 33 library tests passed, main 0;
- strict `cargo clippy --locked --all-targets -- -D warnings` passes for widget and shell;
- `RUSTDOCFLAGS=-Dwarnings cargo doc --locked --no-deps` passes for widget and shell; and
- final standalone UI `cargo test --locked --all-targets`: 38 library, 30 browser-session, and three
  navigation-port tests passed (71 total).

The exact release binary is
`/home/user/Documents/wildbuzzardbuilds/w7-a6h/shell/x86_64-unknown-linux-gnu/release/wild-buzzard`,
size 22,637,472 bytes, SHA-256
`ae1c1c7c7a5ddbf689c22da72424ffb639b0a946c8e9381d594ace1a339fc9a6`.

Live commands used the exact binary with `WILDBUZZARD_REAL_DISPLAY_TEST=1` and a 30-second external
timeout. The smoke opens and dismisses the application popup, resizes away and back, exercises two
tabs/navigation, closes back to one tab, holds, and requests clean shutdown. It is programmatic
same-binary functional evidence, not injected human pointer automation.

- Owner run: Wayland 1/1 passed, success marker 16 compositions, clean `Requested` exit.
- Owner run: X11 10/10 sequential passed, success markers 13-15 compositions and clean `Requested`
  exits; final counts can be one higher after the correctly reissued deferred redraw.
- Independent hostile review: Wayland 5/5 and X11 10/10 sequential passed on the frozen binary.
- On this host the normal 1280x800 logical request was observed as 1600x1000 at scale 1.25 on
  Wayland and 2560x1600 at scale 2 on X11; resize-away targets were 1440x880 and 2400x1480.

The marker proves successful WebRender/EGL swap submission and exact functional state progression.
It does not prove that a desktop compositor visibly displayed every buffer.

## Explicit residual gaps

- The normal-size tab strip still divides all tabs across the available strip. At 1024 physical
  pixels, 50 and 64 tabs shrink to roughly 19 and 15 pixels rather than preserving Firefox-like
  minimum-width tabs with horizontal overflow/scroll. Tab minimum width, strip scrolling,
  reorder/drag, pinning, mute state, close affordance policy, previews, groups, and tear-off remain a
  dedicated normal-size lane.
- No TLS/certificate verification, secure or mixed-content identity, permission controller,
  tracking protection, certificate details, or security-panel actions.
- No URL-bar autocomplete, suggestions/results, search modes/engines, switch-to-tab, keywords,
  page actions, formatting, or Firefox URL-bar accessibility relations.
- No broad settings, bookmarks, history, downloads, passwords, prompts, permissions, extensions,
  developer tools, private/container windows, multiple windows, profiles, persistence, session
  recovery, notifications, sidebars, nested subviews, or full customization.
- No Linux AT-SPI adapter, localization/Fluent output, localized shortcuts, screen-reader suite,
  or locale-driven direction policy.
- Pointer/touch coverage is bounded to the implemented authority state machine; there is no live OS
  pointer-injection suite, tab drag/drop, context menu, popup animation, or broad touch gesture
  policy.
- No claim of general DNS/HTTPS browsing, JavaScript page execution, storage, media, process
  isolation, WebDriver, AppImage closure, normal-site/YouTube compatibility, or Firefox parity.

# W7-A4S Firefox-like primary chrome compositor handoff

## Scope and decision

- Task: extend the W6-A4R Rust browser compositor with the first bounded real primary toolbar and
  popup surface: back, forward, combined reload/stop, site identity, URL editor, new tab, all tabs,
  application menu, and deterministic overflow.
- Owner: Agent 4 — graphics/GPU. Browser-session state and action dispatch remain Agent 6 work;
  root integration, product status, and release acceptance remain orchestrator-owned.
- Baseline: repository `1c7f4aa2d78fafc43172bfbb0ad50580f703afa6`; read-only Firefox ESR153
  reference `c19b7e89270787889495688244ec6ee8e79288a1`.
- Writable scope used: `gfx/wild_buzzard_linux_presenter/**` and this handoff. No root manifest,
  lockfile, renderer crate, widget crate, browser-session lane, JavaScript lane, program-status file,
  `AGENTS.md`, or Firefox-reference source was edited by this owner.
- Decision: GO for this bounded graphics/session integration surface after the unit, structural,
  static-analysis, documentation, release-build, and live Wayland/X11 evidence recorded below.
  It is explicitly NO-GO for Firefox toolbar/panel parity, complete browser-product behavior,
  accessibility parity, or release acceptance.

This is not a painted screenshot mockup. The scene consumes typed immutable browser state, validates
its identities and action availability, resolves scale/direction/overflow before publication,
builds real WebRender primitives and shaped text, returns semantic receipt-bound hit targets, and
submits the existing hardware EGL surface. Visual resemblance alone is not accepted as behavior
evidence.

## Preserved W6 compositor boundary

W7 changes only chrome scene construction, painting, and the retained typed hit map. The following
W6-A4R invariants remain intact and their regressions continue to pass:

- exact surface snapshot and generational revision;
- independent monotone page and chrome revisions;
- exact page, chrome, root, and native-swap epochs/sequences;
- page `Install`/`Retain`/`ClearToBlank` transition rules;
- page pipeline collision checks and exact retired `(PipelineId, DocumentId)` removal;
- complete current `PipelineInfo` epoch/removal verification rather than stale renderer-cache
  inference;
- one accepted WebRender transaction followed by exact build/ready/render evidence;
- direct WebRender draw to framebuffer zero and checked EGL swap;
- preaccept rejection preserving the old receipt and postaccept failure latching terminal;
- resize/scale/suspend/resume invalidating stale hit authority; and
- backend shutdown, renderer deinitialization, font release, non-current EGL state, and checked Rust
  wrapper release ordering.

`BrowserFrameReceipt::desktop_compositor_acknowledged()` remains false. The live evidence proves
WebRender submission and EGL swap acceptance, not that Mutter/Xwayland displayed the buffer to a
human.

## Browser-session ownership split

Graphics accepts one bounded immutable `BrowserPrimaryChromeState`; it does not derive product
actions, menu contents, history state, or permissions. The browser session is authoritative for
the exact `{UI revision, element identity, semantic kind, availability}` mapping and must
revalidate it before an effect. The chrome revision in the successful frame receipt binds the
graphics projection used for hit testing.

Every fixed control has:

- an opaque nonzero `BrowserChromeElementIdentity`;
- one `BrowserPrimaryControlKind` in canonical fixed order;
- an exact shaped localized name;
- one `BrowserElementAvailability::{Disabled, Enabled}` value; and
- one mutually exclusive `BrowserElementInteraction::{Idle, Hovered, Pressed}` value.

The fixed semantic inventory is `Back`, `Forward`, `ReloadStop`, `SiteIdentity`, `UrlBar`,
`NewTab`, `AllTabs`, `ApplicationMenu`, and `Overflow`. Missing, reordered, duplicated, or repeated
element identities reject. Disabled controls cannot claim hover or press. The sole
`BrowserReloadStopMode::{Reload, Stop}` determines both artwork and derived loading state; there is
no second loading boolean. `BrowserSiteIdentityKind` distinguishes Empty, internal, loopback HTTP,
secure, insecure, and mixed states without importing Firefox artwork.

Each `BrowserChromeTab` additionally carries exact
`BrowserElementInteraction::{Idle, Hovered, Pressed}` state for its body and an exact
`{BrowserElementAvailability, BrowserElementInteraction}` pair for its close action. The stable
tab identity plus the semantic `Tab`/`TabClose` hit kind is the authority; geometry never invents
an action identity. Disabled tab-close interaction rejects. The URL editor deliberately reuses the
existing `UrlBar` control identity, availability, and interaction rather than creating a second
address state. These states now drive distinct idle, hover, press, and disabled WebRender paint.

`BrowserChromeFocus` remains the sole focus source and now adds exact `PrimaryControl(element)` and
`PopupRow(element)` targets. URL editing deliberately keeps the existing canonical `AddressBar`
focus/hit target; a second competing primary-control URL hit was not added. Hidden, disabled, or
scrolled-out elements cannot claim focus. A zero-area row produced by an ordinary tiny resize is
collapsed, not hidden: exact logical focus/interaction may survive but produces no paint or hit
until nonzero geometry returns.

## Pure pre-publication layout seam

`BrowserPrimaryLayoutPreview::for_surface(surface, direction, tab_count)` is a pure bounded API
which runs before scene construction and exposes:

- exact per-kind placement and physical rectangle;
- exact URL container and canonical editable address rectangle;
- ordered tab body, direction-aware title, and direction-aware close rectangles;
- deterministic ordered hidden-control inventory; and
- `popup_row_capacity()` for the exact surface/scale, bounded to 64 and zero when no panel viewport
  fits.

This breaks the otherwise circular overflow/focus problem. Agent 6 can resolve the new surface,
store the same visible/overflow/focus/scroll membership in its next immutable UI revision, then
construct and publish one matching chrome scene. `BrowserChromeScene::new` calls the same resolver
and rejects any availability, popup inventory, scroll, or focus drift. No first mismatched frame or
compositor-feedback frame is required.

The resolver is total for every supported nonzero drawable surface. It does not return
`SizeMismatch` merely because width or height is small. When the nonoverflowable navigation row
cannot retain all fixed controls plus the minimum URL editor, it keeps the exact existing
Toolbar/AddressField semantic membership but collapses those rectangles, address text/selection,
and hit regions to bounded zero area. If the tab strip cannot retain its fixed affordance, its
rectangles similarly collapse while the exact tab/title/close arrays and identities remain. This
preserves A6's membership contract without fabricating overlapping hit authority. Resource limits,
zero-sized suspended surfaces, and malformed state still reject.

At scale one the exact surface threshold is 288 physical pixels: width 287 is collapsed, width 288
has the 64-pixel minimum URL editor, and width 289 has 65 pixels. Scale two has the corresponding
575/576/577 boundary and 128/129-pixel URL editor. A collapsed navigation row forces popup capacity
to zero even if the remaining outer surface could geometrically hold a floating panel; A6 must
disable anchors and omit the popup in the matching revision. Supplying stale popup state rejects as
a contract mismatch, never as a narrow-surface `SizeMismatch`.

The current deterministic physical geometry is derived from CSS-pixel constants through the exact
surface scale:

- inherited tab strip 36 and navigation strip 44;
- primary button 32, logical gap 4, and site-identity width 28;
- minimum editable URL width 64;
- new-tab narrow threshold 420 and minimum average tab width with new-tab present 72;
- popup requested width 300, minimum width 160, maximum height 420, row height 36, margin 8, and
  padding 8.

Back, forward, reload/stop, site identity, URL editor, all-tabs, and application menu never
overflow. `NewTab` is the first deterministic relocatable control. It moves to overflow when the
surface is below the narrow threshold, when retaining it would reduce the average tab allocation
below the bounded threshold, or when exact tab/control pixels do not fit. Its opaque identity and
state are retained when represented as an overflow row. `Overflow` is hidden when there is no
hidden inventory. It is visible but disabled when inventory exists and popup capacity is zero, and
enabled only when both inventory and capacity exist.

At zero popup capacity, SiteIdentity, AllTabs, ApplicationMenu, and Overflow must all be Disabled
and no popup may be supplied. The rest of the primary scene remains paintable. This fail-closed
policy was added after the A6 integration tests exposed the tiny-surface state seam.

Logical placement, tab order, tab-close logical edge, direction-aware title inset, back/forward direction, reload artwork,
overflow artwork, popup anchoring, and selected-row accent mirror in RTL. Scale-two structural
tests prove exact doubled geometry rather than relabeling scale-one pixels. The corrected title
bounds reserve the close and adjacent gap on physical right in LTR and physical left in RTL;
maximum-count narrow tabs prove the shaped title never overlaps the close rectangle.

## Popup contract

Exactly zero or one `BrowserPrimaryPopup` may be open. It carries a typed kind, the exact enabled
visible anchor identity, a complete bounded row inventory, and the first visible row. Each row has
an opaque identity, semantic kind, canonical availability/interaction/selection/expansion, and an
exact shaped label. The resolver retains the complete inventory but assigns rectangles only to the
capacity-bounded visible window. Focus must be inside that window.

W7 validates popup semantics rather than accepting arbitrary painted rows:

- `AllTabs` is anchored to AllTabs and must contain every live tab identity exactly once in exact
  order. The selected row must equal the active tab and every row is enabled. Long inventories
  retain all rows while a supplied scroll window controls the visible subset.
- `Overflow` is anchored to Overflow and must equal the pure preview's exact ordered hidden
  inventory. Each row retains the fixed control identity, kind, availability, interaction, and
  label; no independent painted state is accepted.
- `ApplicationMenu` is anchored to ApplicationMenu and admits only the real A6 W7 dispatch set:
  NewTab, CloseTab, Back, Forward, and ReloadStop. Back/forward/reload/new-tab availability must
  equal the corresponding fixed control; enabled CloseTab requires an active tab. Arbitrary,
  duplicate, unlabeled, or selected application actions reject. NewWindow and Settings are not
  exposed as fake enabled rows.
- `SiteIdentity` is anchored to SiteIdentity and admits only unique disabled informational
  SiteInformation and SitePermissions rows in this gate. It does not pretend that permission or
  certificate actions exist.

The expansion type is present for an exact future nested-view contract, but W7 rejects
Collapsed/Expanded because no child-view compositor was implemented. Painting an arrow without a
real child surface would be false behavior evidence.

## WebRender painting and hit authority

The primary scene paints:

- tab/navigation backgrounds, selected/loading/hovered/pressed tabs, stateful close affordances,
  and exact shaped titles;
- enabled/disabled/hovered/pressed/open/focused control backgrounds;
- Rust-authored geometric back, forward, reload, stop, site-identity, new-tab, all-tabs,
  application, and overflow marks;
- the exact URL/site field, address selection/caret, shaped URL and status text;
- popup dismiss shade, bordered panel, stateful rows, selected accent, shaped row labels, focus,
  and bounded scroll indicators.

Artwork is first-party geometric Rust output. No Firefox/Mozilla icon, SVG, trademark, application
ID, artwork, or branded string is copied. Directional marks are explicitly mirrored rather than
assuming a left-to-right bitmap.

The retained Rust hit map has strict topmost order:

```text
visible popup row
  -> popup non-row surface
  -> in-surface popup dismiss shield
  -> status overlay
  -> canonical AddressBar
  -> visible primary control
  -> tab close
  -> tab body
  -> clipped page-local point
```

Popup hits return `PrimaryPopupRow { element, kind }`, `PrimaryPopupSurface { kind, anchor }`, or
`PrimaryPopupDismiss { kind, anchor }`. Control hits return `PrimaryControl { element, kind }`.
Existing Page, Tab, TabClose, AddressBar, and Status variants are preserved. Every result carries
the exact last successful `BrowserFrameReceipt`; an accepted in-flight frame, resize, legacy scene,
foreign surface, or terminal failure still invalidates hit authority.

The WebRender display list also contains hit-test primitives in the same layer order, but the
synchronous retained Rust map remains this gate's browser input authority, as in W6.

## Bounds and resource accounting

Primary chrome remains caller-nonenlargeable and fully bounded:

- 64 tabs;
- 9 fixed controls;
- 64 popup rows;
- at most 139 shaped chrome text records (`64 tabs + address + status + 9 controls + 64 rows`);
- 1 MiB aggregate source UTF-8;
- 100,000 shaped glyphs;
- 16,384 shaped runs;
- 16 MiB serialized chrome display-list bytes; and
- 1 MiB serialized root display-list bytes.

The shaped-run cap increased from W6's 4,096 to 16,384 after integrated review identified a valid
64-tab AllTabs scene: every bounded tab title is shaped once in the tab strip and again in the
popup, before address/control labels. Text count, bytes, glyphs, runs, and display-list bytes remain
independent aggregate defenses.

Every allocation and arithmetic boundary used for layout, popup capacity, text counts, visible
windows, and display-list primitive counts is checked or guarded by the fixed small inventory.
Supported nonzero surfaces that are too narrow or short collapse rather than rejecting or silently
dropping a core control. Exhaustive scale-one regressions cover every width from 1 through 289,
both directions, four height boundaries, and the maximum 64-tab inventory; selected scene tests
cover 1px, threshold-minus-one, threshold, threshold-plus-one, popup, scale-two, and retained-focus
cases.

## Firefox ESR153 implementation, tests, and history inspected

The ignored Firefox checkout remained detached and read-only. Focused implementation/style review
covered:

- `browser/base/content/navigator-toolbox.inc.xhtml` for the tab/new-tab/all-tabs row, nonremovable
  nonoverflowing back/forward/URL container, combined stop/reload container, overflow anchor, and
  application button;
- `browser/themes/shared/toolbarbuttons.css`, `toolbarbutton-icons.css`, and
  `urlbar-searchbar.css` for pressed/open/checked/hover/focus state, combined stop/reload visibility,
  logical-direction transforms, toolbar density, and narrow URL minimums;
- `browser/themes/shared/identity-block/identity-block.css` and
  `browser/base/content/browser-siteIdentity.js` for identity availability, exact panel anchor,
  one-open-panel behavior, and focus-driven dismissal;
- `browser/components/tabbrowser/content/browser-allTabsMenu.js` and
  `browser-allTabsMenu.inc.xhtml` for visible anchor admission, live selected tab rows, selected-row
  scroll visibility, and subview ownership;
- `browser/components/customizableui/CustomizableUI.sys.mjs`,
  `content/panelUI.inc.xhtml`, and `content/panelUI.js` for overflow membership and sole panel state;
  and
- `browser/base/content/test/about/browser_aboutStopReload.js` for loading-driven combined
  reload/stop behavior.

Focused behavioral tests inspected include:

- `browser/base/content/test/keyboard/browser_toolbarKeyNav.js`: disabled forward is skipped and
  overflow participates only when visible;
- `browser/base/content/test/keyboard/browser_toolbarButtonKeyPress.js`: keyboard activation opens
  overflow and moves focus inside;
- `browser/components/customizableui/test/browser_panel_toggle.js`: exact open/closed state;
- `browser/components/customizableui/test/browser_940307_panel_click_closure_handling.js`:
  enabled actions dismiss, disabled actions retain, Escape dismisses, nested context interaction
  retains;
- `browser/components/customizableui/test/browser_hidden_widget_overflow.js`: an overflow marker is
  not shown for dimensionless hidden items alone;
- `browser/components/customizableui/test/browser_overflow_use_subviews.js` and
  `browser_884402_customize_from_overflow.js`: overflow subview and relocation behavior; and
- `browser/components/tabbrowser/test/browser/tabs/browser_list_all_tabs_telemetry.js` plus the
  all-tabs implementation's `ViewShown` selected-row scroll behavior.

Focused history used `git log --follow` and `git log -S`, including:

- `f60a0ac760c8` / `6234bcb2e3b0`, stop/reload rule consolidation and its regression backout;
- `a546491d4a05` / `610eed64a4ff`, overflow-button styling consolidation and regression history;
- `ab4cbe853ed9`, moving the list-all-tabs button;
- `99d830d71b21`, CustomizableUI module conversion relevant to current overflow ownership;
- `349e5c5be344`, current identity/trust UI changes; and
- recent navigator-toolbox, all-tabs, panel UI, URL bar, and toolbar-button follow history at the
  pinned ESR commit.

Wild Buzzard adopts observable state, focus, visibility, z-order, and failure invariants. It does
not translate Gecko's XUL/JS/CSS object graph or use Firefox as a build/runtime input.

## Verification evidence

All build artifacts stayed under the required external target:
`/home/user/Documents/wildbuzzardbuilds/w7-a4s`. The final tree was 3.6 GiB after the release
example completed. Toolchain: Cargo 1.96.0 (`30a34c682`), rustc 1.96.0
(`ac68faa20`, LLVM 22.1.2), host/target `x86_64-unknown-linux-gnu`.

Final static/unit/documentation gates:

```sh
export CARGO_TARGET_DIR=/home/user/Documents/wildbuzzardbuilds/w7-a4s

cargo fmt --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml -- --check

RUSTDOCFLAGS='-D warnings' cargo test \
  --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --all-targets --all-features

cargo clippy --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --all-targets --all-features -- -D warnings

RUSTDOCFLAGS='-D warnings' cargo doc \
  --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --all-features --no-deps

cargo build --manifest-path gfx/wild_buzzard_linux_presenter/Cargo.toml \
  --locked --release --all-features --example webrender-window-smoke
```

The reopened final all-target matrix passed all 91 tests. The new evidence covers deterministic
wide/narrow/scale/RTL layout snapshots, direction-aware tab-title bounds, typed tab/tab-close/URL
interaction artwork,
overflow identity retention, application/site/all-tabs popup validation, complete 64-row scroll
inventory, total bounded nonzero-surface degradation, zero-capacity fail-closed policy, canonical
state drift rejection, and receipt-bound control/popup z-order hits. All inherited W4/W5/W6 tests
remain in the same matrix.

The real smoke submits two frames: a genuine compiled page plus closed primary chrome, then a
retained page plus independently revisioned open application popup. It checks shaped font
resources, page/address hits on the first receipt, popup-row/dismiss hits on the second receipt,
page epoch retention, chrome epoch advancement, exact two-swap native accounting, confirmed
backend/renderer teardown, and released font resources.

Final live output:

```text
W7-A4S wayland primary-toolbar+application-popup publish=2 page_epoch=Some(1) chrome_epoch=2 resize=observed EGL_swap=accepted compositor_ack=false
W7-A4S x11 primary-toolbar+application-popup publish=2 page_epoch=Some(1) chrome_epoch=2 resize=observed EGL_swap=accepted compositor_ack=false
```

Both runs used the host's live `WAYLAND_DISPLAY=wayland-0` and X11/Xwayland `DISPLAY=:0`, under the
example's independent 25-second parent deadline. They are defensible WebRender/EGL/backend
evidence, not desktop-compositor acknowledgement.

## Dependency, unsafe, branding, and privacy audit

- No `Cargo.toml`, `Cargo.lock`, third-party version, feature, native library, endpoint, ambient
  capability, or telemetry edge changed.
- `primary_chrome.rs` and `browser_compositor.rs` use `#![forbid(unsafe_code)]`; the example adds no
  unsafe. Existing audited EGL/GL unsafe remains localized in `egl_window.rs` and is unchanged.
- Primary presentation performs no pixel readback, CPU framebuffer copy, software fallback, or
  additional renderer creation. The W4 diagnostic pixel path remains separate and unchanged.
- The only URL-shaped strings added are local `about:`/loopback/test fixtures; no request is made.
- No Mozilla name, Firefox artwork, Firefox application ID, profile path, default, icon, or
  affiliation is introduced. The live title/application ID and visible tab branding remain Wild
  Buzzard.
- Firefox reference files were not edited and are neither dependencies nor runtime inputs.

## Explicit limitations and next contracts

- Graphics presents browser-owned immutable state; it does not dispatch Back/Forward/Reload/Stop,
  edit/accept URLs, create/close/select tabs, apply permissions, or persist menu state. Agent 6
  owns those commands and exact UI-revision revalidation.
- Keyboard traversal, mnemonics, accelerator semantics, pointer capture, drag/drop, wheel-to-scroll
  event routing, IME integration with the URL editor, screen-reader semantics, and Linux AT-SPI
  exposure remain browser/widget/accessibility work. The data model and exact rect inventory are
  prerequisites, not substitutes.
- Only one flat popup view is painted. Nested subviews, animation, panel flip/slide, overflow
  customization, extensions, downloads, bookmarks, history menus, search suggestions, permission
  controls, certificate details, and full application-menu contents remain open.
- The all-tabs scene retains a bounded complete inventory and visible scroll window; the graphics
  contract does not itself consume a wheel/keyboard scroll command.
- The current NewTab relocation rule is deterministic Wild Buzzard policy, not a claim of exact
  Firefox responsive thresholds or customizable-toolbar behavior.
- Page/chrome composition remains same-process. GPU/content process isolation, crash recovery,
  context recreation, damage/buffer-age, vsync/frame callbacks, desktop-compositor confirmation,
  AppImage closure, and release performance/memory gates remain open.
- The result is the first real Firefox-like primary surface, not observable Firefox ESR toolbar or
  browser-product parity. That claim requires integrated A6 action, keyboard, accessibility,
  session, and broader conformance evidence.

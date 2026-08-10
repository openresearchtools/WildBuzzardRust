# W9-A6J general-web browser-shell wiring

## Outcome

The ordinary Rust browser product now selects the distinct general-web engine
capability. A startup URL, address-bar submission, back, forward, and reload all
construct `NavigationRequest::general_web` and enter the reviewed
DNS/authenticated-HTTPS presentation worker. The existing `--smoke` product
test retains the narrow numeric-loopback worker and request capability.

This is capability and presentation wiring, not a normal-site or Firefox-parity
claim. Redirects remain typed and blocked until the session can publish the
final URL and connection-security identity. HTTPS therefore remains visually
unverified in chrome rather than receiving a false lock indication.

## Contract changes

- `wild_buzzard_engine` re-exports the immutable `GeneralWebConfig` and
  authenticated `TrustStore` needed by its public general-web constructors.
- `NavigationEnginePort` owns matching headless and presentation general-web
  constructors; it still cannot be forged from independently supplied engine
  and receiver halves.
- `BrowserNavigationMode` is fixed when a `BrowserSession` is created. The
  default constructor remains numeric-loopback for deterministic existing
  callers, while `new_with_navigation_mode` makes product authority explicit.
- Every request construction site in the product controller uses that one
  mode. A history or reload action cannot silently fall back to loopback or
  widen a loopback session.
- The normal Linux shell uses bundled Web PKI roots and default bounded
  general-web policy. The isolated smoke path remains loopback-only.

## Verification

Build output is external under
`/home/user/Documents/wildbuzzardbuilds/w9-a6i-general-navigation/cargo`.

- `wild_buzzard_ui`: 38 unit, 31 browser-session, 3 concrete-port, and 2
  compile-fail doctests pass after the authority regression is included.
- The focused authority regression proves six successive new/history/reload/
  submitted-address operations all remain `GeneralWeb`.
- `wild_buzzard_shell`: 34 unit tests pass, including the pure selection proof
  that ordinary product startup uses general web and smoke startup uses
  numeric loopback.
- Exact rustfmt, strict Clippy, release, and warning-denied rustdoc gates pass.
  Public live-desktop capture remains deliberately pending behind the generic
  automatic-margin blocker recorded by the current example.com probe.

## Remaining product blockers

- Publish redirect final URL, redirect chain, and connection-security metadata
  through engine events, history, and chrome before following redirects or
  showing authenticated identity.
- Normalize user-entered host/search text; this slice requires an explicit
  bounded HTTP or HTTPS URL.
- Connect page hit testing, scrolling, keyboard/pointer input, form controls,
  external resources, scripts/tasks, storage, images, and media.
- Run reproducible 1366x768 and 1920x1080 Wild Buzzard/Firefox ESR comparisons
  only after each public probe reaches a visible frame without a generic style
  or layout rejection.

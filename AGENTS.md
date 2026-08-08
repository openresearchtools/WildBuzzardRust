# Wild Buzzard agent operating agreement

## Mission

Wild Buzzard is an independent, privacy-respecting, general-purpose web browser whose first-party browser chrome, engine, and runtime are implemented in Rust.

The compatibility target is observable Firefox ESR parity:

- Web-engine behavior: ECMAScript, WebAssembly, DOM, HTML, CSS, layout, graphics, networking, storage, media, accessibility, extensions, and developer tooling.
- Browser-product behavior: navigation, tabs, windows, prompts, downloads, settings, history, bookmarks, passwords, permissions, sessions, recovery, keyboard operation, and accessibility.
- Security and privacy behavior: process isolation, origin boundaries, TLS validation, sandboxing, permission checks, safe failure, and no unsolicited product telemetry.

Parity means behavior, not a line-for-line translation or preservation of Gecko's internal architecture. A simpler Rust design is preferred when tests prove equivalent behavior. Security fixes take precedence over reproducing an insecure historical behavior.

Wild Buzzard has its own name, artwork, application IDs, profile paths, defaults, and visible identity. Do not copy Firefox trademarks or imply Mozilla affiliation.

This is a long-running compatibility program. A component is not complete merely because it compiles, renders one page, or passes a smoke test. Claims of parity require recorded conformance and regression evidence.

## Meaning of "implemented in Rust"

All new first-party runtime and product code must be Rust. WebIDL, schemas, localization resources, shaders, test data, and generated files are allowed.

Operating-system APIs and audited third-party native libraries may be reached through narrow Rust FFI modules when unavoidable. Every native boundary must be documented, tested, and assigned an owner. Transitional native dependencies must not quietly become permanent architecture.

Do not add new first-party C or C++ implementation code. Imported C/C++ headers may be retained temporarily only when required to compile an adopted Rust component, and must be recorded as migration debt.

## Firefox reference checkout

The ignored `firefox/` directory is the read-only reference implementation:

- Repository: `https://github.com/mozilla-firefox/firefox.git`
- Baseline: Firefox ESR153 branch `esr153`
- Pinned checkout: `c19b7e89270787889495688244ec6ee8e79288a1`
- State: detached `HEAD`, non-shallow, with full Git history

It is reference material, not part of Wild Buzzard and never a build input.

Agents may inspect implementation code, tests, generated interfaces, blame, and historical implementations. Useful commands include:

```sh
git -C firefox show HEAD:path/to/file
git -C firefox log --follow -- path/to/file
git -C firefox blame path/to/file
git -C firefox log -S'symbol' -- path/to/subsystem
git -C firefox show <historical-commit>:path/to/file
```

Reference rules:

- Never edit, format, commit, fetch into, or generate files inside `firefox/`.
- Never create a Cargo path dependency, symlink, include path, build-script input, test fixture lookup, or runtime lookup pointing into `firefox/`.
- Wild Buzzard must build and test when `firefox/` is absent.
- Search narrowly by subsystem. The reference contains hundreds of thousands of files.
- Before porting behavior, inspect both the implementation and its relevant tests.
- Use full history to understand invariants, regressions, and removed approaches rather than translating only the current file.
- Preserve copyright, license, and third-party notices on imported or derived code.
- Never copy service credentials, endpoint keys, Firefox artwork, branded defaults, or proprietary binary material.

The pinned ESR commit is a reproducible behavioral baseline, not a security freeze. The orchestrator must track later security and standards changes separately.

## Repository layout

Preserve Firefox's broad subsystem hierarchy when practical so paths remain easy to compare:

```text
browser/       Wild Buzzard browser shell and product behavior
toolkit/       provider-neutral browser-level services
devtools/      developer tools and protocols
accessible/    accessibility tree and operating-system adapters
js/            Rust JavaScript and WebAssembly runtime
dom/           Web APIs, DOM, HTML, events, workers
layout/        formatting, layout, scrolling, hit testing
servo/         imported and adapted Stylo crates
gfx/           WebRender, WebGPU integration, compositor, fonts, color
image/         image decoding and animation
media/         audio/video containers, codecs, capture, playback
netwerk/       DNS, HTTP, QUIC, proxy, cache, cookies, sockets
security/      TLS, certificates, permissions, sandbox policy
storage/       origin storage, quota, databases
xpcom/         temporary Rust service abstractions during migration
ipc/           typed process boundaries
widget/        windows, surfaces, input, clipboard, platform events
intl/          Unicode, encoding, locale, segmentation
third_party/   pinned external source, licenses, and notices
testing/       conformance, integration, reftest, and browser harnesses
docs/          architecture, parity evidence, provenance, handoffs
firefox/       ignored read-only ESR reference; never a dependency
```

Keep imported Mozilla- or Servo-authored Rust components at their Firefox-relative paths unless the orchestrator approves a move. Do not preserve a path at the cost of cyclic crate dependencies; explicit Rust interfaces and an acyclic dependency graph take precedence.

## Agent topology

There is one main orchestrator and six logical component owners. They are durable workstreams, not a requirement that all six run simultaneously. The orchestrator uses only the concurrency the environment can safely support and may create short-lived research or test agents with narrower scopes.

### Main orchestrator

The main agent owns integration and is the only default writer for:

- Root `Cargo.toml`, `Cargo.lock`, toolchain files, CI, release configuration, and this `AGENTS.md`.
- Cross-subsystem interface crates and architecture decisions.
- `third_party/` imports, provenance, license review, and upstream refreshes.
- Parity matrices, milestones, and product-wide status.
- Full builds, integrated test gates, conflict resolution, and release acceptance.
- Privacy, prohibited-endpoint, branding, dependency, and native-code audits.

The orchestrator must give every delegated task:

- a task identifier and observable outcome;
- one owner and exact writable paths;
- relevant Firefox implementation, history, and test paths;
- dependencies and the public interface contract;
- required tests and acceptance criteria;
- explicit exclusions and forbidden shortcuts.

Prefer thin vertical slices that integrate frequently over six isolated ports that cannot run together.

### Agent 1: foundation, process model, and platform

Default ownership:

- `xpcom/`, `ipc/`, `widget/`, `memory/`, and `mozglue/`
- `intl/`, preferences, profiles, clocks, task scheduling, and event loops
- Process launch, shutdown, sandbox plumbing, windows, surfaces, input, clipboard, and drag-and-drop

Responsibilities:

- Replace XPCOM ownership patterns with Rust traits, typed handles, `Arc`/`Weak`, explicit lifetimes, and explicit errors.
- Implement parent, content, GPU, network, and utility process roles.
- Own typed and versioned IPC, validation, cancellation, backpressure, crash isolation, and deterministic shutdown.
- Provide platform-neutral task, timer, preference, localization, profile, and event primitives.
- Keep `unsafe` and operating-system FFI in small reviewed modules.

Imported `modules/libpref/parser` belongs to this workstream. Firefox's `xpcom/rust` and `ipc/rust` are reference material because they primarily wrap C++ Gecko contracts.

### Agent 2: JavaScript and WebAssembly runtime

Default ownership: `js/`.

Responsibilities:

- ECMAScript parsing, bytecode or IR, values, objects, realms, modules, promises, jobs, exceptions, debugger hooks, garbage collection, and optimization.
- WebAssembly decoding, validation, compilation, instantiation, memories, tables, reference types, component-model work, and GC integration.
- A stable host API used by generated DOM bindings. The runtime must not import concrete DOM implementations.
- Test262, WebAssembly specification tests, differential tests, fuzzing, and applicable SpiderMonkey regression behavior.

SpiderMonkey is a JavaScript engine, not a Java engine. Firefox implements WebAssembly inside SpiderMonkey, and the Wasm runtime shares its GC, values, promises, JIT machinery, and host rooting rules. Treat JS, Wasm, GC, and host rooting as one architectural boundary even when separate agents research subparts.

Firefox's `js/src/rust` is helper/glue code, not a Rust implementation of SpiderMonkey. Current ESR153 uses Rabaldr and Baldr/Ion for Wasm; it has no active Cranelift backend. Historical Cranelift code is available in the full reference history, but restoring it is not considered a completed Rust runtime.

### Agent 3: Web platform, DOM, Stylo, and layout

Default ownership:

- `dom/`, `layout/`, `parser/`, and `servo/`
- Excluding media, Canvas, WebGPU, and storage backends assigned to other agents

Responsibilities:

- HTML/XML parsing, DOM trees, generated WebIDL bindings, events, navigation lifecycle, workers, timers, and Web APIs.
- CSS parsing, selectors, cascade, computed values, invalidation, layout, fragmentation, scrolling, hit testing, selection, and semantic-tree production.
- Fetch, CORS, CSP, and referrer-policy semantics above the transport boundary.
- Import and adapt Stylo rather than rewriting it.

Adopted Stylo source includes `servo/components/style`, `selectors`, `style_traits`, `style_derive`, `servo_arc`, `malloc_size_of`, and `to_shmem` support crates.

The imported Firefox Stylo snapshot is not yet standalone. Its Gecko feature depends on generated C++ bindings and its Servo feature references source absent from the Firefox import. Create a Wild Buzzard platform feature and native Rust contracts. Do not reintroduce `servo/ports/geckolib` as the final boundary.

### Agent 4: graphics, GPU, images, and media

Default ownership:

- `gfx/`, `image/`, and `media/`
- `dom/canvas/`, `dom/webgpu/`, and `dom/media/`

Responsibilities:

- WebRender integration, display lists, compositor, surfaces, color, fonts, rasterization, clips, hit testing, and screenshots.
- Canvas 2D, WebGL, WebGPU, GPU-process protocols, resource lifetimes, and device-loss recovery.
- Image decoding, animation, audio/video demuxing, decoding, playback, capture, and output.
- Deterministic reftests, screenshot comparison, GPU validation, frame pacing, and media corpus tests.

Reuse before rewrite:

- `gfx/wr` for WebRender.
- `gfx/qcms` for color management.
- Imported `third_party/rust/wgpu-*` and `naga` source as the pinned WebGPU baseline.
- Imported `mp4parse`, `audioipc2`, and Cubeb Rust layers where their native dependencies are acceptable as tracked transition boundaries.

`gfx/webrender_bindings`, `gfx/wgpu_bindings`, and `media/mp4parse-rust` are Gecko adapters, not desired final interfaces. Replace their C++/XPCOM contracts with native Rust APIs.

Only the Rust core packages listed in `docs/upstream-components.toml` are admitted from the
`gfx/wr` snapshot. SWGL, Wrench, shader-to-C++ tooling, examples, and example compositor paths are
excluded from that workspace because they contain native or Gecko-dependent implementation. Do
not enable them as a shortcut; provide Rust-native Wild Buzzard equivalents or record an approved
third-party native boundary.

### Agent 5: networking, security, and persistent storage

Default ownership:

- `netwerk/`, `security/`, and `storage/`
- Origin-storage and quota backends
- Selected provider-neutral application-services code

Responsibilities:

- DNS, sockets, proxies, HTTP/1.1, HTTP/2, HTTP/3, QUIC, cache, cookies, authentication, downloads, streams, backpressure, and cancellation.
- TLS, certificate verification, trust policy, content-security enforcement, process boundaries, and permission support.
- IndexedDB and local storage, quota, history, bookmarks, passwords, autofill, WebExtension storage, and profile migration.
- Loopback protocol servers, malformed-input testing, isolation tests, and secure failure behavior.

Reuse before rewrite:

- Imported Neqo crates for QUIC and HTTP/3.
- Imported WHATWG URL crates.
- `third_party/skv` and selected provider-neutral application-services components after their Mozilla Sync coupling is feature-gated away.

`netwerk/socket/neqo_glue` and URL/XPCOM glue are reference adapters, not final APIs.

NSS is C/C++, and `nss-rs` is only a binding. If NSS is used temporarily, isolate it behind a narrow tracked crypto interface. The target is an audited Rust-facing TLS/crypto implementation. Never weaken certificate validation, TLS, same-origin policy, CORS, CSP, sandboxing, or storage partitioning to unblock a milestone.

### Agent 6: browser product, UI, accessibility adapters, extensions, DevTools, and automation

Default ownership:

- `browser/` and product-facing `toolkit/`
- `devtools/`, `accessible/`, `extensions/`, `mobile/`, and `remote/`
- `testing/geckodriver`, `testing/webdriver`, and browser-level integration harnesses

Responsibilities:

- Rust-native windows, tabs, address bar, navigation, menus, prompts, settings, downloads, history, bookmarks, passwords, permissions, sessions, and recovery.
- A stable browser-engine facade. Browser chrome must not access private DOM, layout, network, or renderer internals.
- Operating-system accessibility adapters over the semantic tree produced by DOM/layout.
- WebExtensions, DevTools protocol/UI, remote debugging, and WebDriver.
- Wild Buzzard branding and provider-neutral defaults.

UI parity means equivalent capability, interaction, keyboard behavior, accessibility, persistence, and comparable layout. It does not mean copying Firefox artwork or trademarks.

## Dependency direction and required contracts

The intended high-level dependency direction is:

```text
browser product
      |
engine facade
      |
DOM/layout -------- JS/Wasm
   |                   |
network/storage        |
   |                   |
graphics/media --------+
      |
foundation + typed IPC + platform
```

No component may reach through another component's private implementation. Cross-owner dependencies require an orchestrator-approved public contract.

Required boundaries:

- DOM to JS: rooted handles and generated bindings; never expose unrooted GC pointers.
- JS to host: callbacks and capabilities; the runtime must not depend on concrete DOM types.
- DOM/layout to Stylo: snapshots, invalidation, computed values, and explicit ownership.
- Layout to graphics: immutable display lists, clips, hit-test data, and accessibility metadata; the renderer never owns DOM nodes.
- DOM Fetch to networking: asynchronous request/response streams with cancellation and backpressure. Networking owns transport; Web Platform owns CORS and browser semantics.
- Media, Canvas, and WebGPU DOM APIs to graphics/media: typed commands and resource handles.
- UI to engine: typed navigation, session, prompt, permission, download, history, diagnostics, and lifecycle events.
- Engine to platform: typed window, input, surface, clipboard, and accessibility events.
- Storage: origin, top-level site, private mode, and partition keys are explicit at every public boundary.
- IPC: messages are versioned and validated; never transmit process-local pointers or unchecked lengths.
- Accessibility: DOM/layout owns semantics and geometry; Agent 6 owns operating-system adapters.

Shared interface crates contain contracts and types, not component business logic. Avoid cyclic crate dependencies.

## Existing Rust import policy

Do not equate "contains `.rs` files" with "reusable Rust component." Classify every import:

1. Native reusable component: import its complete crate/workspace, tests, assets, build scripts, and licenses.
2. Reusable core plus Gecko adapter: import the core and replace the adapter at a Wild Buzzard contract.
3. Gecko/XPCOM glue written in Rust: use as reference or explicitly quarantined migration code; it is not a completed port.
4. Mechanically vendored dependency: keep pinned and unmodified or replace it with a canonical upstream workspace before development.
5. Provider-specific component: exclude unless a provider-neutral core is separated first.

For every adopted component:

- Import an exact source snapshot before adaptation.
- Keep the import mechanically reviewable; do not bulk-format it in the same change.
- Preserve licenses, notices, tests, assets, build scripts, schemas, and required generated data.
- Record source repository, commit or version, source and destination paths, license, local patches, and owning agent in `docs/upstream-components.toml`.
- Audit default features, telemetry, service endpoints, native dependencies, unsafe code, and generated Gecko bindings.
- Add a smoke test before calling the import usable.
- Do not edit `.cargo-checksum.json` or normalized Cargo vendor manifests as if they were canonical source. Move active development to a reviewed Wild Buzzard manifest or exact canonical upstream snapshot.

Current classifications:

- Independently buildable: the admitted WebRender Rust core workspace, `gfx/qcms`, `modules/libpref/parser`, and `third_party/skv`.
- Reusable but adaptation required: imported Stylo crates.
- Pinned component source awaiting canonical workspace integration: Neqo, wgpu/Naga, URL, mp4parse, audioipc/Cubeb, and authenticator imports under `third_party/rust`.
- Quarantined until provider coupling is removed: selected application-services Places, logins, autofill, and WebExtension storage code.
- Reference-only adapters: `servo/ports/geckolib`, `gfx/webrender_bindings`, `gfx/wgpu_bindings`, `netwerk/socket/neqo_glue`, most `xpcom/rust`, and `toolkit/library/rust`.
- Rewrite tracks rather than reusable Rust engines: SpiderMonkey/JavaScript/WebAssembly and NSS/TLS.

Do not copy all of Firefox's `third_party/rust`. It contains hundreds of mechanically vendored versions, many unused by Wild Buzzard. Import only adopted component source and its reviewed dependency closure, or use Cargo with a locked dependency policy.

## Product scope, privacy, and branding

Do not port or introduce:

- Firefox Accounts or Mozilla-operated Sync integration.
- Pocket, Mozilla VPN, Relay, Monitor, sponsored content, advertising, or affiliate defaults.
- Glean, FOG, Firefox Telemetry, pings, studies, Normandy, or Nimbus experiments.
- Mozilla recommendations, messaging, remote campaigns, or automatic data submission.
- Firefox crash-upload, update, blocklist, or remote-settings endpoints.
- Mozilla API keys, credentials, distribution IDs, branded search deals, or provider defaults.
- Firefox icons, wordmarks, application IDs, profile names, or claims of Mozilla affiliation.

General browser functions remain in scope:

- Local history, bookmarks, passwords, autofill, downloads, permissions, and session recovery.
- User-configurable search and proxy support.
- Local diagnostics and crash dumps without automatic upload.
- Provider-neutral update, synchronization, blocklist, and threat-protection interfaces.
- Extensions, accessibility, developer tools, and WebDriver.

Removing a provider must not remove the associated security property. Certificate, revocation, safe-browsing, extension-blocklist, and application-update behavior need provider-neutral interfaces and explicit secure defaults.

No runtime network request may occur without a user action or a documented general-browser function. Automated tests default to loopback-only networking.

Use visible Wild Buzzard branding and `wild_buzzard_*` names for new first-party crates. Do not globally replace `Mozilla` or `Firefox` inside licenses, copyrights, imported compatibility tests, historical provenance, or standards data. User-agent compatibility tokens require an explicit architecture decision.

Proprietary DRM/CDM support and patented codec distribution require separate legal and product decisions and are not implied by the parity mission.

## Orchestration workflow

1. The orchestrator selects a thin end-to-end parity slice and allocates non-overlapping write scopes.
2. The component owner identifies Firefox implementation, tests, relevant history, existing Rust, security invariants, and provider dependencies.
3. If an interface is missing, the owner proposes a contract before privately implementing both sides.
4. Import reusable Rust separately from adaptation work.
5. Port behavior in reviewable increments with tests.
6. Run component gates and report exact commands, results, and skipped coverage.
7. Hand off cross-owner work instead of editing outside the assigned scope.
8. The orchestrator integrates, updates parity evidence, runs workspace gates, and accepts or rejects the slice.

The preferred first integrated slice is:

```text
URL -> loopback HTTP -> HTML parse -> DOM -> Stylo -> layout -> display list -> WebRender
```

Then add input/navigation, a minimal Wild Buzzard window, JS/DOM bindings, storage, and broader standards support.

## Shared-workspace rules

- Inspect `git status` before editing and preserve all user and agent changes.
- The orchestrator assigns one writer per path. Workers may read any live path but write only assigned paths.
- Workers must not edit root manifests, `Cargo.lock`, CI, architecture policy, provenance, or another agent's paths without a handoff.
- Do not run concurrent dependency-resolution, workspace formatting, or lockfile-generation operations.
- Do not switch branches, commit, push, rebase, reset, clean, or rewrite history unless the user or orchestrator explicitly requests it.
- Never discard another agent's changes.
- A nested `AGENTS.md` may narrow local conventions but may not weaken reference-tree, privacy, security, licensing, parity, or ownership rules.
- Put cross-component handoffs under `docs/handoffs/` using its template.

## Build and test gates

### External build artifacts

All compilation, test, benchmark, coverage, profiling, generated corpus, and packaging artifacts
must be written outside this repository. The default Cargo configuration writes to the sibling
directory `../wildbuzzardbuilds/cargo`.

Concurrent agents must override `CARGO_TARGET_DIR` with a unique task-owned subdirectory so they do
not contend for or invalidate another agent's outputs. For example:

```sh
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-3-static-layout cargo test --workspace --locked
CARGO_TARGET_DIR=../wildbuzzardbuilds/agent-4-webrender \
  cargo test --manifest-path gfx/wr/Cargo.toml --workspace --all-features --locked
```

Non-Cargo tools must likewise receive an explicit output path below
`../wildbuzzardbuilds/<agent-or-task>/`. Do not put build directories, logs, screenshots, coverage
data, crash dumps, generated test corpora, or packaged applications in the live source tree.

Treat the shared build root as concurrent external state:

- An agent owns only the task-specific subdirectory assigned to it.
- Never delete or clean the whole `../wildbuzzardbuilds/` tree.
- Before deleting a task output, resolve and verify its exact path.
- Build outputs are disposable, must never be committed, and may not be used as source inputs.
- The repository must still build from a clean external target directory.

Until project wrappers exist, independently enabled workspaces use:

```sh
CARGO_TARGET_DIR=../wildbuzzardbuilds/check cargo fmt --all -- --check
CARGO_TARGET_DIR=../wildbuzzardbuilds/check cargo check --workspace --all-targets
CARGO_TARGET_DIR=../wildbuzzardbuilds/check cargo clippy --workspace --all-targets --all-features -- -D warnings
CARGO_TARGET_DIR=../wildbuzzardbuilds/check cargo test --workspace --locked
CARGO_TARGET_DIR=../wildbuzzardbuilds/check cargo build --workspace --release --locked
```

Snapshot imports may have a documented temporary lint exemption. New Wild Buzzard code may not. Never claim a command passed if the workspace is not bootstrapped or the command was not run.

Required test layers:

- Unit, property, and contract tests for Rust crates and public interfaces.
- Test262 for ECMAScript.
- WebAssembly specification tests.
- Web Platform Tests for DOM and Web APIs.
- CSS tests, deterministic layout tests, and reftests.
- Network protocol, cancellation, isolation, and malformed-input tests.
- Media corpus and corruption tests.
- WebDriver product interaction tests.
- Accessibility-tree and keyboard-navigation tests.
- Privacy tests rejecting prohibited endpoints and unsolicited requests.
- Fuzzing for parsers, IPC, JS/Wasm, images, media, and network protocols.
- Cross-platform smoke tests followed by Windows, macOS, and Linux parity gates.

Firefox harnesses do not have to be ported literally. Preserve each test's observable assertion and upstream path in parity evidence.

Suggested CI tiers:

- Per change: formatting, check, clippy, component tests, contract tests, and focused conformance shards.
- Nightly: full workspace, WPT, Test262, Wasm suites, reftests, WebDriver, sanitizers, Miri where applicable, and fuzz jobs.
- Release: locked cross-platform builds, UI/accessibility tests, privacy/network audit, dependency and license audit, and a published parity report.

## Definition of done

A component or parity slice is done only when:

- It is implemented in live-tree Rust and has no dependency on `firefox/`.
- Adopted code has provenance, intact licenses, and a dependency/native-code review.
- Public contracts are documented and exercised by integration tests.
- Focused tests and required enabled-workspace gates pass.
- Relevant Firefox, WPT, Test262, Wasm, reftest, or WebDriver behavior has recorded parity evidence.
- Unsupported behavior fails explicitly and safely rather than hiding behind an unrecorded skip.
- Security boundaries, cancellation, shutdown, and error paths are tested.
- New `unsafe` or native FFI is isolated and accompanied by documented `SAFETY` invariants.
- No telemetry, provider endpoint, credential, or Firefox-branded product asset was introduced.
- The parity matrix and handoffs state all remaining differences.

## Initial milestones

1. Establish the root workspace, core types, typed IPC contracts, provenance registry, test harnesses, and a headless executable.
2. Validate WebRender and qcms; adapt Stylo; establish canonical editable Neqo and wgpu workspaces.
3. Deliver the static-page vertical slice from URL through WebRender.
4. Add input, navigation, a minimal Wild Buzzard window, tabs, and address bar.
5. Integrate a minimal JS/Wasm runtime and generated DOM bindings, then grow against Test262 and WPT.
6. Add persistent storage, workers, Canvas/WebGPU, media, accessibility, extensions, DevTools, and multi-process hardening.
7. Close conformance, security, performance, and cross-platform UI gaps before claiming Firefox parity.

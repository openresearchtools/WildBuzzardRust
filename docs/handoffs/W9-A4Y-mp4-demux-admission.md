# W9-A4Y-C3/C4 mp4parse payload-logging and provenance correction handoff

Date: 2026-08-23

C3 base commit: `5739fa22359919b86a4bda4771fd6ac367592884`

C4 base commit: `8eefa01eaa43ce25ab500f60f7d71945aaad2767`

Firefox reference baseline: detached `c19b7e89270787889495688244ec6ee8e79288a1`

Target: Linux `x86_64-unknown-linux-gnu`

Disposition: shared-working-tree corrections; nothing staged, committed, or pushed

## Self-review decision

**GO for W9-A4Y-C3's payload-logging correction only.** Every active mp4parse
`debug!`/`trace!`/`warn!`/`error!` site was audited. Complete userdata and every other identified
media-controlled byte, string, vector, and aggregate metadata value were replaced by bounded
structural fields, presence flags, byte counts, or entry counts. Parser control flow and returned
metadata are unchanged. The adversarial regression verifies that all sentinels were genuinely
parsed and that neither printable nor decimal-byte forms reached the logger on successful
admission or on a later parser-failure path. The 20-test matrix, 25 focused repetitions, fresh
warning-denied Clippy including the local mp4parse dependency, warning-denied rustdoc including
dependencies, and all requested locked/offline gates pass in the Data-drive container.

**GO for W9-A4Y-C4's provenance correction only.** `docs/upstream-components.toml` now classifies
the exact imported and locally adapted Cargo-normalized mp4parse 0.17.0 tree as `pinned-source`
with `editable = false`, records both C1 and C3, and records adapted-tree SHA-256
`eb98c147d2b0c545595994a7f906c07435559494185579fa3f5786edc6b458ea`. The adjacent patch record
states the same imported/adapted status without copying the digest into the tree it helps hash.
This closes the registry classification/hash inconsistency only. Establishing a reviewed canonical
editable upstream workspace remains open.

**NO-GO for untrusted browser-content admission, sample demux, decoding, playback, DOM/browser
integration, or a Firefox/YouTube parity claim.** `UNTRUSTED_CONTENT_ADMISSION_ENABLED` remains a
literal compile-time `false`. mp4parse still has parser-owned deep sample-table allocations and no
caller-supplied aggregate operation/allocation budget.

## C4 provenance classification and tree identity

The parser core remains the exact upstream revision
`b693c7e4a91d1d5a391c0404d18b6fc94714894e` represented by Firefox's Cargo-normalized vendor
snapshot, plus the documented C1 and C3 payload-redaction adaptations. The retained
`.cargo-checksum.json`, missing `Cargo.toml.orig`, and local source edits mean this tree is neither
an unmodified upstream snapshot nor a reviewed canonical editable workspace. Under the existing
registry schema it therefore remains `pinned-source`, `editable = false`, with its adapted tree
identified separately.

The authoritative recipe runs from `third_party/rust/mp4parse`:

```text
find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum
```

It hashes every file in that directory, including the normalized manifest, license, source, tests,
fixtures, and `WILDBUZZARD_PATCHES.md`. After the C4 patch-record clarification it produces
`eb98c147d2b0c545595994a7f906c07435559494185579fa3f5786edc6b458ea`.

## C3 correction and active-log audit

The original C1 patch had already removed complete AVC/H.263/HEVC/AMR configuration,
protection-structure, PSSH, and `stsd` formatting. C3 closes the independently observed remaining
userdata leak and the same flaw class across every active log invocation:

- userdata now reports only metadata presence and cover-art entry count;
- skipped box headers report type, framing sizes, and UUID presence without UUID bytes;
- AVIF item references, property associations, properties, and conflict diagnostics report only
  types, IDs, and counts, never auxiliary strings, raw AV1 configuration, ICC bytes, or vectors;
- file type, edit list, and all sample-table diagnostics report bounded scalars/counts rather than
  complete lists;
- PSSH, codec configurations, ICC profiles, and similar byte payloads report byte/entry counts;
- HDR/colour and other fixed metadata structures no longer use complete aggregate formatting.

Remaining formatted values in active log calls are bounded structural enums/FourCCs, IDs, sizes,
offsets, counts, fixed scalars, and parser status classes. No active log call formats a complete
media-controlled byte/string/vector/aggregate metadata object.

## C3 adversarial regression

The in-memory fixture contains unique title (`©nam`), description text (`desc`), podcast URL
(`purl`), owner (`ownr`), and cover-art (`covr`) sentinels alongside unique AVC SPS/PPS, PSSH system
ID/KID/data, track KID, and constant-IV sentinels. A direct mp4parse read inside the successful
case proves each userdata value and cover-art entry was parsed exactly. The same userdata precedes
a track whose later `stts` parse is intentionally truncated.

Captured `Trace`-through-`Error` output excludes both the printable and comma-separated decimal
form of every sentinel on both paths. Both logs must still contain the bounded userdata summary,
PSSH KID count, and sample-description count. One focused run passed after the only compile-time
iteration (a method-reference type mismatch, corrected before authoritative gates), then 25
locked/offline exact-filter repetitions passed serially.

## Retained bounded-admission outcome

The independent `media/` workspace owns the MPL-2.0 `wild_buzzard_mp4` Rust crate. Its
`admit_complete_mp4` API accepts one caller-owned complete byte slice under a 64 MiB limit,
performs bounded structural preflight, invokes exact local mp4parse 0.17.0, cross-checks parser
output, and atomically publishes only Wild Buzzard-owned provider-neutral initialization metadata
and explicit resource accounting.

The crate performs no file, URL, network, DOM, browser-shell, decoder, playback, telemetry,
endpoint, or ambient-capability operation. No mp4parse type crosses the public API.

## Review-finding status after C4

1. **Complete AAC ES hierarchy and exact DSI agreement — closed.** Before mp4parse runs, the
   wrapper frames every expandable-class length using checked arithmetic, accepts the legal
   one-to-four-octet forms (including mp4parse's fixed four-octet form), rejects unterminated or
   overflowing lengths, and requires exact parent-end consumption. An audio
   `esds` must contain exactly one root ES descriptor and exactly one nested DecoderConfig. AAC and
   xHE-AAC DecoderConfigs must contain exactly one nonempty DecoderSpecificInfo; MP3's applicable
   hierarchy contains no DSI. A single bounded SLConfig sibling is permitted only after the
   DecoderConfig. DecoderConfig audio stream type and reserved-bit framing are validated.
   Parser-incompatible optional ES flag combinations and every unexpected nested tag fail closed.
   Payload-identical second ES, DecoderConfig, or DSI records have distinct typed
   duplicate errors; differing second records have distinct conflict errors. Preflight retains
   only the exact DSI source range, then compares those bytes byte-for-byte with mp4parse's
   `decoder_specific_data` before validation or publication. A truncated DSI which mp4parse
   partially interprets but does not preserve is rejected as `ParserDecoderSpecificMismatch`.
   Clear and protected fixtures cover every duplicate/conflict/malformed class, an inexact trailing
   end, missing ES/DecoderConfig/DSI, exact publication, and recovery.

2. **Global PSSH versus per-description protection — closed.** `Mp4Initialization::protection_present`
   is derived only by OR-ing admitted sample descriptions whose own protected entry has one
   coherent `sinf`; global movie PSSH boxes never set it. Global PSSH count/bytes remain explicitly
   named accounting fields. Tests prove clear AVC plus PSSH remains unprotected, protected AVC
   without PSSH is protected, and protected AVC plus PSSH is protected.

3. **Hidden-extra `stsd` allocation edge — closed.** After checking the declared 1-through-16
   policy, a first pass uses only checked offsets, a counter, and nonallocating box framing to scan
   every sample entry to the exact `stsd` end. Declared and framed counts must agree before any
   description-vector reserve or population. Only then does the wrapper `try_reserve_exact` and
   perform the bounded semantic pass. mp4parse is invoked only after the complete source preflight
   succeeds. Parser-published count is checked independently after parsing. Over-declared,
   under-declared/hidden-extra, zero, oversized, truncated, valid, and recovery cases pass.

4. **Provenance classification/hash inconsistency — closed by C4.** The registry now describes
   the exact tree as an imported, locally adapted Cargo-normalized `pinned-source` snapshot with
   `editable = false`, lists C1 and C3, and records the current complete-tree digest
   `eb98c147d2b0c545595994a7f906c07435559494185579fa3f5786edc6b458ea`. Establishing a reviewed
   canonical editable upstream workspace remains open and was not attempted. C4 changed no
   manifest, lockfile, source, test, media crate, or inactive adjacent C API snapshot.

5. **Payload-log correction evidence — closed.** The complete active-log audit, adversarial
   success/later-failure regression, 25 focused repetitions, full locked/offline matrix,
   warning-denied Clippy and rustdoc, release build, and whitespace/scope checks pass. Every final
   Cargo gate masked `firefox/` with an empty read-only directory.

## Fixed limits and resource accounting

Pre-parser limits remain:

- complete source: 64 MiB;
- top-level boxes: 4,096;
- direct `moov` children: 4,096;
- all logically inspected boxes below the top level: 8,192;
- tracks: 32;
- compatible brands beyond the major brand: 63;
- sample descriptions per track: 16;
- handler name: 1,024 bytes and NUL-terminated;
- one configuration-box payload: 1 MiB;
- aggregate declared configuration payloads: 4 MiB;
- AVC parameter sets per `avcC`: 64;
- one AVC parameter set: 64 KiB;
- PSSH boxes: 16;
- one PSSH payload: 1 MiB;
- aggregate PSSH payload: 4 MiB;
- KIDs in one version-1 PSSH: 64;
- audio channels: 1 through 64;
- integral audio sample rate: 1 through 768,000 Hz.

Successful admission accounts source bytes, top-level boxes, direct movie children, logical nested
boxes, brands, tracks, sample descriptions, declared and published configuration bytes, and global
PSSH count/bytes. Duration conversion is checked. Track IDs are nonzero and unique; movie and track
timescales are nonzero; admitted video dimensions are nonzero. Errors are typed and redacted.

The first `stsd` framing pass deliberately does not increment `nested_boxes` a second time: the
accounting field measures logical source boxes, while the implementation performs two physical
header reads to prevent allocation before cardinality agreement.

## Changed paths and SHA-256

The source and media hashes below are the frozen C3 inputs. C4 changed no source, manifest,
lockfile, or test. The patch-record and registry rows are superseded by the C4 provenance record
immediately below the table.

| Path | SHA-256 |
| --- | --- |
| `media/Cargo.toml` | `99b123508b1c0d4b817f8aa02c56997a7e24b7a6d99c2daf5782f390e20e7e5f` |
| `media/Cargo.lock` | `fffa5ad560f39b33e47751e60e53536351ffc4a92de611deeba82e8225e6d1ff` |
| `media/rust/wild_buzzard_mp4/Cargo.toml` | `f45cd21ad3c8065cd80f8aea7df9e10061d1ed3990c0f54d8881b63d2d586fd5` |
| `media/rust/wild_buzzard_mp4/src/lib.rs` | `135b273b4864aa2a7cddda6114588900d40720b2c66d60816f3ee89e07ef4fd1` |
| `media/rust/wild_buzzard_mp4/tests/admission.rs` | `f7d8d0f9d88a3e208468f6f08ad59ecd154eb0ace91865ded5665ba6e62a89b4` |
| `third_party/rust/mp4parse/src/lib.rs` | `f28038dc16189fb359d045ae3cda3c1fc6382e2390bf8ec094fa9d8d34307c48` |
| `third_party/rust/mp4parse/src/tests.rs` (unchanged by C3) | `fe513bc0ad707a6fbd0c10537dcd0236bb650801f4776d1a39756e7a2e9bf4fa` |
| `third_party/rust/mp4parse/WILDBUZZARD_PATCHES.md` | `06792b02ccb9d0f9fbe16065a1c24d9196035b3dccac1d7c8af77570bb8a9723` |
| `docs/upstream-components.toml` | `c8cdb3f511464fe9ea3912a018dd5160f0f627f515138ab726675c5567517a01` |

C4's authoritative adapted mp4parse tree SHA-256 is
`eb98c147d2b0c545595994a7f906c07435559494185579fa3f5786edc6b458ea`. Current individual hashes
for the three C4 documentation paths are reported after their final edit because this handoff
cannot contain its own stable digest. No root manifest/lockfile, Firefox reference file,
browser/DOM/network source, mp4parse source/test, media crate, or unrelated shared-working-tree
path was edited by C4.

## Parser provenance and dependency audit

mp4parse remains version 0.17.0 from upstream revision
`b693c7e4a91d1d5a391c0404d18b6fc94714894e`, MPL-2.0. Its locally redacted `src/lib.rs` SHA-256 is
listed above; the unchanged license SHA-256 is
`fab3dd6bdab226f1c08630b1dd917e11fcb4ec5e1e020e2c16f83a0a13863e85`; and the unchanged normalized
manifest SHA-256 is `dac1fff4cb5ebbad3ef52cb3d7db5dba012ad599759df521ff98bb95c491110d`.

The normalized vendor `.cargo-checksum.json` was not changed. Cargo consumes the locally patched
parser as a local path dependency. The registry now truthfully classifies that exact imported and
adapted tree as `pinned-source`, `editable = false`; replacing it with a reviewed canonical
editable upstream workspace remains open. Any future source refresh must carry the adjacent patch
record, update the complete-tree digest, and rerun the payload-sentinel logger regression.

The first-party manifest selects exact local mp4parse 0.17.0 with defaults disabled. Its only
direct dev dependency is exact `log` 0.4.34 with defaults disabled. `mp4parse_capi`, `3gpp`, `mp4v`,
`meta-xml`, `missing-pixi-permitted`, and `unstable-api` are disabled. The normal enabled graph has
14 distinct packages and 15 occurrences because `cfg-if` occurs twice. Locked metadata contains
21 packages/resolve nodes; all 21 declare a license, none declares Cargo `links`, and no Git source,
unexpected local path, or Firefox path occurs.

Scans found no first-party unsafe implementation, FFI/native link, filesystem/network/process
capability, URL stack, WASI, ffmpeg, GStreamer, telemetry, Mozilla endpoint, or site-specific code.
The enabled mp4parse source has no unsafe or native boundary. The dependency payload-logging test
captures and excludes unique title, description, podcast URL, owner, cover-art, AVC, PSSH, KID,
and IV sentinel sequences on success and later failure.

## Read-only reference inspected

C2 re-read mp4parse's `find_descriptor`, `read_es_descriptor`, `read_dc_descriptor`,
`read_ds_descriptor`, and `read_esds` implementation, plus public/internal ESDS tests. In
particular, the parser's fixed four-octet expandable-length fixtures were used to prevent an
over-strict wrapper policy. `Cargo.toml.orig`, README/license material, the adjacent patch record,
and the canonical provenance registry were also rechecked.

The earlier W9-A4Y/C1 reference audit remains applicable: detached Firefox ESR153
`MP4Metadata.cpp`, `MP4Demuxer.cpp`, `TestMP4Demuxer.cpp`, the `mp4_demuxer/` corpus inventory, and
relevant history were inspected read-only. Firefox was not fetched, edited, built, used as a
fixture, or made a dependency. C3 performed no Firefox inspection or UI/live-site work. Every
final C3 gate replaced `/workspace/firefox` with an empty read-only task directory.

## Deterministic test evidence

The final integration matrix has 20 deterministic in-memory tests: 20 passed, 0 failed. Existing
coverage retains exact AAC hierarchy/DSI agreement, clear/protected failure and recovery,
global-PSSH/per-description-protection separation, exact `stsd` framing, protection coherence,
AVC structure, disabled families, track hierarchy, time/identity/resource limits, and parser
failure recovery. C3 expands the payload-log test with five parsed userdata sentinels and bounded
diagnostic assertions. No external media fixture, decoder, platform codec, network, or Firefox
checkout is used.

The exact payload-log filter passed with 1 test and 19 filtered. It then passed 25 of 25 serial
locked/offline repetitions. The retained non-authoritative initial log records one compile-time
method-reference mismatch discovered before the gates; replacing it with the equivalent closure
changed no parser behavior, and every subsequent run passed.

## Reproducible environment, commands, and logs

Podman graph root:
`/run/media/user/Data/Repositories/wildbuzzardbuilds/podman/storage`

Podman run root: `/run/user/1000/wildbuzzard-podman`

Image: `localhost/wildbuzzard-rust-tests:1.90-trixie-tools`

Image ID: `2bd2b60e38453b22d4d13f8d303b4dbc26de6e8c42b6322dbcee31ba2119e7c6`

Image digest: `sha256:5cb79706a1853550f400e37c712804df498b2b8621fa6faf340b9f68b0f60ea1`

Toolchain: `rustc 1.90.0 (1159e78c4 2025-09-14)`,
`cargo 1.90.0 (840b83a10 2025-07-30)`

Task root:
`/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a4y-c3-payload-logging`

The task-local Cargo home was seeded from the prior Data-drive C2 cache; C3 performed no fetch and
no network-enabled container action. Every authoritative command used `--network none`, mounted
the repository read-only, overmounted an empty task directory at `/workspace/firefox`, and placed
Cargo home, targets, rustdoc output, temporary files, and logs beneath the C3 task root:

```text
cargo fmt --manifest-path media/Cargo.toml -- --check
cargo check --manifest-path media/Cargo.toml --locked --offline --workspace --all-features --all-targets
cargo test --manifest-path media/Cargo.toml --locked --offline --workspace --all-features --test admission parser_logging_redacts_all_media_payloads_on_success_and_later_failure -- --exact --test-threads=1
cargo test --release --manifest-path media/Cargo.toml --locked --offline --workspace --all-features --test admission parser_logging_redacts_all_media_payloads_on_success_and_later_failure -- --exact --test-threads=1
cargo test --manifest-path media/Cargo.toml --locked --offline --workspace --all-features --all-targets
cargo clippy --manifest-path media/Cargo.toml --locked --offline --workspace --all-features --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --manifest-path media/Cargo.toml --locked --offline --workspace --all-features
cargo build --manifest-path media/Cargo.toml --locked --offline --workspace --all-features --all-targets --release
```

Retained compact evidence under the task `logs/` directory includes:

- `cargo-fmt-check.log`, `cargo-check.log`, `cargo-test-full.log`;
- `cargo-clippy-fresh-with-dependencies.log`, `cargo-rustdoc-with-dependencies.log`, and
  `cargo-release-build.log`;
- `focused-log-redaction-initial.log`, `focused-log-redaction-iteration2.log`,
  `focused-log-redaction-release.log`, and `focused-log-redaction-repetitions.log`;
- the final active-log audit, hashes, scope/status, container/toolchain, and whitespace logs.

C4 is documentation/provenance-only and therefore did not rerun Cargo or media tests. Its compact
schema, hash, scope, and whitespace evidence is retained under
`/run/media/user/Data/Repositories/wildbuzzardbuilds/w9-a4y-c4-provenance`.

## Residual risk and explicit exclusions

- The registry classification/hash inconsistency is closed. A reviewed canonical editable
  upstream mp4parse workspace is still absent and remains a separate provenance/integration gate.
- The 64 MiB source cap and structural preflight make work finite but do not provide a tight
  browser-grade allocation/CPU budget for parser-owned `stts`, `stsc`, `stsz`, `stco`/`co64`, edit,
  fragment, metadata, and other deep tables. Compact hostile files can still amplify parser work.
- mp4parse's duplicate-track-ID `std::collections::HashSet` is bounded to 32 entries by preflight,
  but its growth is infallible; allocation failure may abort instead of returning typed
  `OutOfMemory`.
- ES optional-field admission is deliberately narrower where mp4parse 0.17.0's cursor behavior
  cannot be made to agree exactly with a standards-framed preflight. Broader variants need a
  parser correction and new differential evidence.
- AVC validation proves `avcC` framing, limits, and NAL categories, not complete SPS/PPS semantics
  or decoder acceptance. AAC/xHE-AAC validation proves the bounded initialization structure and
  exact parser agreement, not decoding.
- Multiple `moov` merging, non-version-0 audio sample entries, noncanonical initialization order,
  and codec families not explicitly admitted remain excluded.
- The payload-redaction patch is a maintained local divergence and must be carried and regression
  tested on refresh.
- No fuzz/sanitizer corpus, process sandbox/isolation, streaming/incremental input, sample delivery,
  fragmented-media updates, MSE, EME/CDM, decoding, audio/video presentation, controls, media
  automation, browser integration, AppImage closure, or live-site parity is claimed.

The next security gate remains a browser-owned deep parser allocation/operation budget or a
resource-constrained isolated media process, plus fuzz/sanitizer evidence. Until then the literal
product-admission constant must remain false.

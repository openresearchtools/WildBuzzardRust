# Wild Buzzard Wasmtime upstream provenance

## Admission status

This directory is an **exact Wasmtime v47.0.3 superproject source snapshot plus the pinned core
WebAssembly specification suite**. It is source for a future browser-owned WebAssembly adapter; it
is not yet part of the Wild Buzzard root workspace or a claim that the browser WebAssembly API is
implemented.

The import deliberately does not recursively materialize the Component Model or WASI test-suite
gitlinks. Those features and ambient host capabilities are outside the initial browser-core build.
Their exact gitlink identities and inspected provenance are recorded below so their absence is
explicit rather than an unnoticed incomplete checkout.

## Wasmtime superproject pin

- Repository: `https://github.com/bytecodealliance/wasmtime.git`
- Selected release: `v47.0.3`
- Tag form: lightweight tag pointing directly to the release commit
- Revision: `5554cc1a651da536af2cc46c7324bdc085b162e3`
- Tree: `c48fdb3d3530ac038f149f17d9e35f0a554ec0ec`
- Author time: `2026-07-31T17:45:43+02:00`
- Commit time: `2026-07-31T15:45:43Z`
- Subject: `Release Wasmtime 47.0.3 (#14037)`
- License declared by the workspace and `wasmtime` crate: `Apache-2.0 WITH LLVM-exception`
- License text retained at `LICENSE` and in the upstream crate directories
- Upstream tree inventory: 6,862 tracked entries: 6,859 blobs and 3 gitlinks
- Upstream blob payload: 74,672,920 bytes (71.21 MiB)
- Imported superproject blobs: all 6,859, at their upstream-relative paths
- Owner: Agent 2, JavaScript and WebAssembly runtime
- Import date: `2026-08-09`
- Local source changes: none; this provenance file and the separately pinned core spec-suite
  contents are the only additions to the superproject blobs

The release commit contains a PGP signature made with key `B5690EEEBB952194`. The key was not
available in the local keyring, so Git reported `Can't check signature: No public key`. The
signature is present but **was not cryptographically verified locally**. Because `v47.0.3` is a
lightweight tag, it has no separate signed tag object.

Selection policy: pin the newest stable Wasmtime release inspected on the import date rather than a
moving branch; preserve the complete superproject source mechanically; activate only the reviewed
browser-core library feature set in an external embedding; and keep excluded upstream products and
capabilities inactive. The source snapshot therefore contains upstream code for features which the
Wild Buzzard adapter must not enable.

## Gitlink disposition

The exact root `.gitmodules` file is retained. Its three gitlinks were independently fetched and
inspected before the admission decision:

| Upstream path | Repository | Revision | Tree | Commit time | Blobs / bytes | License | Live-tree disposition |
| --- | --- | --- | --- | --- | ---: | --- | --- |
| `tests/spec_testsuite` | `https://github.com/WebAssembly/testsuite` | `0dc0343c9876267d99a7577ed4fc2289406a7869` | `c1c4b8d1bdc915a5a8dfe413a83e600cdfb0f9f7` | `2026-05-27T15:57:20-05:00` | 296 / 12,255,079 | Apache-2.0, retained as `tests/spec_testsuite/LICENSE` | Materialized exactly; required core Wasm conformance input |
| `tests/component-model` | `https://github.com/WebAssembly/component-model` | `87d36cef87c1a38338a3a42ccbf423a5ed3e935d` | `7e7c22dd02cbe7394d484d3745eb40bbf35fa680` | `2026-06-16T14:24:00-05:00` | 104 / 1,981,117 | Apache-2.0 (`LICENSE` and `LICENSE-APACHE`) | Content intentionally not materialized; Component Model excluded from the initial product feature set |
| `tests/wasi-testsuite` | `https://github.com/WebAssembly/wasi-testsuite.git` | `6345da2237c562d9a94e281332c653b5528fdd52` | `e7c6e162cd5e2c06c09bbe101aeb554df59b671c` | `2026-06-17T16:43:40Z` | 573 / 221,101,089 | Apache-2.0 | Content intentionally not materialized; WASI and its 210.86 MiB test payload excluded from browser content |

All three inspected gitlink repositories had no nested gitlinks and no Git LFS pointer files at
their pinned revisions. Only the 296 core spec-suite blobs are present below `tests/` in this
snapshot. No nested `.git` directory or file was copied.

`crates/wizer/.gitmodules` is also an exact superproject blob. It describes
`benches/uap-bench/uap-core`, but the pinned Wasmtime tree has no gitlink at that path. Wild Buzzard
does not invent a revision or materialize that unpinned repository.

## Mechanical import and equality proof

An external partial clone was detached at the pinned revision under
`/home/user/Documents/wildbuzzardbuilds/w2-a2x-wasmtime-import/upstream`. Blobs were exported with
`git archive`; no checkout metadata was copied into this repository. The core spec suite was fetched
independently at its gitlink revision and exported into `tests/spec_testsuite` by the same method.

Archive SHA-256 values:

- Wasmtime superproject archive: `c882ec0d54d99b1613854ab0608932f86c635a6f6c2cd43ce84817fb51483045`
- Core spec-suite archive: `656abd7095e4e869b462b7262440b5d7b25a8997999ea50477a5fd1d5e952430`

Verification used an external Git index to hash the imported bytes with their Git modes. It proved:

- 6,859 of 6,859 superproject blobs have the exact upstream mode, blob ID, and relative path.
- 296 of 296 core spec-suite blobs have the exact pinned submodule mode, blob ID, and relative path
  below `tests/spec_testsuite/`.
- The superproject expected/imported NUL-delimited manifests both have SHA-256
  `690f42927015239724b3e241ef1ee7241c91483f7428b7e01e11c1773175cd9a`.
- The spec-suite expected/imported manifests both have SHA-256
  `c85fe0eb9ac7484a2bd775071ddcf23da929b12ae390af74a5ea8f07d3c5fdfb`.
- This file is the sole Wild Buzzard-authored file in the directory.
- There are 7,156 live source/provenance files after this note: 6,859 superproject blobs, 296
  spec-suite blobs, and 1 provenance note.
- No Git LFS pointer signature occurs in the imported files. The root `.gitattributes` contains no
  LFS filter.
- No nested `.git` metadata, in-repository `target` directory, Git dependency specification, or
  Cargo lockfile `git+` source occurs.
- Cargo manifests and build scripts contain no path reference to the ignored `firefox/` checkout.

These rules define refresh verification: compare all superproject blobs while treating its three
gitlinks as recorded identities; separately compare every materialized spec-suite blob to its pinned
tree; require the component and WASI gitlink contents to remain absent unless separately admitted;
then allow only this provenance note as an additional file.

## Initial browser-core feature boundary

The reviewed top-level dependency selection is:

```toml
wasmtime = { default-features = false, features = [
    "std",
    "runtime",
    "cranelift",
    "gc",
    "gc-drc",
    "threads",
] }
```

This selection does not activate Wasmtime's top-level `wasi`, WASI HTTP, CLI, WAT, Winch, component
model, cache, async, stack switching, profiling, coredump, debug built-ins, pooling allocator, or
ambient host-capability features. Wild Buzzard must continue to enforce that boundary in its own
adapter; the exact upstream snapshot is not itself a product feature selection.

Two upstream implementation details must not be mistaken for product exposure:

1. `wasmtime-internal-cranelift` unconditionally asks its private `wasmtime-environ` dependency to
   compile general translation support whose feature names include `component-model`, `gc-copying`,
   `gc-null`, `stack-switching`, and `threads`. The public `wasmtime/component-model` and
   `wasmtime/stack-switching` APIs remain disabled by the selected top-level feature set. A future
   adapter must still reject unadmitted proposals at configuration and conformance boundaries.
2. Pulley crates are transitive compiler/runtime infrastructure in this graph, but on supported
   Linux x86-64 the inspected build selects the native Cranelift backend. Winch is not in the
   closure.

The selected Linux build compiles `crates/wasmtime/src/runtime/vm/helpers.c` through the `cc` crate
for the upstream runtime/unwind helper boundary. This is an imported native boundary, not new
first-party C code; it requires explicit AppImage, signal, unwind, and hardening review before
product admission. The disabled fiber and JIT-debug crates are not in the selected 23-package tree.

## Selected in-tree package closure

`cargo tree`, filtered to normal and build dependencies for `x86_64-unknown-linux-gnu` and the exact
features above, reaches these 23 packages from the Wasmtime tree:

| Package | Version | Upstream path |
| --- | --- | --- |
| `cranelift-assembler-x64` | 0.134.3 | `cranelift/assembler-x64` |
| `cranelift-assembler-x64-meta` | 0.134.3 | `cranelift/assembler-x64/meta` |
| `cranelift-bforest` | 0.134.3 | `cranelift/bforest` |
| `cranelift-bitset` | 0.134.3 | `cranelift/bitset` |
| `cranelift-codegen` | 0.134.3 | `cranelift/codegen` |
| `cranelift-codegen-meta` | 0.134.3 | `cranelift/codegen/meta` |
| `cranelift-codegen-shared` | 0.134.3 | `cranelift/codegen/shared` |
| `cranelift-control` | 0.134.3 | `cranelift/control` |
| `cranelift-entity` | 0.134.3 | `cranelift/entity` |
| `cranelift-frontend` | 0.134.3 | `cranelift/frontend` |
| `cranelift-isle` | 0.134.3 | `cranelift/isle/isle` |
| `cranelift-native` | 0.134.3 | `cranelift/native` |
| `cranelift-srcgen` | 0.134.3 | `cranelift/srcgen` |
| `pulley-interpreter` | 47.0.3 | `pulley` |
| `pulley-macros` | 47.0.3 | `pulley/macros` |
| `wasmtime` | 47.0.3 | `crates/wasmtime` |
| `wasmtime-environ` | 47.0.3 | `crates/environ` |
| `wasmtime-internal-component-util` | 47.0.3 | `crates/component-util` |
| `wasmtime-internal-core` | 47.0.3 | `crates/core` |
| `wasmtime-internal-cranelift` | 47.0.3 | `crates/cranelift` |
| `wasmtime-internal-jit-icache-coherence` | 47.0.3 | `crates/jit-icache-coherence` |
| `wasmtime-internal-unwinder` | 47.0.3 | `crates/unwinder` |
| `wasmtime-internal-versioned-export-macros` | 47.0.3 | `crates/versioned-export-macros` |

All declare `Apache-2.0 WITH LLVM-exception` in their package metadata.

## Selected external registry closure

The same selected Linux tree reaches 59 registry packages from the pinned upstream lock. No
registry source was copied into this repository. Every package declared license metadata in the
permissive MIT, Apache-2.0, Apache-2.0-with-LLVM-exception, Zlib, Unlicense, or Unicode-3.0 families,
but that metadata check is not a substitute for the separate exact-source, checksum, license-file,
unsafe/native-code, and redistribution review required before vendoring.

```text
allocator-api2 0.2.20
anyhow 1.0.103
arbitrary 1.4.2
async-trait 0.1.89
bitflags 2.11.1
block-buffer 0.10.2
bumpalo 3.20.2
cc 1.2.41
cfg-if 1.0.0
cobs 0.3.0
cpufeatures 0.2.7
crc32fast 1.3.2
crypto-common 0.1.6
digest 0.10.7
either 1.13.0
equivalent 1.0.1
find-msvc-tools 0.1.4
fnv 1.0.7
foldhash 0.2.0
generic-array 0.14.5
gimli 0.33.0
hashbrown 0.16.1
hashbrown 0.17.0
heck 0.5.0
indexmap 2.14.0
itertools 0.14.0
leb128fmt 0.1.0
libc 0.2.185
libm 0.2.16
linux-raw-sys 0.12.1
log 0.4.28
memchr 2.7.6
memfd 0.6.5
object 0.39.0
once_cell 1.19.0
postcard 1.1.3
proc-macro2 1.0.101
quote 1.0.41
regalloc2 0.15.1
rustc-hash 2.1.1
rustix 1.1.4
semver 1.0.27
serde 1.0.228
serde_core 1.0.228
serde_derive 1.0.228
sha2 0.10.2
shlex 1.3.0
smallvec 1.15.1
syn 2.0.106
target-lexicon 0.13.5
termcolor 1.4.1
thiserror 2.0.17
thiserror-impl 2.0.17
typenum 1.15.0
unicode-ident 1.0.24
version_check 0.9.4
wasm-encoder 0.252.0
wasmparser 0.252.0
wasmprinter 0.252.0
```

## External validation

All generated metadata, adapter source, lockfile adaptation, and target outputs remained under
`/home/user/Documents/wildbuzzardbuilds/w2-a2x-wasmtime-import/`. The adapter depended on the live
`js/wasmtime/crates/wasmtime` path with exactly the six selected features. Its lockfile was seeded
from this snapshot's upstream `Cargo.lock`, after which Cargo removed unrelated workspace packages
and added only the external adapter root; the selected dependency versions above remained pinned.

Validation environment: `rustc 1.96.0 (ac68faa20 2026-05-25)`, Cargo 1.96.0, target
`x86_64-unknown-linux-gnu`.

Results:

- `cargo metadata --locked --format-version 1 --filter-platform x86_64-unknown-linux-gnu` passed;
  the selected graph contains no `git+` package source.
- `cargo tree -p wild-buzzard-wasmtime-import-check --target x86_64-unknown-linux-gnu --edges normal,build --locked`
  resolved 23 Wasmtime-tree packages and 59 registry packages, plus the adapter root.
- `cargo check --locked --target x86_64-unknown-linux-gnu` passed with all target output in the
  external adapter directory. The initial selected-closure compilation took 23.71 seconds; the
  final explicit locked rerun used those external artifacts and finished in 0.04 seconds.
- `cargo run --locked --target x86_64-unknown-linux-gnu` compiled and instantiated an empty binary
  WebAssembly module successfully; it did not use WAT or WASI.

These are import and embedding smoke checks, not WebAssembly conformance, sandbox, resource-limit,
collector-integration, or browser-API acceptance.

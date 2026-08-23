# Wild Buzzard local patches to mp4parse 0.17.0

Upstream: <https://github.com/mozilla/mp4parse-rust>

Pinned upstream revision: `b693c7e4a91d1d5a391c0404d18b6fc94714894e`

License: MPL-2.0; the upstream `LICENSE` and source headers remain unchanged.

## Provenance classification

This directory is the exact imported, locally adapted Cargo-normalized mp4parse 0.17.0 snapshot
tracked as `pinned-source` in `docs/upstream-components.toml`. It is not a reviewed canonical
editable upstream workspace: the normalized `.cargo-checksum.json` is retained,
`Cargo.toml.orig` is absent, and `editable = false` remains authoritative until a separate
canonical-workspace import is reviewed. The registry computes the adapted-tree digest over every
file in this directory, including this patch record; the digest is therefore recorded outside the
hashed tree rather than copied into this self-referential file.

The local source adaptations below preserve MPL-2.0 notices and parser behavior. They do not admit
untrusted browser media, decoding, playback, DOM integration, or broader media compatibility.

## W9-A4Y-C1: payload-redacted parser logging

Wild Buzzard changes only debug messages in `src/lib.rs` that previously formatted complete
decoder-configuration vectors, complete `ProtectionSchemeInfoBox` values (including KIDs and
constant IVs), complete `ProtectionSystemSpecificHeaderBox` values, or the complete `stsd` tree
which transitively contained those values. The replacements report only the parsed structure and,
for decoder configurations, its byte length. Parser control flow, accepted bytes, returned values,
and errors are unchanged.

This divergence is required because the browser-owned admission API cannot prevent a process-wide
logger installed by another component from receiving dependency debug records. Wild Buzzard does
not globally disable unrelated logging. The integration regression installs a capturing logger and
proves sentinel AVC, PSSH, KID, and IV payload bytes are absent on both successful and failed
admissions.

## W9-A4Y-C3: complete active-log payload redaction

An independent review found that the remaining `debug!("{userdata:?}")` path still serialized
complete user metadata, including strings and cover-art bytes. C3 audited every active
`debug!`, `trace!`, `warn!`, and `error!` invocation in `src/lib.rs` and `src/macros.rs`, then
removed all remaining formatting of media-controlled byte, string, vector, and aggregate metadata
values.

Active diagnostics now expose only bounded structural facts: box/property/FourCC kinds, fixed
scalar state, presence flags, and byte/entry counts. In particular, skipped UUID boxes no longer
print UUID bytes; AVIF references, associations, and item properties no longer print aggregate or
auxiliary/configuration payloads; file brands and sample/edit tables no longer print complete
vectors; userdata reports only metadata presence and cover-art entry count; and colour/profile,
codec-configuration, protection, and PSSH diagnostics report only structure and sizes. Parser
control flow, accepted input, returned metadata, and failure mapping are unchanged.

The admission regression constructs and independently verifies parsed title, description text,
podcast URL, owner, cover-art, AVC configuration, PSSH, KID, and IV sentinels. It proves both their
printable forms and byte-sequence forms are absent from captured logs on a successful admission
and on an admission that fails later while parsing a truncated sample table. It also requires the
bounded userdata, PSSH, and sample-description diagnostics to remain present.

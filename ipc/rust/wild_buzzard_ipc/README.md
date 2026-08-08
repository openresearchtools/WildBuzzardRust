# wild_buzzard_ipc

This crate defines a transport-independent Wild Buzzard IPC envelope. It performs no process launch,
socket, pipe, descriptor, or shared-memory work. The 96-byte version-one header carries magic,
header length, stable non-zero protocol/domain ID, protocol major/minor, validated flags, a
protocol-local typed message kind, bounded payload length,
correlation ID, and source/destination service identities.

Decoding validates the entire fixed header and declared payload size before invoking a typed message
decoder. The global hard payload ceiling is 16 MiB, every codec has a lower configurable ceiling,
and every message type declares its own ceiling. Payload writers check bounds before appending.
Unknown flags, request/response ambiguity, malformed identities, version mismatches, truncated data,
unexpected message kinds, unconsumed payload bytes, and trailing envelope bytes are distinct errors.

Protocol and message IDs come from the orchestrator-owned checked-in protocol registry. A message
kind is unique only within its protocol/domain. The crate has no mutable global registry; a
caller-owned `MessageHandlerRegistry` rejects cross-protocol registration and duplicate kinds.

All integer fields use little-endian encoding. Header version one is fixed as follows:

| Byte range | Field |
| --- | --- |
| `0..4` | `WBIP` magic |
| `4..6` | header length (`96`) |
| `6..8` | protocol major |
| `8..10` | protocol minor |
| `10..12` | validated envelope flags |
| `12..16` | protocol/domain ID |
| `16..20` | protocol-local message kind |
| `20..24` | payload byte length |
| `24..32` | correlation ID, zero for none |
| `32..64` | source service identity |
| `64..96` | destination service identity |

Each service identity is a `u128` service kind, `u64` namespace, `u32` slot, and non-zero `u32`
generation. A different header layout requires a new understood header length and protocol policy;
decoders never guess from trailing data.

Firefox ESR153 reference paths inspected at
`c19b7e89270787889495688244ec6ee8e79288a1`:

- `ipc/chromium/src/chrome/common/ipc_channel.h`
- `ipc/chromium/src/chrome/common/ipc_message.h`
- `ipc/chromium/src/chrome/common/ipc_message_utils.h`
- `ipc/glue/MessageLink.cpp`
- `ipc/glue/MessageChannel.h`
- `ipc/glue/ProtocolUtils.cpp`
- `ipc/glue/SerializeToBytesUtil.h`
- `ipc/ipdl/test/`

Full history around `kMaximumMessageSize` was inspected, including bug 1268616's send-side size
guard and later fuzzing-limit changes. Wild Buzzard starts with a smaller fail-closed bound and no
fuzz-only increase. Transport, Linux process roles, descriptor passing, authentication, ordering,
backpressure, and generated protocol bindings remain later integration work.

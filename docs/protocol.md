# Glass protocol

**Version 0 (draft).** Nothing is on the wire before M2. This file is the source
of truth for packet formats and control messages; `core/src/protocol.rs` must
match it. Bump the version on any change.

## Video packet header (24 bytes, big-endian)

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 1 | version | `PROTOCOL_VERSION` (0). Mismatch → drop. |
| 1 | 1 | stream_id | |
| 2 | 1 | flags | bit 0 `IDR`, bit 1 `REFINEMENT`; others 0 |
| 3 | 1 | reserved | 0 |
| 4 | 4 | frame_id | |
| 8 | 2 | packet_index | must be `< packet_count` |
| 10 | 2 | packet_count | must be `> 0` |
| 12 | 4 | fec_group | 0 until M4 defines the layout |
| 16 | 8 | capture_ts_us | host clock, microseconds |

Payload follows the header. Packets are sized to fit the MTU (about 1200–1400 bytes).

## Not yet defined
- Control channel messages (M2+)
- FEC group layout (M4)
- Input packets (M3)
- Encryption envelope, nonce and replay window (M6)

## Changelog
- v0 — draft video packet header (M0).

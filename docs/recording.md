# `.glsr` recording format

**Format version 1.** An offline stand-in for the M2 stream: one HEVC access
unit per record, with the same per-frame timing the live stream will carry.
Implemented in `core/src/recording.rs`. All integers are big-endian.

## File header (20 bytes)

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 4 | magic | `GLSR` |
| 4 | 2 | format_version | 1 |
| 6 | 1 | codec | 1 = HEVC |
| 7 | 1 | chroma | 0 = 4:2:0, 1 = 4:4:4 |
| 8 | 4 | width | pixels |
| 12 | 4 | height | pixels |
| 16 | 4 | fps | nominal capture rate (display refresh) |

## Frame record (24-byte header + payload), repeated to EOF

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 4 | frame_id | |
| 4 | 1 | flags | `protocol::FLAG_*` (bit 0 IDR) |
| 5 | 3 | reserved | 0 |
| 8 | 8 | capture_ts_us | host QPC clock, µs: when the frame reached Glass |
| 16 | 4 | encode_us | frame arrival → encoded packet out, µs (0 if unknown) |
| 20 | 4 | len | payload bytes |
| 24 | len | payload | HEVC Annex-B access unit; IDRs carry VPS/SPS/PPS in-band |

Frames are only recorded when the screen changes, so timestamps are irregular.
Players pace by `capture_ts_us`.

## Colour

NVENC converts BGRA input to YUV itself using **BT.601, limited range**, and
FFmpeg signals that in the VUI. Decoders must use the colour matrix from the
stream (VideoToolbox exposes it as a `CVPixelBuffer` attachment).

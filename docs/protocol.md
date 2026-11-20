# Glass protocol

**Version 2 (M3).** Unencrypted, LAN only, IPv4. This file is the source of
truth for packet formats and control messages; `core/src/protocol.rs`,
`core/src/packet.rs` and `core/src/input.rs` must match it. Bump the version
on any change.

All integers are big-endian.

## Connection setup

1. The viewer opens TCP to the host's port (default **47800**) and sends
   `Hello`. The host replies `Welcome` (or `Reject`).
2. The viewer binds a UDP socket and sends a **punch** datagram to the host's
   UDP port (from `Welcome`, normally the same number). The host sends video
   to the address the punch came from; it accepts punches only from the
   viewer's TCP source IP and with the right session id. The viewer repeats the
   punch every 1 s until video arrives, then every 5 s. A punch from a new
   address makes the host send a fresh IDR.
3. Video flows host → viewer over UDP; input flows viewer → host over UDP on
   the same sockets. Ping/pong, keyframe requests, pointer shape/visibility
   and Bye use the TCP connection. Either side closing TCP ends the session,
   and the host then releases any keys and buttons the viewer was holding.

The host serves one viewer at a time.

## Control messages (TCP)

Framing: `u16 length` (type + body) · `u8 type` · body. Max 65537 bytes
total; everything except CursorShape fits in 32.

| Type | Name | Direction | Body |
|---|---|---|---|
| 1 | Hello | viewer → host | `version u8`, `display_w u16`, `display_h u16` (0 = unknown; used from M7) |
| 2 | Welcome | host → viewer | `version u8`, `codec u8`, `chroma u8`, `reserved u8`, `width u16`, `height u16`, `fps u16`, `udp_port u16`, `session_id u32` |
| 3 | Reject | host → viewer | `reason u8` (1 = version mismatch, 2 = busy) |
| 4 | Ping | viewer → host | `seq u32`, `client_ts u64` |
| 5 | Pong | host → viewer | `seq u32`, `client_ts u64` (echoed), `host_ts u64` (host clock when the Ping was read) |
| 6 | KeyframeRequest | viewer → host | — |
| 7 | Bye | either | — |
| 8 | CursorShape | host → viewer | `serial u32`, `width u16`, `height u16`, `hot_x u16`, `hot_y u16`, then `width × height × 4` bytes RGBA (straight alpha, top row first). 1–96 px per side; the host scales larger pointers down. Sent when the shape changes. |
| 9 | CursorVisible | host → viewer | `visible u8` (0/1). Sent after every CursorShape and when the pointer is hidden/shown. |

`codec`: 1 = HEVC. `chroma`: 0 = 4:2:0, 1 = 4:4:4. `fps` is the host
display's refresh rate (capture only sends changed frames, so the real rate varies).

### Clock sync
The viewer pings 8 times at 50 ms after connecting, then every 500 ms. For
each Pong: `rtt = t1 - client_ts`, `offset = host_ts - (client_ts + t1) / 2`
(`host - viewer`). The viewer keeps the last 16 samples and uses the offset of
the lowest-RTT one (error ≤ ±RTT/2). Host timestamps map to the viewer clock
as `host_ts - offset`.

Clocks are each platform's media clock in µs: QPC on Windows,
`mach_absolute_time` (`CACurrentMediaTime`) on macOS.

### Keyframe requests
The viewer sends `KeyframeRequest` when it has to withhold frames until an IDR:
after a lost frame, after skipping frames on joining mid-stream, after a full
receive queue, or after a decode error. At most one every 250 ms while still
waiting. The host forces an IDR on the next captured frame, or re-encodes the
last captured frame as an IDR if the screen is idle.

## UDP datagrams

### Punch (viewer → host, 8 bytes)
| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `"GLPN"` |
| 4 | 4 | session_id (from `Welcome`) |

### Input (viewer → host, ≤ 168 bytes)

Header (24 bytes), then `key_count` bytes of held keys, then `event_count`
events of 8 bytes.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `"GLIN"` |
| 4 | 4 | session_id |
| 8 | 4 | packet_seq: +1 per datagram |
| 12 | 4 | first_event_seq: sequence number of the first event in this datagram |
| 16 | 1 | event_count (≤ 16) |
| 17 | 1 | key_count (≤ 16) |
| 18 | 1 | buttons held (bit 0 left, 1 right, 2 middle, 3 back, 4 forward) |
| 19 | 1 | flags: bit 0 = pointer position valid |
| 20 | 2 | x: 0..65535 across the video picture |
| 22 | 2 | y |

Held key: one HID usage (keyboard page 0x07) per byte.

Event: `kind u8`, `code u8`, `flag u8`, `reserved u8`, `value u32`:

| kind | Event | code | flag | value |
|---|---|---|---|---|
| 1 | Key | HID usage | 1 = down (repeat = another down), 0 = up | 0 |
| 2 | Button | button 0–4 | 1 = down | `x << 16 \| y` where it happened |
| 3 | Scroll | 0 | 0 | `dy << 16 \| dx` as i16; 120 = one wheel notch, `dy > 0` up, `dx > 0` right |

**Viewer rules.** Send a datagram on every input change and a heartbeat every
50 ms once input has started. Each datagram repeats every event from the last
300 ms (at most 16), plus the current state. Keys are physical (HID); the
Mac viewer maps ⌘/⌃/⌥ to Ctrl/Win/Alt (default) or Win/Ctrl/Alt before
sending. On losing focus, release everything.

**Host rules.** Apply each event once, in sequence order (skip sequence
numbers already applied). Then, if the datagram is newer than any seen, move to
its position and reconcile held buttons and keys with its state (release
non-modifiers first, then modifiers; press modifiers first). Record the
latest datagram that changed anything as `input_seq`.

### Video packet (host → viewer, ≤ 1400 bytes)

Header (24 bytes):

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 1 | version | `PROTOCOL_VERSION` (1). Mismatch → drop. |
| 1 | 1 | stream_id | 0 (video) |
| 2 | 1 | flags | bit 0 `IDR`, bit 1 `REFINEMENT`; others 0 |
| 3 | 1 | reserved | 0 |
| 4 | 4 | frame_id | +1 per sent frame, wraps |
| 8 | 2 | packet_index | must be `< packet_count` |
| 10 | 2 | packet_count | must be `> 0` |
| 12 | 4 | fec_group | 0 until M4 defines the layout |
| 16 | 8 | capture_ts_us | host clock, µs, when the captured frame arrived |

Payload: a chunk of the frame's **logical payload**, which is

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | encode_us: host time from capture to encoded |
| 4 | 4 | frame_len: Annex-B bytes that follow |
| 8 | 4 | input_seq: last input `packet_seq` the host had applied when this frame was captured (0 = none) |
| 12 | frame_len | HEVC access unit, Annex-B (parameter sets in-band on IDRs) |

The viewer measures **input → screen** as the time from sending input datagram
`input_seq` to presenting the first frame that carries it.

The logical payload is cut into chunks of exactly **1376** bytes (1400 − 24); only the last chunk may
be shorter. Chunk `i` goes in the packet with `packet_index = i`, so a
receiver can place packets at `i × 1376` in any order.

### Receiver rules
- A frame is complete when all `packet_count` packets have arrived.
- Frames are delivered in `frame_id` order. When a frame completes, any older
  incomplete frame is lost; packets for frames older than the last delivered
  one are ignored.
- After any loss (and at start), frames are withheld until an IDR completes.

## Not yet defined
- FEC group layout (M4)
- Encryption envelope, nonce and replay window; TLS on the control channel (M6)
- Resolution negotiation (M7)

## Changelog
- v2 — M3: input datagrams, CursorShape/CursorVisible, `input_seq` in the
  frame payload prefix (now 12 bytes), control frames up to 64 KB.
- v1 — M2: control messages over TCP, UDP punch, frame payload prefix
  (`encode_us`, `frame_len`), 1400-byte datagrams, keyframe requests, clock sync.
- v0 — draft video packet header (M0).

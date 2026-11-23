# Glass protocol

**Version 10 (M4).** FEC on video and receiver reports (v10); clipboard sharing and adaptive file speed (v8, v9); no connection loops; the host says what it is and can do; view modes and stream changes (v5) on top of v4. Paired devices only: mutual TLS 1.3 on the control
connection, every UDP datagram sealed with AES-128-GCM. LAN, IPv4. This file
is the source of truth for packet formats and control messages;
`core/src/protocol.rs`, `packet.rs`, `input.rs`, `seal.rs`, `tls.rs` and
`pairing.rs` must match it. Bump the version on any change. ADR 0010 has the
security design.

All integers are big-endian.

## Identity, TLS and pairing

Each device has a long-lived ECDSA P-256 key and a self-signed certificate,
made on first run. A device *is* the SHA-256 fingerprint of its certificate
(shown to people as the first 8 bytes, `3f2a 91c0 77de 10ab`). Each side
keeps a list of paired fingerprints with roles: **host** (I view it) and
**viewer** (it may view and control me).

Every connection starts with **TLS 1.3** (only), both sides presenting their
certificate. Verifiers check only that the peer signed the handshake with the
key of the certificate it sent; no CA, no name check, no session resumption
or tickets. Right after the handshake each side looks the peer's fingerprint
up in its list, before acting on any message:

- The **viewer** sends `Hello` only to a host it has paired with (role
  host). Otherwise it sends `Bye` and reports "not paired" (it may then pair).
- The **host** answers `Hello` from a fingerprint without the viewer role
  with `Reject(3 not paired)`. Optionally ("ask") it asks its user first and
  sends `Reject(4 denied)` on no.

**Pairing** (viewer's first message is `PairStart` instead of `Hello`):

1. viewer → `PairStart{name}`. The host shows a 6-digit PIN (OS RNG; valid
   2 min; reused for repeat requests while valid) and replies
   `PairReady{name}`, or `Reject(5 locked)`.
2. The viewer's user types the PIN. viewer → `PairPake{X}`: SPAKE2
   (Ed25519 group) side A, password = the PIN, identities
   `"glass-viewer:" ‖ viewer_fp` and `"glass-host:" ‖ host_fp` (each side
   uses the fingerprints *it saw in TLS*).
3. The host counts the attempt **before** answering, then sends
   `PairPake{Y}` (side B).
4. Both derive K. Confirmation: `mac_v = HMAC-SHA256(K, "viewer" ‖ E)`,
   `mac_h = HMAC-SHA256(K, "host" ‖ E)` with `E` = TLS exporter
   (`"EXPORTER-glass-pairing"`, empty context, 32 bytes). viewer →
   `PairConfirm{mac_v}`; the host checks it (constant time): wrong →
   `Reject(6 wrong PIN)` (or `5` if that locked it); right → stores the viewer
   (role viewer) and sends `PairConfirm{mac_h}`. The viewer checks it and
   stores the host (role host), then `Bye`. The viewer reconnects normally.
5. Limits: 3 wrong tries per PIN (then a new PIN), 10 wrong in total and
   pairing is off until the host restarts. Abandoned attempts count.

A man in the middle has to present its own certificate on each side, so the
two SPAKE2 runs use different identities (and different exporters) and fail
unless it guesses the PIN, once per attempt.

## Connection setup

1. The viewer opens TCP to the host's port (default **47800**), runs TLS and
   the fingerprint check above, and sends `Hello`. The host replies `Welcome`
   (or `Reject`). The viewer waits up to 60 s (the host may be asking).
   Both sides derive the media keys from the TLS exporter
   (`"EXPORTER-glass-media"`, context = `session_id` u32, 64 bytes): bytes
   0–31 host → viewer, 32–63 viewer → host.
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

## Control messages (TLS over TCP)

Framing (inside TLS): `u16 length` (type + body) · `u8 type` · body. Max
65537 bytes total; everything except CursorShape, ClipChunk and FileData fits in 96.

| Type | Name | Direction | Body |
|---|---|---|---|
| 1 | Hello | viewer → host | `version u8`, `display_w u16`, `display_h u16`, `refresh_hz u16` (the viewer's screen in pixels, 0 = unknown), `caps u8` (bit 0: can decode HEVC 4:4:4; bit 1: start in 4:2:0), `mode u8` (view mode wanted, below), `seen_count u8`, then `seen_count` × 8 bytes (≤ 8): the machines that currently see the viewer (first 8 bytes of their fingerprints). A host that is the viewer, or finds itself on that list, answers `Reject(7 loop)`. |
| 2 | Welcome | host → viewer | `version u8`, `codec u8`, `chroma u8`, `mode u8`, `width u16`, `height u16`, `fps u16`, `udp_port u16`, `session_id u32`, `host_os u8` (0 unknown, 1 Windows, 2 Mac), `host_caps u8` (bit 0: view modes, bit 1: 4:4:4 switchable) |
| 3 | Reject | host → viewer | `reason u8`: 1 version mismatch, 2 busy, 3 not paired, 4 denied by the host's user, 5 pairing locked (too many wrong PINs), 6 wrong or expired PIN |
| 4 | Ping | viewer → host | `seq u32`, `client_ts u64` |
| 5 | Pong | host → viewer | `seq u32`, `client_ts u64` (echoed), `host_ts u64` (host clock when the Ping was read) |
| 6 | KeyframeRequest | viewer → host | — |
| 7 | Bye | either | — |
| 8 | CursorShape | host → viewer | `serial u32`, `width u16`, `height u16`, `hot_x u16`, `hot_y u16`, then `width × height × 4` bytes RGBA (straight alpha, top row first). 1–96 px per side; the host scales larger pointers down. Sent when the shape changes. |
| 9 | CursorVisible | host → viewer | `visible u8` (0/1). Sent after every CursorShape and when the pointer is hidden/shown. |
| 10 | HostNotice | host → viewer | `code u8`: 0 none (clears), 1 input blocked by an administrator window in front (UIPI), 2 input blocked by the secure desktop (UAC prompt, lock screen). The viewer shows it even with stats hidden. |
| 11 | PairStart | viewer → host | `name` UTF-8, ≤ 63 bytes (the rest of the body) |
| 12 | PairReady | host → viewer | `name` UTF-8, ≤ 63 bytes; the host is showing a PIN |
| 13 | PairPake | either | SPAKE2 message, 33 bytes |
| 14 | PairConfirm | either | HMAC-SHA256, 32 bytes |
| 15 | SetMode | viewer → host | `mode u8`, `allow_444 u8` (0 asks for 4:2:0, e.g. after 4:4:4 failed to decode). A host that can't change ignores it. |
| 16 | StreamChange | host → viewer | `codec u8`, `chroma u8`, `mode u8`, `reserved u8`, `width u16`, `height u16`, `fps u16`: frames have these parameters from the next IDR on. |
| 17 | ClipChunk | either | `id u32`, `kind u8`, `total u32` (the item's size, 1 B–16 MB), `offset u32`, then up to 60 000 bytes of data. An item is sent as chunks of one `id`, in order (`offset` 0, 60 000, …), and is whole once `total` bytes have arrived. A chunk that doesn't continue the current item drops it. Kinds: **1** text (UTF-8, `\n` line endings), sent when copied and put on the receiver's clipboard; **4** a picture offer (`size u64`, its PNG's size); **3** a files offer (a file list, below); **2** a PNG, the answer to a `FileRequest` for a picture offer. Sent when the sender's clipboard changes, never echoing back what just arrived; interleaved with other control messages. A newer copy replaces anything not yet sent, and only the latest offer can be fetched. |
| 18 | FileData | either | `id u32` (the offer being fetched), `index u32` (the entry in its file list), `offset u64`, then up to 60 000 bytes. Each file's bytes come in order, files in list order (folders and empty files have none). The receiver drops the whole fetch on a piece that doesn't continue it. |
| 19 | FileCancel | either | `id u32`: that offer can't be fetched (the files changed or couldn't be read, or something newer was copied). The receiver drops what arrived. |
| 20 | FileRequest | either | `id u32`: something was pasted here; send what offer `id` stands for (the PNG as a kind-2 ClipChunk with that id, or the files as `FileData`), or `FileCancel` if it's gone. |
| 22 | ReceiverReport | viewer → host | `expected u32`, `missing u32`: every 0.25 s, the video data packets the viewer expected since the last report, and how many didn't arrive (rebuilt by FEC or not). The host sizes FEC from it (up fast, down slowly). |
| 21 | FileRate | viewer → host | `bytes_per_s u32`: how fast file data to the viewer may go. Sent every 0.25 s. The viewer steers it from its picture: the video frames' average network time over the last 0.25 s 4 ms over its quiet baseline, a single frame 30 ms over, a lost frame, late packets, or a round trip 10 ms over its baseline halve it; an average 2 ms over holds it; otherwise it rises by 15 % while a copy is moving (1–60 MB/s). It uses the same speed for its own file data. |

**Offers, not copies.** Pictures and files are sent only when pasted, as Remote Desktop does: the receiver puts a promise on its clipboard (Windows: delayed rendering for pictures, virtual files for files; Mac: lazy pasteboard items), and the request goes when an app asks for the data. Copying without pasting costs a few bytes.
**File list** (ClipChunk kind 3): `version u8` (1), `count u32` (≤ 10 000), then per entry `dir u8` (1 folder), `size u64`, `path_len u16`, `path` UTF-8: relative to the copy, `/`-separated, folders before what they contain. No size limit: the paste waits as long as the size needs. When fetched, the receiver writes the files under a folder of its own, made safe (no `..` or absolute paths; characters and names Windows can't store are replaced), and hands them to the paste once every byte has arrived. File data is paced by the sender (about 60 Mbps for now).

View modes (`mode`): **0** the host's own screen (scaled to fit by the viewer); **1** fit: a host display at the viewer's pixel size that looks about as big as the host's own screen (Mac: HiDPI virtual display, "looks like" viewer size ÷ 1.5); **2** fit, more space: the viewer's size at 1×; **3** fit, larger: the viewer's size at 2×. `Welcome` always describes mode 0 at 4:2:0; the host then switches (a `StreamChange` followed by an IDR) if `Hello` asked for another mode or allows 4:4:4.

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

Every datagram (both directions, all kinds) is **sealed**:

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | counter: 1, 2, 3 … per direction per session, never reused |
| 8 | n | AES-128-GCM ciphertext of the datagram below |
| 8 + n | 16 | GCM tag |

- Nonce = `iv XOR (0⁴ ‖ counter)`; AAD = `counter ‖ session_id`.
- Keys roll every 2^23 packets (epoch = `counter >> 23`):
  `secret(e+1) = HKDF-Expand(secret(e), "glass media next", 32)`,
  `key = HKDF-Expand(secret, "glass media key", 16)`,
  `iv = HKDF-Expand(secret, "glass media iv", 12)` (HKDF-SHA256, the secret
  as the PRK). Old secrets are dropped. A receiver follows up to 16 epochs
  ahead and drops packets from epochs it has left.
- Replay window: 1024 counters behind the highest accepted; duplicates and
  older packets are dropped. Failures are counted (`rejected`), never answered.
- Sealed size ≤ **1400** bytes; the plaintext inside is ≤ 1376.

The datagrams inside:

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
latest datagram whose events (or repairs) changed anything as `input_seq`;
pure pointer moves don't count (the pointer isn't in the captured picture).

### Video packet (host → viewer, ≤ 1376 bytes before sealing)

Header (24 bytes):

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 1 | version | `PROTOCOL_VERSION` (4). Mismatch → drop. |
| 1 | 1 | stream_id | 0 (video) |
| 2 | 1 | flags | bit 0 `IDR`, bit 1 `REFINEMENT`; others 0 |
| 3 | 1 | reserved | 0 |
| 4 | 4 | frame_id | +1 per sent frame, wraps |
| 8 | 2 | packet_index | must be `< packet_count` |
| 10 | 2 | packet_count | must be `> 0` |
| 12 | 1 | fec_groups | FEC: the frame's data packets are in this many interleaved groups (0 = no FEC) |
| 13 | 1 | fec_parity | parity packets per group |
| 14 | 2 | reserved | 0 |
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

The logical payload is cut into chunks of exactly **1352** bytes
(1400 − 24 sealing − 24 header); only the last chunk may be shorter. Chunk `i`
goes in the packet with `packet_index = i`, so a receiver can place packets at
`i × 1352` in any order.

### FEC (M4, v10)
- A frame has `packet_count` data packets, then `fec_groups × fec_parity`
  parity packets: `packet_index` from `packet_count` up, group `g`'s parity
  rows at `packet_count + g × fec_parity + j`.
- Data packet `i` is in group `i mod fec_groups`, so a run of consecutive
  losses is spread over the groups. Groups hold at most 64 data packets.
- Parity is systematic Reed–Solomon over GF(2^8) (polynomial `0x11D`), with
  a Cauchy matrix. Row `j` over the group's `t`-th data packet has the
  coefficient `1 / ((k + j) XOR t)`, for a group of `k` data packets. The
  data payloads count as `MAX_PAYLOAD` (1352) bytes long, the last one
  zero-padded. Parity payloads are exactly 1352 bytes.
- The receiver rebuilds a group as soon as it holds as many of the group's
  packets (data plus parity) as it has data packets. A rebuilt last packet
  takes its length from the frame's `frame_len`.
- The host sizes `fec_parity` from the viewer's `ReceiverReport`s: the
  smallest count such that a group loses more than that about 1 in 10 000
  times at 1.5 × the reported loss + 0.5 %, at least 1.

### Receiver rules
- A frame is complete when all `packet_count` data packets are there, arrived
  or rebuilt. Parity for a frame already delivered is ignored, not counted as
  late; a data packet of the frame just delivered (FEC got there first) was
  only reordered.
- Frames are delivered in `frame_id` order. When a frame completes, any older
  incomplete frame is lost; packets for frames older than the last delivered
  one are ignored.
- After any loss (and at start), frames are withheld until an IDR completes.

## Not yet defined
- Resolution negotiation (M7)

## Discovery (mDNS / DNS-SD)

Hosts advertise `_glass._tcp` (Windows: the system responder). TXT: `v`
protocol version, `fp` first 8 bytes of the fingerprint (hex), `ip`, `port`.
Advisory only: trust comes from the pinned fingerprint, never from discovery.

## Changelog
- v10 — M4: FEC on video (`fec_groups`, `fec_parity` in the header, parity packets), `ReceiverReport` (22).
- v9 — M5c: `FileRate` (21): file speed adapts to the picture instead of a fixed 60 Mbps.
- v8 — M5c clipboard: `ClipChunk` (17) carries text, and offers of pictures and files, both ways; a paste sends `FileRequest` (20), answered by the picture or `FileData` (18); `FileCancel` (19).
- v7 — M3d no-loop rule: `Hello` carries who sees the viewer; reject reason 7 = loop. Control messages may now be up to 96 bytes.
- v6 — M3d: `Welcome` ends with `host_os` and `host_caps`, so the viewer picks its key mapping and offers only what the host can do; `Hello` caps bit 1 asks to start in 4:2:0.
- v5 — M3c view modes: `Hello` carries the viewer's refresh rate, decode capabilities and wanted mode; `Welcome`'s reserved byte is the mode; `SetMode` (15) and `StreamChange` (16).
- v4 — M6 security: mutual TLS 1.3 with pinned self-signed certificates,
  PIN pairing (SPAKE2 + exporter-bound confirmation, messages 11–14), reject
  reasons 3–6, every UDP datagram AES-128-GCM sealed (24 bytes: video chunks
  now 1352 bytes), random session ids, mDNS TXT.
- v3 — HostNotice (Windows silently drops injected input into elevated
  windows and the secure desktop; the host detects it and tells the viewer).
  `input_seq` now only advances for events and repairs, not pure pointer moves.
- v2 — M3: input datagrams, CursorShape/CursorVisible, `input_seq` in the
  frame payload prefix (now 12 bytes), control frames up to 64 KB.
- v1 — M2: control messages over TCP, UDP punch, frame payload prefix
  (`encode_us`, `frame_len`), 1400-byte datagrams, keyframe requests, clock sync.
- v0 — draft video packet header (M0).

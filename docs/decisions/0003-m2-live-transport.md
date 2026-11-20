# 0003 — M2 live transport

**Status:** accepted (M2) · **Date:** 2026-11-20

## Context
M2 needs live PC → Mac video over the LAN with in-band latency numbers, no
encryption yet (M6), no FEC yet (M4). Whatever we build is reused by the
Mac host and Windows viewer in M3b.

## Decisions
1. **All networking lives in `core`**: host session, viewer client (receive
   and control threads, reassembly, clock sync, frame queue) and a replay host.
   The Swift app only decodes and draws. M3b's Windows viewer and Mac host
   reuse the same code.
2. **Plain TCP control channel now** (Hello/Welcome, ping/pong, keyframe
   requests, Bye), the same messages M6 will carry inside TLS 1.3. Nothing in
   the UDP path depends on TCP framing.
3. **UDP punch from the viewer** tells the host where to send. The host never
   sends to an address the viewer didn't send from, and the viewer's firewall
   sees replies, not unsolicited traffic. It also suits NAT later.
4. **1400-byte datagrams, fixed 1376-byte chunks.** Fixed chunk size means
   out-of-order placement needs no per-packet offset. 1400 leaves room under a
   1500 MTU for IP/UDP and the M6 AEAD tag.
5. **Per-frame info travels in packet 0's payload** (`encode_us`, `frame_len`),
   not in every header, so the header stays as defined in M0.
6. **Basic keyframe requests are pulled forward from M4.** Without FEC a single
   lost packet would otherwise corrupt the picture until the next periodic IDR,
   which can be minutes on an idle desktop (capture only sends changes). The
   host re-encodes its last captured frame when the screen is idle, so the
   request is answered at once.
7. **Clock sync by min-RTT ping sample** (NTP-style midpoint) over TCP with
   `TCP_NODELAY`. Each platform's media clock is used directly (QPC,
   `mach_absolute_time`), so capture and present timestamps need no conversion.
8. **Viewer pipeline:** decode every frame on a dedicated thread (P-frames need
   their references), keep only the newest picture, present it on the next
   `CADisplayLink` tick. Simple and never blocks the network side. The M1
   numbers show this costs 1–2 refreshes of queueing; M5 replaces it with core's
   jitter buffer and vsync-targeted presentation.
9. **SPSC frame queue** (8 preallocated buffers) between core's receive thread
   and the decoder. A mutex/condvar is used only to park an idle consumer. If
   the queue is full the frame is dropped and the viewer resyncs on an IDR.

## Consequences
- The host sends one viewer at a time, IPv4 only.
- No pacing of datagram bursts yet: an IDR (~150 datagrams at 1440p) leaves in
  about 1 ms. Fine on wired gigabit; Wi-Fi may show loss → keyframe requests (M4/M5).
- Latency numbers include clock-sync error of ±RTT/2 (shown in the stats).

# 0025 — Audio (M4b): Opus in core, never broken or choppy

**Status:** accepted · **Date:** 2026-11-23

## Context
SPEC M4b: the host's sound to the viewer, in both directions of use (PC →
Mac, Mac → PC). The user's requirements:
- sound must never lose quality or turn broken or choppy (ADR 0018);
- while being viewed, the host doesn't play the sound on its own speakers
  (as with Remote Desktop), and they come back when the session ends.

Targets (SPEC §2): audio latency ≤ 40 ms, in sync with the picture
(±20 ms), no dropouts on a good link.

## Decision
- **Codec: libopus in core**, the `opus` crate, which builds libopus from
  its bundled source with CMake and links it statically (user's choice
  over each platform's own Opus).
  - Settings: 48 kHz stereo, 10 ms frames, restricted low-delay mode,
    192 kbps constrained VBR. That's music quality with about 5 ms of codec
    delay.
  - One encoder and decoder for both platforms, testable on any machine.
  - Opus's own concealment (PLC) covers a frame that never comes.
- **Transport** (`core/src/audio.rs`): a sealed `"GLAU"` datagram on the
  session's media socket.
  - Each datagram carries the newest frame plus the 2 before it, so losing
    one or two datagrams in a row costs nothing and adds no delay. That's
    about 600 kbps, small next to video.
  - Longer gaps are asked for again with a `"GLNA"` NACK. The host keeps
    640 ms of frames.
- **Playout:**
  - The receiver decodes frames in order into a lock-free ring. The
    device's callback only reads from it: no lock, no allocation, no wait.
  - The buffer target adapts: it starts at 30 ms, falls to 20 ms after
    10 s of calm, and grows by 10 ms after a gap or 5 ms after a
    concealment, up to 80 ms.
  - A missing frame is concealed only when the buffer is down to 5 ms and
    a newer frame is here. A stream that stops (session ending, sound off)
    goes quiet without counting as a gap.
  - Clock drift between the two machines is followed by resampling, up to
    ±0.5 %, steered by the buffer depth. It never drops or repeats audio.
- **Not playing on the host's speakers** (the user's rule): done per
  platform when capture is built (step 2). On the Mac, ScreenCaptureKit
  captures the apps' audio before the output device, so the output can be
  muted while viewed and restored after. On Windows, loopback capture
  reads the device's mix. Whether muting the device also silences the
  loopback depends on the device, so it gets tested on the real PCs. A
  virtual output device would need a vendor-signed driver (ADR 0008).

## Measured (core, before any app plays it)
- Unit tests, all passing:
  - a clean link plays every frame;
  - two datagrams lost in a row: rebuilt from the redundant copies;
  - 5 % random loss: 0 concealed, 0 gaps;
  - a 30 ms outage: covered, with a resend for the third frame;
  - a long outage: concealed, never silent;
  - device clocks 0.3 % fast or slow: no gaps;
  - no clicks: the biggest step between samples stays what a 440 Hz tone
    makes, with or without drift correction.
- losssim, 100 s per model, 4 ms round trip, NACKs and resends lost too:

  | loss | concealed | gaps | buffer |
  |---|---|---|---|
  | random 5 % | 0 | 0 | 20 ms |
  | random 10 % | 1 | 0 | 20 ms |
  | bursts 1.1 % | 0 | 0 | 20 ms |
  | bursts 3.9 % (heavy) | 1 | 4 | 30 ms |

## Steps
1. Core (this ADR): codec, packets, redundancy, NACK, playout. Done.
2. Windows host: WASAPI loopback capture → `AudioSender`, on the session's
   socket, marked as voice. Mac viewer: CoreAudio output reading the ring.
   The host's speakers muted while viewed.
3. Mac host: ScreenCaptureKit audio. Windows viewer: WASAPI low-latency
   output.
4. The service, where capture runs in the user's session helper (with the
   clipboard); the bar's sound on/off and volume; a Sound setting; A/V sync
   measured; the results lines (audio latency, concealed, gaps, buffer).

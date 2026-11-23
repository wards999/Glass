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
  - Each datagram carries the newest frame plus the 3 before it (2 at
    first; see below), so losing
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

## Revised after m4b-test1: sound in step with the picture
In m4b-test1 the sound ran half a second behind. Frames queued while the
Mac's output started, and only the decoded buffer was held at its target,
so the backlog stayed for the whole session. Now:
- **The total is held**, all the sound waiting (decoded and not): 20 ms
  normally. It can grow after losses, but never past 50 ms (it was 80),
  so sync holds on bad Wi-Fi too.
- **Catching up:**
  - before the output reads, only the newest frames are kept;
  - a backlog under 100 ms plays up to 2 % faster until it's gone
    (briefly a third of a semitone; nothing dropped);
  - a bigger one, after a stall, skips to the newest with a 10 ms fade-in.
- **3 redundant copies** instead of 2: three datagrams lost in a row cost
  nothing, with no wait for a resend. That's about 770 kbps. Older copies
  are left out if a datagram would exceed the socket's size.
- **Never silent:** when the buffer is about to run dry and the stream is
  live, Opus conceals a frame instead of leaving a gap.
- **The trade-off:** a small buffer means a few concealed frames on a bad
  link. At 5 % random loss that's 3 single frames in 100 s, no gaps. At
  2 % or less, none.
- **The A/V line:** the results say how far the sound is behind (or ahead
  of) the picture, including the output device's own latency. The Mac's
  overlay shows it live.

## Revised after m4b-test2: sound waits for a slower picture
- On a Mac showing a 165 Hz game at 60 Hz, the picture took 53 ms and the
  sound 30 ms, so the sound came early, which is more noticeable than late.
- The viewer now measures both once a second and holds enough sound (20–80
  ms) for it to play about 5 ms after its picture.
- The picture's own delay is M5's to fix (frames at the viewer's refresh).
- A frame is concealed only when it's lost or 15 ms overdue, never while
  it's merely late.

## Steps
1. Core (this ADR): codec, packets, redundancy, NACK, playout. Done.
2. Windows host: WASAPI loopback capture → `AudioSender`, on the session's
   socket. Mac viewer: CoreAudio output reading the ring. The host's
   speakers muted while viewed. Done in m4b-test1:
   - Loopback asks Windows for 48 kHz stereo float, converted from the
     device's own format. Frames follow the device's clock while sound
     plays. When nothing plays, silence frames go out on a 10 ms clock, so
     the viewer's buffer never runs dry.
   - Muting: checked on QUIETPC22's default output. Loopback still captured
     the tone at full level (RMS 0.0353 against 0.0354) while the speakers
     were muted. The mute is restored when the session ends, or if Glass
     stops capturing.
   - Mac viewer: AVAudioEngine with a source node that reads the ring in
     the render callback (no lock, no allocation), and a 256-frame output
     buffer (~5 ms).
   - Audio shares the video's socket, so it's marked as video, not voice.
     LATER(M5): its own socket marked as voice, if A/V numbers ask for it.
   - In-app host only: the service's sessions come in step 4.
3. Mac host: ScreenCaptureKit audio. Windows viewer: WASAPI low-latency
   output.
4. The service, where capture runs in the user's session helper (with the
   clipboard); the bar's sound on/off and volume; a Sound setting; A/V sync
   measured; the results lines (audio latency, concealed, gaps, buffer).

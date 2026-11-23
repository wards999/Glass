# 0027 — Video rate control on the viewer, delay-based

**Status:** accepted · **Date:** 2026-11-23 · M5

## Context
SPEC §6b asks for delay-based congestion control like WebRTC's GCC: back
off when one-way delay starts to rise, before loss. Since M5's frame-rate
changes, Glass sends 40–50 Mbps (twice the viewer's refresh), and nothing
adapted it to the link. On a good link that's fine. On a weak spot, a busy
network or during a file copy, a queue builds and the picture lags.

## Decision
- **The viewer runs the controller** (`core/src/ratecontrol.rs`), since
  it sees the delay and the loss.
  - Every 100 ms it takes the average *first-packet* network time of the
    frames that arrived. A queue stretches it; a big frame doesn't.
  - It compares that with a quiet baseline (the lowest tick average of
    10 s):
    - **more than 15 ms over, or any loss:** back off to 85 % of what
      actually arrived, and hold for 0.5 s;
    - **more than 6 ms over:** hold;
    - **otherwise:** raise the limit 5 % a tick while the encoder is using
      it, and drop it altogether at 200 Mbps. On a still screen the limit
      stays where it is, so the next motion doesn't burst at full rate.
  - The floor is 8 Mbps, below which text and fine detail go soft.
- **It sends the limit to the host** in a new control message:
  `VideoRate { bits_per_s }` (kind 23, protocol v14; 0 = no limit). It's
  sent when the limit changes by 3 % or more, and again every second.
- **The host applies it, never above its own setting.**
  - NVENC (through FFmpeg) and VideoToolbox both change the bitrate at the
    next frame, without a keyframe.
  - The service passes it to its screen helper (`ToHelper::Bitrate`).
- **The results** say what happened:
  - viewer: "rate control (M5): limit X / no limit now | backed off N
    times, lowest Y";
  - host: "rate control (M5): bitrate … | N changes, lowest Y".

## Consequences
- **Tests:** a good link is never limited; a lone Wi-Fi spike doesn't back
  off; a queue backs off at once and recovers; a loss backs off, never
  below the floor; the limit is resent each second.
- **What gives way first** (SPEC: desktop → frame rate before sharpness)
  comes with the Frame rate choice and Auto. This step only lowers the
  bitrate.
- File copies already follow the picture's delay (ADR 0016). Together, a
  copy slows first and the video only if that isn't enough.

# 0018 — Audio between M4 and M5

**Status:** accepted · **Date:** 2026-11-23

## Context
Audio was part of M5b (gaming), after M5 (smoothness). The user asked
whether audio might affect smoothness and should come first.

Audio itself barely affects the picture. Opus at about 0.1–0.25 Mbps is
small next to video. The effect runs the other way: Wi-Fi loss and jitter
hurt audio more than video. A lost video packet is a brief glitch; a lost
or late audio packet is an audible click or gap.

## Decision
The order is **M4 (robustness) → M4b (audio) → M5 (smoothness)**.
- **M4** designs loss recovery (FEC/NACK) with a packet format that carries
  more than one stream, so audio is protected from the start. No
  protocol rework later.
- **M4b** adds the host's sound: system audio capture, low-latency Opus, a
  small jitter buffer, and A/V sync, on top of M4's recovery.
- **M5** tunes pacing, rate control and A/V sync with the real traffic mix,
  measured with sound playing.

Audio before M4 would sit on a transport with no loss recovery and need
reworking straight after. Audio after M5 would mean tuning smoothness
without the real traffic.

## Requirement for M4b (user, 2026-11-23)
Sound must never lose quality or turn broken or choppy while it plays,
including during a file copy or on a weak Wi-Fi spot. So:
- audio gets the same loss recovery as video (FEC and NACK on its stream),
  with priority over video and files;
- the playout buffer stays steady, with no gaps;
- a lost packet is concealed (Opus PLC) and never heard as a click;
- the bitrate stays high enough for music, not just speech;
- the results report audio gaps and concealed packets.

## Consequences
- M5b becomes "Gaming: feel" (relative mouse, game mode, cadence).
- The milestone order follows the top rule: speed, smoothness and sharp
  text first.

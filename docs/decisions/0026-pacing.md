# 0026 — Pacing: short, so big frames aren't late

**Status:** accepted · **Date:** 2026-11-23 · M5

## Context
SPEC §6b: pace each frame's packets at about 1.5× the bitrate, and big
frames over up to two frame intervals, on their own thread. Until now a
frame left as one burst from the encode thread.

The measurements say be careful:
- In m5-test6 the Wi-Fi took full-speed bursts with no loss, and network
  p99 was 8.5 ms.
- Its biggest frame, a 335 KB scene change, would take ~30 ms at 1.5× a
  45 Mbps average. That's 30 ms late at exactly the moment the user
  notices (the break, a new scene).
- Bursts did hurt earlier (m5-test2's stutters at the break: frames to
  143 KB alongside 57 Wi-Fi spikes), and they will on a busier link.

## Decision
- **A frame is spread over at most half a refresh of the viewer's screen**
  (8.3 ms at 60 Hz), and never sent slower than 4× the recent average
  bitrate. A typical 47 KB frame goes in about 2 ms (from 0.5), and the
  335 KB scene change in 8 ms at most.
- **The first packets go at once**, from the frame sender. A thread sends
  the rest as they fall due (`core/src/pacer.rs`, inside the send lock the
  sealer already needs; no extra lock).
- **Newest first:** when a new frame is queued, what's left of the
  previous one goes at once. Never dropped, never queued behind.
- **Audio, input, NACK resends and keep-alives bypass the pacer**, so they
  always go first (input > audio > video).
- **The queue holds 768 datagrams**, fewer than the receiver's replay
  window (1024), so packets sent out of order with audio and resends are
  never refused. If it's full, a datagram goes at once.
- **No allocation per frame** (a fixed ring).
- **The results** say "pacing (M5): N frames spread over X ms on average
  (longest Y) | Z sent at once for a newer frame".

## Consequences
- Up to 8 ms later for the very biggest frames, 1–2 ms for typical ones,
  in exchange for no bursts.
- If the numbers show bursts don't matter on a link, the spread can come
  down (a constant). If they show they do, rate control (next) keeps
  frames smaller in the first place.

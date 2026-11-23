# 0019 — The viewer's refresh and the Windows virtual display move into M5

**Status:** accepted · **Date:** 2026-11-23

## Context
Glass's rule is speed, smoothness and sharp text first. Today:
- **Refresh rates are ignored.** The viewer reports its refresh in `Hello`,
  but a Windows host captures at its own monitor's rate, and the Mac host's
  virtual display is fixed at 60 Hz.
- **Mismatches show.** QUIETPC (60 Hz) viewed on QUIETPC22 (165 Hz)
  judders: each frame stays an uneven 2 or 3 refreshes. A 165 Hz host
  viewed on a 60 Hz Mac encodes and sends frames the Mac can never show,
  which wastes bandwidth that matters on Wi-Fi.
- **Text is soft on a smaller host screen.** A Windows host can only
  stream its real monitor's modes. QUIETPC's monitor tops out at
  1280x1024, so QUIETPC22 shows it stretched to 2560x1440. DSR modes are
  scaled back down for the monitor, so they don't help.

## Decision
Move these into **M5 (smoothness)**:
- The host captures and encodes **at the viewer's refresh**, never faster
  than the viewer can show. The viewer presents in step with its screen.
- **A virtual display on a Windows host** (from M7). A vendor-signed
  open-source IddCx driver (ADR 0008: no driver of our own) adds a screen
  at exactly the viewer's size and refresh, e.g. 2560x1440 at 165 Hz.
  Glass streams it 1:1: sharp text, no scaling, motion at the viewer's
  rate.
- A **Frame rate** choice in the viewer's bar, with its default in
  Settings: **Auto** (the default: the viewer's refresh, stepping down by
  itself when the network can't keep up) or a cap of 120, 60 or 30. Only
  rates the viewer's screen can show are offered. It switches live, like
  Colour. Use it for weak Wi-Fi, a laptop's battery, or a busy host GPU.
- **The Mac host's virtual display at the viewer's refresh,** up to what
  the Mac allows, instead of a fixed 60 Hz.

The order stays M4 → M4b (audio) → M5. M5's adaptive bitrate and M4's loss
recovery are what make the higher pixel and frame rates safe on Wi-Fi.

## Consequences
- 2560x1440 at 165 Hz is about 6x the pixels per second of 1280x1024 at
  60. Encode cost per frame rises a little, and the bitrate must follow
  the network (M5's controller). Measure p50/p99 latency and the frame
  interval before and after, as CLAUDE.md asks.
- Installing the IddCx driver is a one-time admin step, like the service.
- M7 keeps the scene-change burst and still-region refinement.

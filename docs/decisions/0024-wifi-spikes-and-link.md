# 0024 — Wi-Fi spikes named, and the link reported

**Status:** accepted · **Date:** 2026-11-23 · M4 step 5 (ADR 0020)

## Decision
- **Spikes** (`core/src/spikes.rs`, on the viewer):
  - Each frame's *first-packet* network time is tracked against a typical
    time. A frame's size doesn't stretch the first packet's time, so big
    frames don't look like spikes.
  - A frame 8 ms over the typical time is a spike. Spikes within 150 ms of
    each other count as one event.
  - If at least 70 % of the gaps between events are one period (or 2–3 of
    it, since some spikes are missed), the period names a likely cause:
    - 0.7–1.4 s: a Mac's AWDL (AirDrop, Handoff, Sidecar);
    - 40–80 s: a background Wi-Fi scan (Windows WLAN AutoConfig, or a
      Mac's);
    - 90–550 ms: Wi-Fi power save;
    - anything else regular: "cause unknown";
    - no pattern: interference, distance or other traffic.
  - The result is a line in the viewer's results on both platforms (core
    writes the text). No allocation per frame, and the summary is published
    about once a second with `try_lock`.
- **The link** (core `LinkTrack`, fed once a second):
  - Windows uses the WLAN API (no admin rights). The Mac uses CoreWLAN.
  - The results say "Wi-Fi here: signal (worst), quality (Windows), rate
    (lowest), channel, roams, channel changes", or "not on Wi-Fi (wired)".
    The viewer and the host both say it, so a session shows both ends.
  - A roam is a change of access point (BSSID). macOS only gives the BSSID
    with Location access, which Glass doesn't ask for, so on the Mac a
    channel change is the hint.
- **Never a setting change.** Glass names the cause. The SPEC's opt-in
  switches (AWDL, the adapter's power setting) are LATER(M5), if the
  numbers show they're needed.
- Checked on QUIETPC22: −42 dBm, quality 92 %, 1729 Mbps, channel 85.

## Consequences
- No protocol change: it's all measured locally.
- With M4's five steps done, M4's results need the real numbers from the
  test builds to close.

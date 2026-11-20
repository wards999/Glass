# 0006 — Wi-Fi is the primary network

**Status:** accepted · **Date:** 2026-11-20

## Context
The spec assumed wired gigabit (8–15 ms glass-to-glass at 120 Hz). The user's
main setup is home Wi-Fi with a 60 Hz MacBook Air. M2 on that setup: network
p50 5.6 ms but spikes to 110 ms and bursts that overflowed the receive queue.

## Decision
- SPEC §5d "Wi-Fi first": send pacing, Wi-Fi-sized adaptive/interleaved FEC,
  adaptive bitrate, a jitter buffer of typically 0–2 frames, DSCP/WMM marking,
  AWDL spike detection with user guidance (never changing system settings).
- §8 gets a Wi-Fi budget (≈ 20–30 ms typical, p99 < 50 ms at 60 Hz) next to
  the wired one; the §5c gaming pacing target is measured on Wi-Fi.
- M4 adds pacing, DSCP and a loss simulator with Wi-Fi-like bursts; M5 adds
  adaptive bitrate. Test steps assume Wi-Fi; wired is a comparison.

## Consequences
- Lowest-latency ideas that only work wired (e.g. zero jitter buffer) become
  options, not defaults.
- Every result file reports the network type (Wi-Fi band, RTT, spikes).

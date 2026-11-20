# 0005 — Gaming is in scope

**Status:** accepted · **Date:** 2026-11-20

## Context
The user wants to play PC games from the Mac (with audio and a controller)
without micro-stutter. The original spec targeted desktop work, had audio in
the last milestone (M8) and no controller support at all.

## Decision
- Add SPEC §5c (Gaming) with a **measured frame-pacing target**: ≤ 1 missed
  refresh in 1000 while the game produces frames, present-interval stddev
  < 1 ms, p99 < 1.5 refresh periods (5-minute wired session).
- M5 (smoothness) is judged against that target and matches the host's capture
  rate to the viewer's refresh.
- New **M5b – Gaming** straight after M5: audio with A/V sync, game controller
  (Mac GameController framework → virtual Xbox 360 pad via ViGEmBus on Windows),
  relative mouse mode, game-mode overlay.
- Audio leaves M8 for the PC → Mac direction; M8 keeps the Mac-host side.

## Consequences
- Security (M6) and clarity (M7) move one step later.
- The Windows host gains an optional third-party driver (ViGEmBus) with a
  one-time admin install; Glass must work without it (no controller).

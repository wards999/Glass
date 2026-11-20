# 0007 — Milestone order follows the use cases

**Status:** accepted · **Date:** 2026-11-20

## Context
SPEC §0 makes "the PC views the Mac with always-sharp text" (use case A) the
primary use, and "the Mac plays PC games smoothly with audio" (B) secondary,
both over Wi-Fi. The original order built A's direction only in M3b and its
sharpness only in M7.

## Decision
After M3: **M3b** (Mac host + Windows viewer, starting with a check of HEVC
4:4:4 hardware encode on the M4) → **M3c** (sharp text Mac → PC: virtual
display at the PC's exact resolution, 1:1, 4:4:4 or still-region refinement,
measured with a clarity test card) → **M4** (Wi-Fi robustness, both
directions) → **M5** (smoothness, StarDesk baseline first) → **M5b** (gaming)
→ **M6** (security + signing) → **M7** (remaining clarity work) → **M8**.
Milestone names stay the same so earlier results and ADRs still line up.

## Consequences
- Use case A becomes usable (and sharp) two milestones from now, before the
  Wi-Fi and smoothness work, which then improves both directions at once.
- The Mac host needs Screen Recording and Accessibility permissions from
  M3b; unsigned test builds will ask again after each update until M6 adds
  Developer ID signing.

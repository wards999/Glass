# 0028 — The remaining milestones are tested in three rounds

**Status:** accepted · **Date:** 2026-11-23

## Context
About seven milestones are left before the future viewers (M4 close-out,
M4b steps 3–4, the rest of M5, M3e, M7, M8, M5b). Each one so far has
meant its own builds and test sessions on both machines. Most of what's
left is tested by the same session anyway: a game with sound, PC → Mac
and Mac → PC. The riskiest piece, the Windows virtual display, depends on
a signed driver we don't control (ADR 0008: no trusted code signing of our
own), and finding out late would stall the work built around it.

## Decision
- **First, a check:** can Glass use an existing signed IddCx driver for the
  Windows virtual display (ADR 0019)? About an hour of reading and one try.
  If not, a fallback is chosen before Round 2 starts (for example the PC's
  real modes, scaled on the viewer).
- **Round 1: sound and smoothness, both directions.**
  - M4 close-out (numbers only).
  - M4b steps 3 and 4: Mac → PC sound, sound through the Windows service,
    and volume and on/off in the bar.
  - M5's rest, apart from the virtual display: frames locked to the
    viewer's refresh, the Frame rate choice, the Windows viewer's
    presentation, and the StarDesk and Moonlight comparisons.
  - M3e Wake-on-LAN (small and separate; a minute of the test).
- **Round 2: the Windows host's picture.** The virtual display at the
  viewer's size, Auto resolution, M7 (scene changes, still-region
  refinement) and M8's direct NVENC path (no keyframe at a bitrate change
  or after a loss) and MTU probe. All of them change how the PC captures
  and encodes.
- **Round 3: gaming feel (M5b),** tuned against Round 2's picture.
- M9 to M11 (iPad, Android, controllers) stay separate milestones.

**Within a round,** the pieces are built and checked one at a time here,
each with its own line in the results, then tested together in one session
both ways. One or two follow-up tests are expected per round, not one per
milestone. The milestone names, results files and tags stay as they are;
a round's tag is `rN-testK`.

## Consequences
- Far fewer test sessions on the two machines.
- A fault in a combined test is harder to place; the per-piece result lines
  are what keep it findable. If one can't be placed, that piece is turned
  off and the round is retested without it.
- CLAUDE.md's "one milestone at a time" now reads "one round at a time".

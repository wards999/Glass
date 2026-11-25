# 0033: The scorecard

## Context
Rounds of tuning were judged by feel ("sharper after about half a second",
"some cards didn't animate"), with numbers that mixed motion and idle time.
SPEC §0 has measurable targets; nothing measured them the same way twice.

## Decision
A scorecard session: the viewer sets `CAP_SCORECARD` in `Hello`, and the host
streams a fixed test scene (`core/src/scorecard.rs`) at the viewer's size
and refresh in place of its screen. The scene and the scoring live in core,
so both hosts draw the same pixels and any viewer scores them the same way.

- **Scene (33.5 s):** 3.5 s warm-up (card 0, plus a barcode of the scene's
  age in ms), six changes of a grey text-heavy test card 2 s apart, 10 s of
  the card scrolling 3 rows a frame at the viewer's refresh, 10 s of a still
  card whose 64-pixel marker changes with every input packet.
- **Clock:** the viewer finds scene time 0 as the earliest capture time minus
  the barcode's age. Input packets can't mark the start: heartbeats send them
  anyway.
- **Sharpness:** the viewer copies each decoded still picture's luma back and
  computes PSNR against the original (the card is grey, so BT.601 and
  BT.709 give the same Y). Time from a change's first frame to the first
  present at ≥ 45 dB; target ≤ 150 ms.
- **Smoothness:** presents while scrolling (after 0.5 s): refreshes without a
  new picture per 1000 (target ≤ 1), interval sd (< 1 ms), p99 (< 1.5
  refreshes).
- **Delay:** the viewer presses or lets go of F24 every 100 ms (an event: a
  host counts a pointer move alone as nothing to show); input ->
  screen of the marker frames (target p50 ≤ 30 ms, p99 ≤ 60 ms).
- **Output:** one PASS/FAIL line per target in the viewer's log (fetched with
  the results), `glass connect HOST --scorecard`.

## Consequences
- The real desktop isn't measured (its content varies); the scene is a fixed
  stand-in: small text, 1-pixel patterns, a grainy gradient.
- Luma only: 4:4:4's chroma isn't scored yet. LATER: a colour card.
- F24 is pressed and let go on the host while scoring (nothing answers it).
- Mac viewer scoring LATER; the Mac host draws the scene.

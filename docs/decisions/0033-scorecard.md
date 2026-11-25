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
- **Clock:** the viewer finds scene time 0 as the median of capture time
  minus the barcode's age over the warm-up's frames; a second barcode row
  holds the inverse, and a reading counts only if both agree (m5-test37: in
  QUIETPC's blurry 4:4:4 warm-up a misread, taken as the earliest, put every
  card against the wrong one: 15.6 dB). Input packets can't mark the start: heartbeats send them
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

## First results and what they changed (m5-test37 → m5-test40)
- Mac host 4:2:0 passed sharpness (all changes at 45 dB in 83 ms) and delay
  (25.6 / 29.3 ms); its 4:4:4 was blurrier (40.8 dB at best) and slower
  (18.5 ms a frame against 12.6). Mac hosts now start in 4:2:0; the viewer's
  colour toggle still asks for 4:4:4.
- The PC host's quality frame at QP 18 peaked at 44.2–44.8 dB: now QP 16
  (both hosts, `still_qp`), and 100 ms after the last change (was 200; the
  changes whose first frame wasn't sharp took 219–243 ms). Loopback 4:4:4:
  6 of 6 at 45 dB, worst 148 ms: PASS.
- No host here encodes a full-screen 2560x1440 change 165 times a second
  (RTX 2060 4:4:4 18 ms a frame, the Mac 12.6–18.5 ms). The PC host now
  lowers its frame rate in even steps (the viewer's refresh, half, a third)
  when encoding takes over 80 % of the gap for 0.3 s, and the scorecard
  judges smoothness at the steady rate reached, which its line states.
- The Mac viewer's present margin adapts (3–8 ms) to presents that miss.

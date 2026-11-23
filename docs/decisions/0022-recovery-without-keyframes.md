# 0022 — Recovering from a loss: keyframes on request only, for now

**Status:** accepted · **Date:** 2026-11-23

## Context
ADR 0020's step 3 was to repair a lost frame without a full keyframe: NVENC
reference-frame invalidation, or intra refresh, and whatever VideoToolbox
offers.

What we found:
- **FFmpeg's `hevc_nvenc` has no reference control.** Its options (FFmpeg
  in `third_party/ffmpeg`, checked with `ffmpeg -h encoder=hevc_nvenc`)
  offer `intra-refresh`, but no reference invalidation and no long-term
  references. Invalidation is only in NVIDIA's native API
  (`nvEncInvalidateRefFrames`). Using it means our own NVENC encoder
  instead of FFmpeg's.
- **VideoToolbox has long-term references** (`EnableLTR`, macOS 12, with
  low-latency rate control). Apple documents them for H.264. HEVC support
  is unconfirmed.
- **Intra refresh breaks the no-artifacts rule.** After a loss, the
  damaged parts stay on screen until the refresh sweep passes them. The
  alternative is to hold the picture for the whole sweep, which is longer
  than an IDR's round trip.
- **Losses are now rare.** With FEC and NACK (ADR 0021), recent sessions
  lost 0 frames. Their 2–3 keyframe requests were at the start and on
  stream changes, not losses.
- **Windows made an IDR every 10 s** (`gop_size = fps × 10`) regardless: a
  burst of up to ~200 KB and a hitch risk, for nothing. The Mac already
  made IDRs only on request (or every 10 minutes).

## Decision
- **No periodic keyframes.** NVENC's GOP is now `i32::MAX` frames. Checked
  on an RTX 4070: 30 s at 60 fps gave 1 keyframe, against 3 before. IDRs
  come at the start, when the stream changes, and on a viewer's request.
- **Measure the hitch.** The viewer's results now say "hitches after a
  loss: N (avg, max ms)". That's the time from giving up on a frame to the
  keyframe that resumes the picture.
- **No intra refresh.**
- **Reference invalidation waits for numbers.** If the reports show
  hitches that matter, the next step is an NVENC encoder of our own using
  the native API, which also needs its own D3D11 input path, plus LTR on
  the Mac if HEVC supports it. It's a big piece of work, and only worth it
  if losses that NACK can't save are common enough to see. AMD and Intel
  GPUs would still recover with an IDR.

## Consequences
- One less burst every 10 s on a Windows host.
- A lost frame still costs a keyframe round trip, now measured.

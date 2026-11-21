# M3c — Sharp text, view modes, 4:4:4: results

**Status: m3c-test1 done (views work; 4:4:4 not yet). m3c-test2 pending, with the 4:4:4 fix.**
**Date:** 2026-11-21 · design: ADR 0012 · protocol v5

## What's in it
- **View modes, chosen on the PC (Ctrl+Alt+Shift+R) and remembered per
  Mac:**
  - the Mac's own screen (default)
  - fit to this PC, like now (HiDPI virtual display)
  - fit, more space (1×)
  - fit, larger (2×)
  The fit modes are captured at the PC's exact size and shown 1:1, and the
  Mac's screen mirrors them while they're in use.
- **4:4:4 when both ends can:** the Mac converts captured BGRA to 4:4:4 on
  the GPU; the PC decodes it with NVDEC plus a shader. Otherwise 4:2:0, with
  an automatic fallback.
- **Clarity Test Card on the Mac:** while it's open, the host reports PSNR
  of what it sends.

## Measured here (Windows PC, loopback)
- Core: 81 tests pass, including the v5 messages and a Hello → SetMode →
  StreamChange round trip over the C ABI. Windows app: 9 tests.
- **2560×1440 HEVC 4:4:4 (NVENC test pattern):**
  - M3b viewer: 0 of 243 frames decoded (no D3D11VA 4:4:4 on the RTX 4070).
  - M3c viewer: 179 of 182 shown, 0 decode errors, NVDEC decode plus GPU
    copy p50 2.9 ms, capture → screen p50 3.3 ms.
  - In a window, Windows reported 169 of 175 on-screen times as earlier than
    the present call. Those count as the present call; full screen should
    report properly.

## Not compiled or tested here
- Mac: the virtual display, mirroring, mode switching, the 4:4:4 encode
  path, the test card and PSNR. These build in CI and need the real Mac.

## Measured — m3c-test1 (318 s, home Wi-Fi; MacBook Air M4 → PC 2560×1440 @ 165 Hz)

**View modes work.** The user cycled all four views about four times. Each
switch rebuilt the stream: a virtual display at 1706×960 HiDPI, 2560×1440,
or 1280×720 HiDPI, or the Mac's own 2940×1912. The PC followed every
StreamChange: 1 lost frame, 3 skipped, 0 rejected in 8984 frames.

| Stage (PC viewer) | m3b-test1 p50 | m3c-test1 p50 / p99 |
|---|---|---|
| encode (host) | 15.08 ms | 7.26 ms* |
| network | 3.02 ms | 2.12 / 7.41 ms |
| decode | 1.20 ms | 1.09 / 2.06 ms |
| present | 8.48 ms | 6.78 / 10.07 ms |
| **capture → screen** | 27.94 ms | **17.54 / 28.25 ms*** |
| input → screen | 38.74 ms | **32.58 ms** (p99 is the mode switches) |

\* **Measured from ScreenCaptureKit's frame time.** That time turned out to
be ahead of when the frame reaches Glass: "capture delivery" wrapped
negative. So it is the moment the Mac itself shows the frame, not when the
picture was ready.
- Host-side VideoToolbox encode alone was p50 **11.9 ms**, p99 16.2 ms.
  Capture → screen therefore reads "Mac's own screen → PC screen", and the
  encode figure excludes time before that moment.
- Input → screen (32.6 ms p50) doesn't depend on this and is the solid
  number.
- VideoToolbox at 12 ms against NVENC at 3 ms is the next latency target
  (M5).

**Sharpness (test card open, host decodes its own output, 908 frames):**
- luma PSNR p50 **51.8 dB**, chroma p50 53.6 dB.
- Luma ≥ 45 dB (visually lossless) in **96 %** of frames.
- The minimum, 29.4 dB, is at mode switches and scene changes.
- This compares against the 4:2:0 encoder input, so it doesn't measure the
  colour detail 4:2:0 discards. That needs the 4:4:4 run.

**Problems found:**
1. **No real 4:4:4.**
   - The stream was announced as 4:4:4, but VideoToolbox encoded 4:2:0: the
     Main profile was set, then cleared.
   - NVDEC got NV12, logged 30 decode errors, and the viewer asked for 4:2:0
     by itself. The fallback worked.
   - Fixed: no profile is set for 4:4:4.
2. **Wrapped timings:** frame times ahead of the clock wrapped to 4294967 ms
   in encode p99 and capture delivery. Fixed by saturating at 0.

## Measured — m3c-test2 / test3
- **test2:** still 4:2:0. The session used the low-latency encoder, which
  only does 4:2:0. Fixed in test3 by using the plain hardware encoder for
  4:4:4, plus a self-check that switches to 4:2:0 if the encoder makes it
  anyway.
- **test3:** real 4:4:4 at 2560×1440, then failures when the Mac's own
  2940×1912 view came back (NVDEC can't grow past its first size). Fixed in
  test4: a new decoder on every size change.
- The user confirmed the Mac's screen returns to normal after sharing.
  Asked which view was best: no clear difference, except that "fit" filling
  the PC screen without black bars was good, so it is now the default.

## Measured — m3c-test4 (194 s; 4:4:4 in every view)
4:4:4 held through three rounds of all four views: **0 decode errors, 0 lost
frames**, NVDEC on the PC. But it costs a lot:

| | 4:2:0 (test3) | 4:4:4 (test4) |
|---|---|---|
| Mac: capture delivery p50 | ~0 ms | **17.8 ms** (p99 75 ms) |
| Mac: VideoToolbox p50 | 12.6 ms (low-latency RC) | 13.7 ms (plain) |
| PC decode p50 | 1.7 ms | 4.2 ms |
| capture → screen p50 | 19.0 ms | **43.1 ms** |
| input → screen p50 | 32.9 ms | 30.5 ms |
| bitrate avg | 4.9 Mbps | **27.6 Mbps** |
| sharpness: luma PSNR p50 / ≥ 45 dB | 49.9 dB / 91 % | **38.8 dB / 23 %** |

- **The encoder isn't the problem:** only about 1 ms slower.
- **Capture is the problem:** ScreenCaptureKit hands over full-colour (BGRA)
  frames about a frame later than 4:2:0 ones, because it has no 4:4:4 YCbCr
  output.
- **Sharpness drops:** 4:4:4 at the same 40 Mbps cap has more data per frame
  to compress, so PSNR falls during changes. Without still-region refinement
  the last frame's quality stays on screen.
- **Conclusion:** at these settings 4:4:4 is slower and not sharper per
  frame. Default 4:2:0; 4:4:4 is a choice (`--colour`, Ctrl+Alt+Shift+C).
- **What would make 4:4:4 worth it:** a faster capture path, a higher bitrate
  or refinement while the screen is still (M5, or M7 for still-region
  refinement).
- **The user's verdict:** in 4:4:4 the red/blue stripes matched the Mac's
  screen and looked better than 4:2:0, but dragging felt laggier, and the lag
  **grew over time**.
  - Cause: encoding a 4:4:4 frame (plus the test card's sharpness probe) took
    about a frame's time, so ScreenCaptureKit's queue filled and every frame
    shown was older.
  - The MacBook Air is fanless; thermal slowdown would make that worse.
  - Fixed for test5:
    - Capture runs on its own queue, and only the newest frame waits for the
      encoder (stale frames are skipped and counted).
    - The probe decodes off the hot path.
    - The virtual display is kept when only the colour changes.
    - **4:2:0 is the default** (`Hello` bit `CAP_PREFER_420`), with 4:4:4 a
      choice: Ctrl+Alt+Shift+C, or `--colour 444`.


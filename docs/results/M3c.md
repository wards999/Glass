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

## Measured — m3c-test2
_Pending: 4:4:4 for real, the user's preferred view, and whether the Mac's
screen returns to normal._

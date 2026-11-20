# M3b — The PC views and controls the Mac: results

**Status: DONE.** m3b-test1 worked on the real PC and Mac (2026-11-20). `check-444` on the M4 still to come (it decides M3c).
**Date:** 2026-11-20 · design: ADR 0011 · protocol v4 (unchanged)

## What's in it
- **Mac host:** Share This Mac.
  - ScreenCaptureKit capture of the main display at its native pixels and
    refresh rate, then VideoToolbox HEVC in real time, IDRs on request.
  - CGEvent input: clicks with double/triple-click counts, drags, ⌘
    shortcuts, scroll, ISO/ANSI layout.
  - Pointer shape sent to the viewer.
  - Permission checks with links into System Settings.
  - Pairing PIN shown large, listed on the network, Copy Results after each
    session.
- **Windows viewer:** `connect.cmd` / `glass connect`.
  - D3D11VA hardware decode, GPU colour conversion and scaling.
  - Flip-model swap chain with one frame of latency and real on-screen times.
  - Full screen with letterboxing.
  - Keyboard hook (Win and Alt+Tab go to the Mac); Ctrl acts as ⌘.
  - Local pointer from the Mac's shape.
  - Pairing at the console, and the same report format as the Mac viewer.
- **Stable signing:** test builds are signed with one self-signed certificate,
  so Screen Recording and Accessibility grants survive updates.
- **Core:** key mapping for both directions and the host C ABI. The protocol
  is unchanged.

## Measured here (Windows PC, loopback)
- Core tests: 81 pass. New ones cover:
  - the host over the C ABI (pairing PIN event, session, frames, key input,
    wake/stop)
  - Annex-B from VideoToolbox-style output
  - Windows scan code ↔ HID for every key
  - PC keys → Mac key codes, including ISO
- Windows viewer against `glass serve`, real binaries, pairing at the console:
  - **320×192 fixture:** 0 decode errors, D3D11VA, DXGI frame statistics.
  - **2560×1440 HEVC (NVENC test pattern, 60 fps):** 291 frames, 57 fps
    shown, 0 decode errors.
    - Capture → screen p50 **5.4 ms**, p99 8.7 ms over loopback: decode
      0.25 ms + present 5.0 ms.
    - Present interval sd 2.6 ms.

## CI (macOS runner, packaged app)
- Swift builds and its unit tests pass:
  - the host over GlassKit: pairing PIN event, frames intact, keys arrive as HID
  - Annex-B helper
  - local addresses
- Glass.app is signed with the stable certificate (designated requirement
  `certificate leaf = H"d1d155bb…"`).
- **Mac host smoke test:** `GlassMac host --test-pattern` (1280×720 test
  pattern, VideoToolbox) streamed to `glass probe`: 188 frames in 4 s,
  0 lost, 0 rejected, pairing included.
- `check-444` on the runner (a VM): HEVC 4:2:0 and 4:4:4 hardware encode
  both YES. To be confirmed on the M4.

## Not compiled or tested here
- The Mac host (Swift) is built and unit-tested only in CI on macOS. The CI
  runner has no screen to share, so CI streams a test pattern from the
  packaged app to `glass probe`.
- Capture, the Screen Recording and Accessibility prompts, CGEvent input and
  the signing-certificate permission persistence need the real Mac.
- 4:4:4 hardware encode on the M4: `GlassMac check-444` on the real Mac.

## Measured — m3b-test1 (home Wi-Fi; MacBook Air M4 host 2940×1912 @ 60 Hz → PC viewer 2560×1440 @ 165 Hz)

**It worked.** Typing, pointer and scrolling all reached the Mac. The user
found the text **sharp and not small**, although it is scaled to 2214×1440
on the PC.

PC viewer, 106 s session: 1660 frames (15.7 fps on average: mostly a still
screen), **0 lost, 0 decode errors, 0 rejected**, 1 keyframe request, D3D11VA,
exact DXGI on-screen times, RTT 4.2–5.2 ms.

| Stage | Mac → PC p50 / p99 | PC → Mac (m6-test1) p50 / p99 |
|---|---|---|
| encode (host) | 15.08 / 67.26 ms | 2.91 / 15.32 ms |
| network (Wi-Fi) | 3.02 / 12.42 ms | 5.26 / 10.46 ms |
| decode | 1.20 / 2.48 ms | 1.98 / 3.15 ms |
| wait for refresh | 0.00 / 0.01 ms | 4.72 / 17.50 ms |
| present | **8.48** / 12.55 ms | **32.00** / 48.71 ms |
| **capture → screen** | **27.94 / 79.05 ms** | 48.00 / 71.93 ms |
| input → screen | 38.74 / 205.96 ms | 65.84 / 215.89 ms |

- **Mac → PC already meets SPEC §0's ≤ 35 ms p50** (27.9 ms). The Windows
  flip-model present takes 8.5 ms, against 32 ms in the Mac's display queue.
- **The Mac's encode step is the weak spot:** 15 ms p50 and 67 ms p99 in
  the long session.
  - In a short second session (59 frames at 5.5 fps, mostly idle) the
    host's own report gave encode p50 60 ms.
  - Sparse frames make it worse, as NVENC's GPU clocks did in M1 and M3.
  - The measure runs from ScreenCaptureKit's frame time to encoder output,
    so it also includes ScreenCaptureKit's delivery delay. The next build
    reports the two separately ("capture delivery", "VideoToolbox"). M5.
- The input → screen and present-interval p99s are idle gaps (screen
  unchanged), as before.
- The host report said "encoder SOFTWARE, low-latency rate control". That
  was a reporting bug: a session created with "hardware only" is hardware,
  but the low-latency encoder doesn't report the property. Fixed for the
  next build.

Problems found (fixed for the next build):
1. connect.cmd run from inside the zip couldn't find glass.exe: both .cmd
   files now say to extract first.
2. Stop Sharing during a session lost its results: they now open in their
   own window.
3. Copy Results gave no feedback: it is now **Show Results**, a window with
   Copy.

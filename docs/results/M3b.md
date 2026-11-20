# M3b — The PC views and controls the Mac: results

**Status: built and tested locally; `m3b-test1` on the real PC and Mac pending.**
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

## Not compiled or tested here
- The Mac host (Swift) is built and unit-tested only in CI on macOS. The CI
  runner has no screen to share, so CI streams a test pattern from the
  packaged app to `glass probe`.
- Capture, the Screen Recording and Accessibility prompts, CGEvent input and
  the signing-certificate permission persistence need the real Mac.
- 4:4:4 hardware encode on the M4: `GlassMac check-444` on the real Mac.

## Measured — m3b-test1
_Pending: PC viewer report and Mac host report, how it felt, check-444._

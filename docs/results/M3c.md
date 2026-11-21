# M3c — Sharp text, view modes, 4:4:4: results

**Status: built and tested locally; `m3c-test1` on the real PC and Mac pending.**
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

## Measured — m3c-test1
_Pending._

# 0012 — View modes and 4:4:4 (M3c)

**Status:** accepted · **Date:** 2026-11-21

## Context
In m3b-test1 the Mac's 2940×1912 screen was scaled to fit the PC's
2560×1440. The user found the text sharp and not small, and asked to keep
that view as an option rather than replace it. The M4 can hardware-encode
HEVC 4:4:4. The RTX 4070 can't decode 4:4:4 through D3D11VA: all 243 test
frames failed.

## Decision
- **The viewer picks the view (protocol v5):**
  - `Hello` carries its screen size, refresh rate, 4:4:4 decode ability and
    the mode it wants. `SetMode` changes the mode mid-session, and the host
    answers with `StreamChange` plus an IDR.
  - The modes are:
    - **0, the host's own screen:** as in M3b.
    - **1, fit like now:** a HiDPI virtual display, "looks like" viewer ÷ 1.5.
      The default for a new host (user's choice after m3c-test1: no black bars).
    - **2, more space:** 1×.
    - **3, larger:** 2×.
  - Modes 1–3 are captured at the viewer's exact pixel size, so the PC shows
    them 1:1.
  - Ctrl+Alt+Shift+R cycles the modes, and the choice is remembered per host
    address.
- **Mac virtual display:**
  - Built on the private `CGVirtualDisplay` classes, looked up at run time
    through a small Objective-C shim, so their absence means a fallback to
    mode 0 rather than a crash at launch.
  - While it exists, the Mac's own screen mirrors it. The mirroring uses
    `.forAppOnly`, so macOS restores the layout if Glass quits or crashes.
  - Refresh stays at 60 Hz. LATER(M5): the viewer's refresh rate.
- **4:4:4 when both ends can:**
  - **Mac host:** captures BGRA (ScreenCaptureKit has no 4:4:4 YCbCr).
    VideoToolbox's pixel-transfer session converts it to 4:4:4 BT.709 on the
    GPU, and the encoder takes that.
  - **Windows viewer:** decodes 4:4:4 with FFmpeg `hevc_cuvid` (NVDEC, low
    delay).
    - The three planes are copied into R8 D3D11 textures over CUDA–D3D11
      interop, so frames stay on the GPU.
    - A shader converts them to RGB, point-sampled when the picture is 1:1.
    - The CUDA driver is loaded at run time, so non-NVIDIA PCs report "no
      4:4:4" and get 4:2:0.
    - If NVDEC fails, the viewer asks for 4:2:0 (`SetMode`, `allow_444 = 0`).
- **Sharpness measurement:**
  - The Mac's Clarity Test Card window turns on a quality probe: the host
    decodes its own output and reports luma and chroma PSNR against the
    encoder input on keyframes and every 10th frame.
  - The probe adds a decode per frame, so it runs only while the card is open.
  - Still-region refinement isn't built. With 4:4:4 at 1:1 it may not be
    needed; the m3c numbers decide.

## Consequences
- v4 builds can't connect to v5 builds.
- Timing: on the 4:4:4 shader path in a window, Windows reported on-screen
  times before the present call (composed presentation). Those samples count
  as the present call, and the report says how many there were.
- The Windows host ignores `SetMode`. LATER(M7): an IddCx virtual display.

# 0011 — Mac host, Windows viewer, stable signing (M3b)

**Status:** accepted · **Date:** 2026-11-20

## Context
Use case A is the PC viewing and controlling the Mac. Everything on the wire
(TLS, pairing, sealing, packets, input) already exists in `core` for both
roles. M3b adds the two platform halves that were missing. macOS ties Screen
Recording and Accessibility grants to an app's code signature, and with no
Developer ID (ADR 0008) every ad-hoc test build would look like a new app.

## Decision
- **Mac host (Swift, `GlassKit/Host*.swift`):**
  - **Capture:** ScreenCaptureKit on the main display at its pixel size and
    refresh rate.
    - IOSurface 4:2:0 video-range buffers, BT.709 matrix, sRGB, no pointer.
    - Frames arrive only when the screen changes. An idle timer answers
      keyframe requests by re-encoding the last picture.
  - **Encode:** VideoToolbox HEVC Main with real-time mode and no frame
    reordering.
    - Low-latency rate control where macOS offers it; otherwise hardware
      only; software only as a last resort, which the report shows.
    - IDRs only on request.
    - Output goes through core (`glass_hevc_from_length_prefixed`) into
      Annex-B, with VPS/SPS/PPS before each keyframe. That's the same
      stream the Windows host sends.
  - **Threads:** capture, encode and every session call run on the capture
    queue. `VTCompressionSessionCompleteFrames` makes each encode
    synchronous, so the encoder's output handler sends while the queue
    waits and session calls never overlap.
  - **Input:** `CGEvent` posted to the HID tap from core's UDP thread.
    - Absolute positions on the display in points.
    - Click counts for double and triple clicks.
    - Modifier flags tracked and stamped on every event.
    - HID usages mapped to key codes by `core::keymap::hid_to_mac`, using
      the Mac's own ISO/ANSI layout.
  - **Pointer:** `NSCursor.currentSystem`, compared by pixels, sent as RGBA
    at stream scale. The viewer draws it as its own cursor.
  - **Permissions:**
    - Screen Recording (`CGPreflight/RequestScreenCaptureAccess`) and
      Accessibility (`AXIsProcessTrusted`) are checked live, with buttons to
      the right System Settings page.
    - Sharing still runs without Accessibility (view only), and the report
      says so.
  - **Discovery:** advertised as `_glass._tcp` with `NetService` (same TXT as
    the PC).
- **Windows viewer (Rust, `app-win/src/win/{viewer,decode,present}.rs`):**
  - **Decode:** FFmpeg HEVC on D3D11VA using the default GPU (any vendor).
    One decode thread, low-delay flags, and frames stay on the GPU.
  - **Present:** the D3D11 video processor converts YCbCr to RGB (BT.601 or
    709, studio or full range, read from the stream) and scales into the
    letterboxed picture area in one pass.
    - Flip-model swap chain with a waitable object and a maximum frame
      latency of 1.
    - `GetFrameStatistics` gives the real on-screen time for "present" and
      capture → screen.
    - Only the newest decoded picture is shown.
  - **Input:**
    - Window messages for the mouse.
    - A low-level keyboard hook while the window is in front, so Win,
      Alt+Tab and Alt+F4 go to the Mac. Ctrl+Alt+Del can't be caught.
    - Scan codes map to HID (`keymap::win_scan_to_hid`).
    - By default Ctrl → ⌘ and Win → ⌃ (`--keys labelled` sends them as
      labelled), so PC shortcuts keep working.
    - Local chords: Ctrl+Alt+Shift+F and Ctrl+Alt+Shift+Q.
  - **Pointer:** the host's shape becomes this PC's own cursor
    (`CreateIconIndirect`, scaled like the picture). There's no round trip.
- **Stable signing:**
  - CI signs Glass.app with one self-signed code-signing certificate, "Glass
    Self-Signed", kept as the repository secrets `MAC_SIGN_P12` and
    `MAC_SIGN_PASSWORD`. The private key also lives outside the repo on the
    build PC.
  - The certificate is trusted only on the CI runner, so codesign accepts it.
    Nothing changes on the user's Mac.
  - The app's designated requirement now names that certificate, so macOS
    keeps Screen Recording and Accessibility grants across test builds.
  - It is still not notarized: Gatekeeper's first-launch step is unchanged.
  - Local builds without the secret stay ad-hoc.
- **Protocol:** unchanged (v4). The C ABI gains the host functions, key
  mapping and Annex-B helpers.

## Consequences
- Either machine can host. Both apps keep their pairing in one identity, and
  roles add up: a PC paired to view the Mac can also be viewed by it after
  pairing the other way.
- The viewer assumes a Mac host for its key rule. LATER(M7): the host tells
  the viewer its OS in `Welcome`, so the rule picks itself.
- M3b streams 4:2:0. `GlassMac check-444` reports whether this Mac can
  hardware-encode HEVC 4:4:4, and that decides M3c's sharp-text approach.
- Mac → PC text is scaled (2940×1912 into a 2560×1440 screen, about 75%)
  until M3c adds the virtual display.
- The Windows viewer has no on-screen stats yet (M5). Its numbers are in the
  report and `results-viewer.txt`.

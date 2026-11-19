# 0002 — M1 offline pipeline: file format, FFmpeg binding, timestamps

**Status:** accepted (M1)

## Decisions

1. **Own `.glsr` file instead of MP4/MKV.** Frame boundaries and per-frame
   timing (capture time, encode time) are what M2's stream carries, and the Mac
   side reads them through `core` with no container parser. `glass dump` writes
   plain Annex-B for `ffplay`/`ffprobe`. Spec: `docs/recording.md`.

2. **Annex-B handling lives in `core`.** NAL splitting, access-unit splitting,
   parameter-set extraction and length-prefixed conversion are needed by both
   clients (VideoToolbox on Mac now, D3D11VA on Windows in M3b), so they're
   written once and exposed over the C ABI.

3. **`ffmpeg-sys-next` rather than the safe `ffmpeg-next` wrapper.** It's the
   same project, but the wrapper has no API for D3D11 hardware frame contexts,
   which zero-copy encode needs. The unsafe part is about 150 lines in
   `app-win/src/win/encode.rs`.

4. **Zero-copy path:** WGC BGRA texture → `CopySubresourceRegion` into an FFmpeg
   D3D11 pool texture (GPU→GPU) → `hevc_nvenc` with `rgb_mode` yuv420/yuv444.
   NVENC does the colour conversion, so the stream is BT.601 limited (FFmpeg
   forces that VUI for RGB input). A custom BT.709 shader conversion can come
   later if colour accuracy needs it.

5. **Capture timestamp = arrival time (QPC), not `SystemRelativeTime`.**
   Measured on Win11 at 165 Hz: `SystemRelativeTime` is ahead of arrival for
   about 98% of frames (p50 3.5 ms, max 7 ms, about one refresh). It behaves
   like a target present time. Arrival is the conservative start point for
   latency; the WGC time is kept only as a pacing signal.

6. **FFmpeg 8.1 LGPL shared build, pinned by URL and SHA-256**
   (`scripts/fetch-ffmpeg.ps1`), from the FFmpeg-Builds project (formerly
   BtbN). It's not committed; CI caches it.

7. **Mac decode is synchronous** for M1 so per-frame decode time is measured
   directly and the conversion buffer can be reused. LATER(M5): async decode
   driven by the jitter buffer.

## Consequences
- The Windows build needs `third_party/ffmpeg` and the MSVC toolchain.
- CI can't capture or encode (no GPU), so CI checks the format and the Mac
  decode/render path using small synthetic fixtures encoded here with NVENC.

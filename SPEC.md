# Project spec: low-latency Mac ↔ PC remote desktop

Working name: **Glass** (rename freely)

## The two overriding rules

Every design choice, default and trade-off in this spec serves these. When
anything else conflicts with them, they win.

1. **Sharp, never fuzzy.** Text and UI look like the host's own screen.
   - Never scale when it can be avoided: render at the viewer's pixel size and
     show it 1:1 (§4b, §5e). A lower size is only ever the user's explicit
     choice (or Auto, which they turn on), and it is rendered at that size,
     not shrunk.
   - Full chroma for desktop content (4:4:4, or still-region refinement to
     visually lossless within 150 ms, §0 A).
   - When the link is short of capacity on a **desktop** stream, give up frame
     rate before sharpness: fewer, sharp frames, never blurry ones. Bursts
     for scrolling and scene changes are allowed (§2.2).
2. **Never laggy.** Input reacts at once and the picture is never behind.
   - The pointer is drawn locally on the viewer (zero network delay).
   - Never queue frames to save them: drop stale frames, show the newest.
     The jitter buffer is the only deliberate delay, capped (§5d) and
     shrunk as soon as the link allows.
   - Input is sent the moment it happens, ahead of video, with priority marking (§5d).
   - For a **game** stream (use case B), smoothness and responsiveness come
     before sharpness: the bitrate (and with Auto, the size) drops before
     frames are delayed or stutter.

Honest limit: over Wi-Fi, a bad moment on the link (interference, a
microwave, walking out of range) can't be wished away. Glass's job is to
recover within a frame or two, keep the still picture sharp, and **show** in
the overlay that the link was the cause. Every result file reports how often
and how long that happened.

## 0. Use cases and acceptance criteria (read first)

Both use cases run **over home Wi-Fi** (§5d). If Glass doesn't beat the
existing tools on the criteria below, there's no reason to build it, so every
milestone reports against them.

The current test setup: Windows PC with an RTX 4070 and a 2560×1440 monitor
rated 144 Hz (Windows reported 165 Hz in M1–M3 tests, so it may be overclocked); MacBook Air M4 with a 2940×1912 @ 60 Hz built-in display. The numbers
in this spec use that setup as the example, but **nothing is hardwired to it**:

- At every connection the viewer reports its display (pixel size, refresh
  rate, scale, HDR later) and both sides report their capabilities (hardware
  encoders/decoders, 4:4:4 encode/decode, max size and rate). The stream size,
  shape, rate, codec and chroma are derived from that, never from constants.
- If a display changes mid-session (a different monitor, a resolution or
  refresh change, the viewer window moved to another screen, a lid closed),
  the viewer re-reports it and the stream is re-negotiated without
  reconnecting (same mechanism as §5e size changes).
- New hardware on either side (another GPU vendor, a different Mac, a 4K or
  ultrawide monitor, a 120 Hz ProMotion panel) should just work, with the best
  mode it supports. Today's Windows host needs NVIDIA (NVENC); AMD AMF and
  Intel QSV are planned (M8).

### A. Primary: the PC shows the Mac's desktop (Mac host → Windows viewer)
Desktop work: reading and writing text, code, web, documents.

- **Text is never fuzzy.** The Mac renders a desktop at the PC monitor's
  exact pixel size (2560×1440 virtual display, §4b) and the PC shows it **1:1,
  no scaling, no bars**. Chroma is full (4:4:4) or, if the Mac can't encode
  4:4:4 in hardware, an equivalent: still regions are refined to visually
  lossless.
  - **Still content:** within **150 ms** of something stopping, the viewer's
    picture matches the Mac's to visually lossless (PSNR ≥ 45 dB on the clarity
    test card; text edges exact).
  - **While moving** (scroll, drag): allowed to soften, but text stays readable
    and recovers within the 150 ms above. Wi-Fi capacity changes lower the
    bitrate first (§5d); resolution drops only if the user allows it (§5e).
  - The 1:1 native mode is the default; lower resolutions and the Mac's own
    shape (bars at the sides) are user choices (§5e), still as sharp as
    possible.
- **Very responsive:** input → screen **p50 ≤ 30 ms, p99 ≤ 60 ms** on Wi-Fi
  The Mac's own panel is 60 Hz, but the stream comes from the virtual display,
  which runs at the **PC's refresh rate** (e.g. 144 Hz) if the Mac's capture and
  hardware encoder keep up (measured in M3b; otherwise the highest rate they
  sustain). So Mac → PC isn't limited to 60 Hz. The pointer is drawn
  locally on the PC, so it always moves with zero lag.

### B. Secondary: the Mac shows the PC, often a Steam game (Windows host → Mac viewer)
- **No stutter:** the §5c pacing target on Wi-Fi (≤ 1 missed refresh in 1000,
  present-interval stddev < 1 ms, p99 < 1.5 refreshes at 60 Hz).
- **Audio:** in sync with the picture (±20 ms), audio latency ≤ 40 ms, no
  dropouts on a good Wi-Fi link.
- **Very responsive:** input → screen **p50 ≤ 35 ms, p99 ≤ 60 ms** on Wi-Fi at
  60 Hz; mouse-look works (§5c). Game controllers come later (M11).
- Text clarity matters less here than smoothness: the bitrate adapts first.

### How we prove it
Each result file (`docs/results/Mx.md`) states the network (Wi-Fi band, RTT,
spike count) and the numbers above for whichever use case the milestone
touches. "Feels fine" is recorded too, but the numbers decide.

## 1. Goal

A **two-way** remote desktop tool between a Windows PC and a Mac on the same LAN. Either machine can be the host, and either can connect to the other (A → B or B → A). Each app contains both roles. It:

- feels as **smooth** as StarDesk (even frame pacing, no micro-stutter),
- keeps **text sharp** enough to read (desktop work, not just games),
- is good enough to **play games on the PC from the Mac**: no micro-stutter, audio in sync, mouse-look works (§5c); game controllers later (M11),
- shows **clean frames straight away on big screen changes** (minimise/restore, app switching),
- works first and best on **home Wi-Fi** (the primary setup; wired is a bonus),
- stays at **very low latency**: about 20–30 ms glass-to-glass on good 5/6 GHz Wi-Fi at 60 Hz (8–15 ms on wired gigabit at 120 Hz; see §8),
- needs **no cloud account**: devices pair once and trust each other directly.

### Non-goals (v1)
- Connecting over the internet / NAT traversal (design for it, don't build it yet)
- Linux, iOS, Android
- Multi-user, multi-monitor, file transfer, mobile clients

## 2. Design principles (lessons from StarDesk vs Moonlight)

1. **Smoothness beats the absolute lowest latency.** A tiny adaptive jitter buffer (0–1 frames) plus presentation timed to the display's refresh. Accept about 1 frame of extra latency for even pacing.
2. **Content-aware rate control.** Treat desktop content differently from motion:
   - **Scene-change burst:** detect a large change → let that frame be 3–5× the normal size (clean IDR) instead of starving it.
   - **Refinement of still regions:** when regions stop changing, send a high-quality or near-lossless update for them.
3. **Text clarity:** stream at the Mac's *native pixel* resolution; prefer YUV 4:4:4 where hardware allows it; Windows scaling at 200%.
4. **Symmetric by design.** One protocol and one `core`; host and client are roles, not separate products. Each platform implements four role modules: capture+encode, inject (host); decode+render, input-capture (client).
5. **Never copy frames to the CPU** on the host. Capture → encode stays on the GPU.
6. **Measure everything.** Every stage is timestamped; latency is a first-class, visible metric.

## 3. Architecture

The diagram shows PC → Mac. Mac → PC is the mirror image, using each platform's equivalent APIs (see §4 and §5).

```
 Windows PC (host)                                   Mac (client)
┌──────────────────────────────┐   UDP media    ┌──────────────────────────────┐
│ Capture (Windows.Graphics.   │ ─────────────▶ │ Receive + FEC recover        │
│   Capture / DXGI dup)        │   (AES-GCM,    │ Jitter buffer (adaptive 0–1) │
│ Encoder (NVENC/AMF/QSV)      │    FEC)        │ Decode (VideoToolbox)        │
│ Rate control (content-aware) │                │ Render (Metal, CAMetalLayer) │
│ Packetiser + FEC             │ ◀───────────── │ Input capture (NSEvent)      │
│ Input injection (SendInput)  │   UDP input    │ Local cursor rendering       │
│ Audio (WASAPI loopback→Opus) │                │ Audio (Opus→CoreAudio)       │
└──────────────┬───────────────┘                └───────────────┬──────────────┘
               └──────── TCP + TLS 1.3 control channel ─────────┘
                 (pairing, settings, keyframe requests, stats,
                  key derivation for the UDP channels)
```

### Code layout

```
/core        Rust crate: protocol, packetiser, FEC, crypto, pairing,
             discovery, jitter buffer, rate-control logic, stats
             - used directly by the Windows app
             - built as a static lib + C header (cbindgen) for the Mac app
/app-win     Rust Windows app (host + client):
               host:   capture, encode, input injection, audio out
               client: decode, D3D11 render, input capture, audio in
               shell:  tray icon, device list, pairing UI
/app-mac     Swift macOS app (host + client), links /core:
               host:   ScreenCaptureKit, VideoToolbox encode, CGEvent injection
               client: VideoToolbox decode, Metal render, NSEvent input
               shell:  menu-bar app, device list, pairing UI
/tools       latency harness, packet-loss simulator, pcap/stat dumpers
/docs        protocol.md, latency-budget.md, decisions/ (ADRs)
```

Why this split: networking, FEC and crypto must match exactly on both sides, so they're written once in Rust. Platform media APIs (VideoToolbox, Metal, AppKit) are much nicer from Swift.

## 4. Windows as host

- **Capture:** Windows.Graphics.Capture (preferred) with DXGI Desktop Duplication as a fallback. Capture at the display's refresh rate; send frames only when something changed.
- **Encode:** Start with FFmpeg's libavcodec hardware encoders (`hevc_nvenc`, `hevc_amf`, `hevc_qsv`) via `ffmpeg-next` for speed of development. Plan for a direct NVENC SDK path later, which gives finer control (intra-refresh, VBV, per-frame QP, ROI).
  - Low-latency settings: no B-frames, no lookahead, zero frame delay, low-latency tuning, CBR with a small VBV for normal frames.
  - Codec: HEVC 4:2:0 by default; HEVC 4:4:4 (Range Extensions) where the encoder and the Mac decoder both support it; AV1 as an option.
- **Content-aware rate control (core differentiator):**
  - Scene-change detection: compare dirty-rect area or a cheap downscaled frame difference. Above a threshold → force an IDR and raise the VBV allowance for that frame.
  - Still-region refinement: track per-tile change age; once tiles have been still for N ms, send a refinement frame at low QP (or encode an ROI with QP offsets).
  - Bitrate floor/ceiling are configurable; LAN default 80–150 Mbps.
- **Input injection:** `SendInput` for mouse and keyboard; absolute mouse coordinates mapped to the captured display. (Later: a SYSTEM service so it works on the UAC/secure desktop.)
- **Audio:** WASAPI loopback → Opus (10 ms frames) → UDP.
- **Virtual display (later milestone):** reuse an existing open-source IddCx virtual display driver rather than writing a signed driver; create a display that matches the client's exact resolution and refresh rate.

## 4b. Mac as host

- **Capture:** ScreenCaptureKit (`SCStream`) at the display's refresh rate, delivering IOSurface-backed frames with only-on-change updates. Needs the **Screen Recording** permission.
- **Encode:** VideoToolbox `VTCompressionSession` (HEVC, hardware) with real-time and low-latency rate control (`kVTVideoEncoderSpecification_EnableLowLatencyRateControl`), no frame reordering, and a short max frame delay. 4:4:4 where supported. Scene-change bursts and still-region refinement use the same `core` rate-control logic as Windows, applied through per-frame bitrate/keyframe properties.
- **Input injection:** `CGEvent` posting (mouse, keyboard, scroll). Needs the **Accessibility** permission. Map Windows key codes to macOS key codes, and Ctrl ↔ Cmd by configurable rule.
- **Audio:** ScreenCaptureKit system-audio capture (macOS 13+) → Opus.
- **Virtual display (required for use case A, §0):** create a display at the viewer's exact pixel size (e.g. 2560×1440 for the PC monitor, at 1× so one Mac pixel = one PC pixel) at the viewer's refresh rate (e.g. 144 Hz), capped at what ScreenCaptureKit and VideoToolbox sustain and stream that, so the PC never scales. macOS has no public API: use the private `CGVirtualDisplay` approach used by open-source tools (e.g. BetterDisplay, DeskPad), behind a fallback that streams the physical display (scaled, with bars) if it ever breaks.
- **Text clarity on the Mac host:** check first (start of M3b) whether VideoToolbox can hardware-encode HEVC 4:4:4 on the M4. If not, use 4:2:0 for motion plus still-region refinement (re-encode unchanged areas at very low QP, or send them in a lossless 4:4:4 side path) to meet the §0 still-content target.
- **Unattended access:** run as a login item. The login window / locked screen is out of scope for v1.

## 5. Mac as client

- **Decode:** VideoToolbox `VTDecompressionSession` with real-time / low-latency hints; output IOSurface-backed `CVPixelBuffer`s with no copies.
- **Render:** Metal into a `CAMetalLayer`; `displaySyncEnabled` is configurable. Present using `CADisplayLink` (macOS 14+) timing; support ProMotion (120 Hz) and fixed 60 Hz displays.
- **Frame pacing / jitter buffer:** an adaptive target of 0–1 frames, raised automatically when measured arrival jitter goes up and lowered again when the network calms down. Drop late frames rather than queueing them.
- **Native resolution:** request a stream at the window's backing pixel size (`convertToBacking`). Never let the GPU scale text up if it can be avoided.
- **Input:** capture `NSEvent` mouse, keyboard and scroll events; send them immediately over UDP with sequence numbers. Render the cursor locally (the host sends its cursor shape and position separately).
- **Audio:** Opus decode → CoreAudio with a small jitter buffer.
- **UI:** a minimal SwiftUI device list (mDNS discovery), pairing screen, **full screen by default** (the main way to use it: it gives the most pixels for text) with a toggle to a movable window for occasional glances, and a stats overlay (fps, bitrate, per-stage latency, loss, jitter-buffer depth).

## 5b. Windows as client

- **Decode:** D3D11 Video (D3D11VA) via FFmpeg `hwaccel=d3d11va` or Media Foundation; decoded textures stay on the GPU.
- **Render:** a D3D11 flip-model swap chain (`DXGI_SWAP_EFFECT_FLIP_DISCARD`) with a waitable object (`SetMaximumFrameLatency(1)`) for low-latency presentation; supports tearing (`ALLOW_TEARING`) as a "lowest latency" option.
- **Frame pacing:** the same adaptive jitter buffer from `core`, presenting against the display's vblank (`IDXGIOutput::WaitForVBlank` / DWM timing).
- **Native resolution:** request a stream at the window's physical pixel size (per-monitor DPI aware v2).
- **Input:** Raw Input for mouse (relative + absolute) and a low-level keyboard hook while focused, so Win/Alt-Tab-style keys can be forwarded. Render the cursor locally.
- **UI:** full screen by default (borderless on the chosen monitor), with a toggle to a movable window; same stats overlay as the Mac client.
- **Audio:** Opus → WASAPI (low-latency shared mode).

## 5c. Gaming (PC host → Mac viewer first)

Playing a PC game from the Mac is a first-class use case, and the strictest
test of smoothness.

- **Frame pacing target (measured, not felt), on Wi-Fi:** with the host's
  capture rate matched to the Mac's refresh, over a 5-minute session on the
  user's home Wi-Fi: **no more than 1 in 1000 refreshes without a new frame
  while the game is producing frames** ("missed refresh"), present-interval
  stddev **< 1 ms**, p99 **< 1.5 refresh periods**. The adaptive jitter buffer
  may spend up to 2 frames to get there; glass-to-glass and input → screen
  stay in the §8 Wi-Fi budget. Wired runs are reported for comparison.
- **Even cadence: every shown frame is one game frame, shown exactly once, on
  an even beat.** A 60 Hz panel can't show 144 distinct frames a second;
  dropping the extras unevenly (2, then 3, then 2 game frames per refresh) is
  what makes animation judder. So:
  - The host samples at the viewer's refresh, phase-locked to the viewer's
    vsync (the viewer reports its vsync timing), instead of forwarding
    whatever the game produced last.
  - Best result: the game renders at the viewer's rate. In M7 the Windows
    host streams games from a virtual display (IddCx) set to the viewer's
    refresh (60 Hz for the MacBook Air, 120 for a ProMotion Mac). The game's
    own V-Sync then locks to it and every game frame becomes one Mac refresh,
    perfectly even, with the PC's monitor unaffected. Until then Glass detects
    a mismatched game rate and suggests a 60 fps cap (game setting or the NVIDIA
    frame limiter). Applying the cap automatically through NVAPI is optional,
    later, and opt-in.
  - The viewer presents each received frame exactly once, on the next refresh,
    with the jitter buffer absorbing Wi-Fi timing so no refresh repeats a frame.
  - Measured as **cadence error**: the spread of game-frame time between
    consecutive shown frames (0 when every refresh advances by one game frame).
    Target: on 99% of refreshes the picture advances by exactly one frame interval.
- **Host capture rate = viewer refresh rate** (e.g. 60 Hz for the MacBook
  Air), aligned to the viewer's vsync phase where possible, instead of the
  host monitor's own rate (e.g. 144–165 Hz). The
  viewer reports its refresh rate in `Hello`.
- **Audio:** WASAPI loopback → Opus (10 ms frames, 48 kHz stereo) → UDP with
  its own sequence numbers and FEC; the viewer plays it through CoreAudio with
  a small adaptive buffer (target ≤ 20 ms). A/V sync: audio and video both
  carry host capture timestamps; keep them within ±20 ms and correct drift by
  resampling, never by dropping video frames.
- **Game controller (M11, future; ADR 0013):** the Mac reads controllers with the GameController
  framework (Xbox, PlayStation, MFi) and sends button/axis state over the input
  channel (state-based like keys, at the controller's rate, ≥ 250 Hz). The
  Windows host presents them as a virtual **Xbox 360 controller** through the
  ViGEmBus driver (a one-time install with admin rights; detect it and guide
  the user). Rumble goes back to the Mac controller.
- **Relative mouse ("mouse-look"):** a toggle (and automatic when the host
  hides and confines its pointer) that captures the Mac pointer
  (`CGAssociateMouseAndMouseCursorPosition(false)`) and sends raw deltas;
  the host injects relative `SendInput` moves.
- **Game mode in the overlay:** frame-interval graph, missed-refresh count,
  audio buffer depth and A/V offset.
- Out of scope: anti-cheat that blocks injected input or virtual
  controllers; HDR (later).

## 5d. Wi-Fi first

Home Wi-Fi is the primary network. It adds delay spikes (tens to 100+ ms,
seen in M2), bursty delivery (frames held then released together), loss, and
capacity that changes as you move. Design for it:

- **Smooth, don't burst.** Pace each frame's datagrams over a few ms instead
  of sending ~150 at once (an IDR at 1440p), and keep frame sizes even: cap
  bursts (scene changes, IDRs) with the rate control, and prefer intra-refresh
  over full IDRs for recovery where the encoder supports it.
- **FEC sized for Wi-Fi** (M4): adaptive redundancy driven by measured loss,
  with interleaving so a short burst of loss doesn't wipe a whole group.
- **Adaptive bitrate** (M5): the viewer reports loss, delay growth and jitter;
  the host lowers or raises the target bitrate (and the 4:4:4 → 4:2:0 choice)
  so the stream fits the link's current capacity instead of building queues.
  Default ceiling 40–60 Mbps on Wi-Fi. If even the floor bitrate can't hold
  quality and "Auto" resolution is on (§5e), step the resolution down a rung,
  and back up when the link recovers (with hysteresis, so it doesn't flap).
- **Jitter buffer** (M5) adapts to the measured arrival jitter, typically
  0–2 frames on Wi-Fi, 0–1 wired; drops late frames rather than queueing.
- **Wi-Fi QoS:** mark media and input packets so WMM-capable routers and the
  hosts' own Wi-Fi drivers put them in the video/voice queues (details and
  per-OS caveats in §6b).
- **Mac Wi-Fi quirks:** AWDL (AirDrop/Continuity) can make the Mac's Wi-Fi
  hop channels and cause periodic delay spikes. StarDesk is smooth on the
  same Wi-Fi without touching AWDL, so Glass must absorb spikes the same way
  (pacing, FEC, jitter buffer, adaptive bitrate) rather than rely on switching
  AWDL off. Detect the spike pattern and report it in the stats.
  - **Opt-in toggle, as a backstop:** "Low-latency Wi-Fi (pauses AirDrop,
    Handoff, Universal Clipboard and Sidecar while streaming)", **off by
    default**. When on, a small privileged helper (approved once with the
    admin password) keeps `awdl0` down only while a session runs and restores
    it when the session ends, when Glass quits, or if Glass crashes (the helper
    watches the app). The overlay shows when it's active, and the stats show
    whether it helped. Without a Developer ID (§7b) the helper is installed
    by a one-time admin step (a LaunchDaemon that only toggles `awdl0` and
    only answers Glass on a local socket), not by `SMAppService`.
    Built after M5, and only if Glass still shows AWDL spikes there.
- **Baseline:** StarDesk on the same machines and Wi-Fi is the bar to beat.
  Measure it once (glass-to-glass with a phone slow-motion video, both
  directions) and keep the numbers in `docs/results/baseline-stardesk.md`.
- **Show the link:** the overlay shows Wi-Fi delay spikes, loss and current
  bitrate, so a bad link is visible as a link problem.

## 5e. Resolution and shape choices (both directions)

Native 1:1 is the sharpest, but it isn't always worth the data. The viewer
offers, per connection (remembered per host):

- **Shape:** *Fit this screen* (default: the host renders at the viewer's
  shape, no bars), or *Host's own shape* (e.g. the Mac's 16:10 desktop on the
  16:9 PC monitor, with black bars at the sides). Either shape stays
  **locked** for the session: the aspect ratio never changes on its own, and
  black bars are the expected result, not a problem to fix. Auto (below) only
  changes the size within the locked shape.
- **Never send more pixels than the viewer can show.** In the host's own
  shape, the stream is the largest size of that shape that fits the viewer's
  screen, and the host renders at that size. Example: the Mac's 2940×1912
  panel (≈1.54:1) on the 2560×1440 PC monitor is streamed at **2214×1440**
  (bars of 173 px each side), not 2940×1912. That's about 43% fewer pixels, and
  it's still 1:1 on the PC.
- **Resolution:** *Native* (default for use case A), a ladder of lower sizes
  in that shape (e.g. for 16:9: 2560×1440, 1920×1080, 1600×900, 1280×720), and
  *Auto* (start native, step down only when §5d's bitrate floor can't keep
  quality; default for use case B).
- **As sharp as possible at a lower size:**
  - **Render, don't shrink.** Where the host can render at the chosen size,
    it does, so text is drawn crisply at that size instead of being shrunk
    after the fact. Mac host: set the virtual display (§4b) to the chosen
    size. Windows host: the IddCx virtual display (M7); until then, or for the
    physical monitor, downscale on the GPU.
  - **Good scaling where scaling is unavoidable.** Host downscale on the GPU
    with an area/Lanczos filter (D3D11 video processor or a compute shader;
    VideoToolbox/vImage on the Mac), never on the CPU. Viewer upscale with a
    sharpening-aware filter (e.g. Lanczos or bicubic, optionally integer
    scaling when the ratio is exact: 1280×720 → 2560×1440 is exactly 2×).
  - Prefer sizes that divide the viewer's screen evenly (2×) and keep 4:4:4
    for text when the bitrate allows.
- Changing size or shape mid-stream: the host sends the new `StreamInfo` and an
  IDR; the viewer re-creates its decoder and view without dropping the
  session (needs M7's resolution-change handling, pulled into M3c).
- The overlay shows the current size, shape and why it changed (user / Auto).

## 6. Transport protocol

- **Control channel:** TCP + TLS 1.3 using mutual authentication against pinned certificates (rustls on the host, Network.framework or `core` on the client).
- **Media and input channels:** UDP. Keys are derived from the control-channel TLS session via the TLS exporter (RFC 5705/8446). Each packet uses AES-128-GCM (or ChaCha20-Poly1305) with a sequence-number nonce and a replay window.
- **Video packets:** header = {stream id, frame id, packet index, packet count, FEC group info, flags (IDR, refinement), capture timestamp}. Packets are sized to fit the MTU (about 1200–1400 bytes).
- **FEC:** Reed-Solomon over packet groups, with a configurable percentage (default 10–20%). Request a keyframe over the control channel only if FEC can't recover.
- **Clock sync:** NTP-style ping exchange over the control channel so the client can calculate true host-to-display latency.
- **Spec:** keep `docs/protocol.md` as the source of truth, versioned.

## 6b. Network design (Wi-Fi, in detail)

What the link actually does, and what Glass does about each part. Numbers are
starting points to tune from measurements, not fixed constants.

### The path
- **Count the wireless hops.** Mac (Wi-Fi) → router → PC (Wi-Fi) is two
  wireless hops: twice the airtime, two sets of retries, two chances of a
  spike. Mac (Wi-Fi) → router → PC (Ethernet) is one. Both are supported,
  but the setup check and README recommend a cable on whichever machine can
  have one (usually the PC). A mesh system adds a wireless backhaul hop:
  connect both to the same node where possible.
- **Band and width:** prefer 5 GHz or 6 GHz, 80 MHz or wider, good signal
  (RSSI better than about −65 dBm). 2.4 GHz works but is warned about. DFS
  channels can go silent for a minute after radar detection; report it if it
  happens.
- **Router features that break Glass:** client/AP isolation and guest
  networks block device-to-device traffic, and multicast filtering breaks
  mDNS (M6). Detect "can't reach the peer" and name these as likely causes; a
  typed address always works as a fallback.
- **Pick the right interface.** Machines have extra adapters (VPNs such as
  Tailscale, Hyper-V/WSL vEthernet, Bluetooth PAN, link-local 169.254.x). Bind
  to, and advertise, the interface on the same subnet as the peer. Warn if a
  VPN is routing local traffic or lowering the MTU.

### What Wi-Fi does to packets
- **Loss is mostly hidden as delay.** 802.11 retries at the MAC layer, so
  instead of losing a packet the link often delivers it several ms late. True
  loss comes in bursts (interference, roaming, a scan). So delay variation
  matters more than loss rate, and loss must be handled as bursts.
- **Aggregation (A-MPDU) and block-ack** batch packets, so arrivals are bursty
  even when sending is smooth. The jitter buffer sizes itself from measured
  arrival jitter, not from send timing.
- **Background scans and power save cause periodic spikes:**
  - Windows WLAN AutoConfig scans roughly every 60 s, and Wi-Fi adapters
    have power-saving modes that add wake-up delay. Glass measures spike
    periodicity per machine, names the likely cause in the stats (AWDL about
    1 s on the Mac, scans about 60 s on Windows) and gives guidance (e.g. the
    adapter's power-saving setting). It never changes settings without an
    opt-in (same pattern as the §5d AWDL toggle).
  - **Keep the radios awake.** During a session, both sides send at least a
    small packet every ~10 ms (input heartbeats and receiver reports already
    do this), so neither radio drops into power save between frames.
- **Roaming** between access points causes a gap of tens to hundreds of ms.
  Detect it (the BSSID changes where the OS reports it), keep the session
  alive, and request an IDR or refresh afterwards.

### Recovering from loss without adding delay
At home LAN RTTs (1–5 ms, below one frame interval) **retransmission is
cheap**, so Glass uses both FEC and NACK, as WebRTC does:
- **NACK first.** The viewer notices a missing packet (sequence gap or a
  short timeout) and immediately asks for it again; the host keeps the last
  ~100 ms of sent packets to answer. Used when RTT plus the remaining frame
  deadline allows it.
- **FEC for what NACK can't fix in time.** Systematic Reed–Solomon (or
  similar) over each frame's packets, with redundancy adapted to the measured
  burst-loss pattern. Interleave across a frame so one burst doesn't take
  out a whole group. Small frames get a minimum of protection, not a
  percentage. No FEC on input (it is already repeated, ADR 0004).
- **Never a full IDR if it can be avoided.** On unrecoverable loss: NVENC
  **reference-frame invalidation** (the next frame references the last frame
  the viewer has), or a rolling **intra refresh** that spreads the cost over
  several frames. A full IDR only as a last resort, paced. Check what
  VideoToolbox supports for the Mac host (M3b) and fall back accordingly.
- The loss simulator (`/tools/losssim`) uses a **Gilbert–Elliott burst
  model** plus delay spikes and aggregation-style batching, with parameters
  fitted to real M2/M3/M4 traces, not just uniform random loss.

### Congestion and bitrate control
- **Delay-based**, like WebRTC's GCC. The viewer sends a receiver report
  every ~20–50 ms: per-frame arrival times, one-way delay trend (from the
  synced clocks), loss and recovery counts, jitter. The host treats a rising
  one-way delay as the queue starting to build and backs off **before** loss
  happens. Back off fast, ramp up slowly, and probe up carefully (padding or
  FEC as the probe).
- **The encoder bitrate follows the controller per frame** (NVENC and
  VideoToolbox both accept bitrate changes without an IDR).
- **What gives way first** follows the overriding rules: desktop → frame rate
  before sharpness; game → bitrate (and with Auto, resolution) before
  smoothness.
- **Other traffic in the house** (downloads, video calls) shares the airtime;
  the controller is what keeps Glass from turning that into lag.

### Pacing and send path
- **Pace every frame's packets** with a token bucket at about 1.5× the
  current bitrate, so a frame leaves well within its interval (e.g. a 55 KB
  frame at 50 Mbps → ~6 ms) instead of as one burst. Big frames (IDR, scene
  change) are paced over up to two frame intervals.
- **Priority:** input > audio > video, and newest video first. A late frame's
  remaining packets are dropped if a newer frame is ready (never queue).
- **Timers:** Windows needs high-resolution waitable timers (or a 1 ms
  timer period) for sub-ms pacing; macOS uses `mach_wait_until`. Pacing runs
  on its own thread, not the capture/encode thread (the M2 send time showed it
  belongs off that thread).
- **Fewer syscalls:** batch sends (Windows UDP send offload `UDP_SEND_MSG_SIZE`
  / RIO; `sendmsg_x` batching on macOS) once measurements show syscall
  cost matters (M2: ~22 µs per datagram on the PC's NIC).

### Packet marking (QoS)
- **Classes:** input and audio as voice (DSCP EF/46 → WMM AC_VO); video as
  video (DSCP CS4/32 or AF41/34 → WMM AC_VI); control and bulk as best effort.
- **macOS:** set `SO_NET_SERVICE_TYPE` (`NET_SERVICE_TYPE_VO` / `NET_SERVICE_TYPE_RV`)
  so the Mac's own Wi-Fi driver queues them right, plus `IP_TOS` for the router.
- **Windows:** `IP_TOS` from an app is ignored by default. Use the qWAVE API
  (`QOSAddSocketToFlow` with `QOSTrafficTypeVoice`/`AudioVideo`), which
  works without admin rights for these traffic types. Verify by packet capture.
- Routers that ignore or strip DSCP are common; marking must help where it
  works and never hurt where it doesn't.

### Sizes, MTU and buffers
- Datagrams ≤ 1400 bytes with the DF bit set; no IP fragmentation, ever.
  At connect, probe that a full-size datagram gets through (VPNs and some
  mesh systems lower the path MTU) and drop to 1200 if not.
- Socket buffers large enough for a paced IDR plus jitter (4 MB requested),
  but the jitter buffer, not the socket, is where delay is allowed to live.
- IPv4 now; IPv6 link-local later (M6).

### Clock sync
- Keep re-estimating the offset (min-RTT filter over a sliding window) and
  also track **drift**: QPC and `mach_absolute_time` drift by a few ppm, which
  over a long session adds up to milliseconds. Fit offset plus drift, and
  report the error bound.
- One-way delay trends (for congestion control) need only drift-corrected
  relative timing, not perfect absolute sync.

### Visibility
- **Wi-Fi link stats in the overlay and results:** band/channel/width, RSSI,
  PHY rate, from the OS (Windows WLAN API, macOS CoreWLAN where it's allowed
  without extra permission).
- Per session: one-way delay p50/p99, spike count and periodicity, burst-loss
  stats, NACK and FEC recoveries, bitrate changes, IDR/RFI count.
- **Optional packet-timing trace** (timestamps only, no content) to feed the
  loss simulator and to diagnose a bad session after the fact.

## 7. Security

- **No cloud account.** Discovery via mDNS/Bonjour on the LAN.
- **Pairing:** one device shows a 6-digit PIN → the other enters it → a PAKE-style exchange (e.g. SPAKE2 or similar) → the devices swap long-term certificates and pin them. Pairing is done once per pair of devices and covers both directions.
- **Per-direction permissions:** each device decides separately whether a paired device may *connect to it* (be its client). "A may control B" doesn't imply "B may control A".
- Every later connection uses mutual TLS against the pinned certificates; anything unpaired is rejected.
- Per-session ephemeral keys (forward secrecy); periodic rekeying for long sessions.
- Host-side setting: "require confirmation for each connection" vs unattended access.
- Clipboard and audio are off until enabled per paired device.

## 7b. Distribution without an Apple Developer ID

Glass will not be signed with an Apple Developer ID or notarized. Design around it:

- **Stable self-signed code signature.** CI signs `Glass.app` with one
  self-signed code-signing certificate (made once; the private key is kept as
  an encrypted GitHub Actions secret), instead of ad-hoc signing. macOS ties
  Screen Recording, Accessibility and Local Network approvals to the app's
  signing identity, so with a stable certificate they **survive updates**;
  ad-hoc signatures change with every build and would make the user re-grant
  them each time.
- **Gatekeeper:** each downloaded build still needs right-click → Open (or
  "Open Anyway", or `xattr -dr com.apple.quarantine`) on first launch. The
  README and the release notes say so.
- **Permissions (Mac host):** detect missing Screen Recording/Accessibility at
  start, open the right System Settings pane, and explain the one-time
  approval. Keep the bundle ID (`local.glass.Glass`) and certificate fixed.
- **No `SMAppService`/privileged-helper APIs** that need notarization;
  anything needing root (only the optional §5d Wi-Fi toggle) is a one-time
  admin install step.
- **Private APIs are allowed** (no App Store): `CGVirtualDisplay` for §4b.
- **Windows:** the exe is unsigned too; SmartScreen needs "More info → Run
  anyway" per build, and Windows Firewall asks per folder. `host.cmd` and the
  README explain both. The ViGEmBus driver (M11) is signed by its vendor.

## 8. Latency budget (targets)

| Stage | Wi-Fi (primary), 60 Hz Mac | Wired gigabit, 120 Hz |
|---|---|---|
| Capture | 1–2 ms | 1–2 ms |
| Encode | 2–5 ms | 2–5 ms |
| Packetise + pacing + network + FEC | 3–8 ms | < 1 ms |
| Jitter buffer | 0–33 ms (adaptive, typically one frame) | 0–8 ms |
| Decode | 2–4 ms | 2–4 ms |
| Render + scanout | 0–17 ms (refresh-dependent) | 0–8 ms |
| **Glass-to-glass** | **≈ 20–30 ms typical, p99 < 50 ms** | **≈ 8–15 ms typical** |

M2 measured 48 ms p50 on Wi-Fi at 60 Hz; most of that (32 ms) was the Mac's
display queue, which M5 removes.

### Measuring latency
- In-band: capture timestamp → client present timestamp (clock-synced), shown in the overlay and logged.
- Glass-to-glass: host test mode that flashes a full-screen box/timestamp; record both screens with a phone's slow-motion camera (or a photodiode later).
- Smoothness: log frame-to-frame present intervals and report their standard deviation/p99 (this is the "feels like StarDesk" metric).

## 9. Milestones

PC → Mac was built first through M3. From there the order follows §0: use
case A (the PC views the Mac, text always sharp) comes next, then Wi-Fi
robustness and smoothness, which both use cases need, then gaming (use case B).
Milestone names are kept stable so earlier results and ADRs still match; the
numbers give the order. (Reordered 2026-11-20, ADR 0007; security moved up
straight after M3, ADR 0009: from M3 on, a host lets a viewer control the
machine, so nothing unpaired may connect.)

1. **M0 – Skeleton:** repo layout, `core` crate building for Windows and macOS, Swift app linking `core`, CI build for both.
2. **M1 – Offline pipeline:** PC captures and encodes to an HEVC file; Mac decodes and renders that file with Metal. Measure encode/decode times.
3. **M2 – Live stream (unencrypted, LAN):** UDP packetiser, no FEC; live PC video on the Mac. Stats overlay with in-band latency.
4. **M3 – Input:** mouse and keyboard back-channel, local cursor. Usable PC → Mac remote desktop.
5. **M6 – Security (moved up, §7):** PIN pairing (SPAKE2) that swaps and pins long-term self-signed certificates; every connection uses mutual TLS 1.3 against the pinned certificate (rustls in `core`, so the Mac uses it through the C ABI); UDP media/input encrypted with AES-128-GCM, keys from the TLS exporter, sequence-number nonces and a replay window; per-direction permissions; optional "ask on the host before each connection"; PIN attempt limits; identities stored with DPAPI on Windows and in a 0600 file on the Mac (Keychain once builds have a stable signing certificate; ADR 0010). mDNS discovery (devices listed by name, typed address still works). Both directions, so M3b's Mac host starts secure.
6. **M3b – Reverse direction (use case A):** first check whether VideoToolbox can hardware-encode HEVC 4:4:4 on the M4 (decides the clarity approach). Then the Mac host (ScreenCaptureKit + VideoToolbox encode + CGEvent, with Screen Recording/Accessibility permission checks) and the Windows viewer (D3D11VA decode + flip-model swap chain + Raw Input/keyboard hook + local pointer), full screen on the PC. Same protocol, with no changes to `core` beyond bug fixes. Role selection in both apps.
7. **M3c – Sharp text, Mac → PC (use case A):** (as built, ADR 0012: the viewer picks the view with Ctrl+Alt+Shift+R — the Mac's own screen scaled as before, or a virtual display at the PC's size: "like now" HiDPI (the default: fills the PC screen, no black bars), more space 1×, larger 2× — remembered per Mac; 4:4:4 when the viewer can decode it, NVDEC on NVIDIA PCs since D3D11VA has no 4:4:4 there, else 4:2:0.) Originally: 2560×1440 virtual display on the Mac (`CGVirtualDisplay`, fallback to the physical display), shown 1:1 on the PC; native-resolution negotiation (viewer sends its size in `Hello`); resolution and shape choices with render-don't-shrink on the Mac and GPU scaling elsewhere (§5e), with mid-stream size changes; 4:4:4 or still-region refinement; a clarity test card and PSNR measurement against the §0 still-content target (≤ 150 ms to visually lossless). Pulled forward from M7.
8. **M3d – Apps on both ends:** a real Windows app replacing host.cmd/connect.cmd: one Glass.exe with a tray icon and a main window (Connect to a Mac / Share this PC), a list of Glass hosts found on the network plus a typed address, PIN dialogs for both pairing directions, paired devices with Forget, status and Show Results with Copy, settings (view mode, key mapping, ask before connecting, start with Windows). While viewing, a small floating handle near the top (faint, draggable sideways, position kept) opens a bar (on both viewers; the top-edge trigger was dropped because it clashed with the remote Mac’s menu bar and macOS’s own full-screen menu bar) with one-click switches for the view (Fit / More space / Larger / host's screen) and colour (4:2:0 fast / 4:4:4 full colour), plus full screen and disconnect; the keyboard shortcuts stay. **Any pair works the same way** (PC→Mac, Mac→PC, PC→PC, Mac→Mac): the host says what it is (Mac or PC) and what it can do (view modes, 4:4:4) when connecting, so the viewer picks the keyboard mapping itself (Ctrl stays Ctrl between two PCs; ⌘ stays ⌘ between two Macs; Ctrl↔⌘ across) and the bar greys out what the host can't do; the Mac viewer also sends its screen size so a Mac host can fit it. PC→PC is tested on two real PCs before M3d is done; Mac→Mac (only one Mac available) is tested automatically in CI (the packaged app hosting a test pattern to its own viewer, keys checked without injecting) and, optionally, against a macOS virtual machine on the Mac. **No connection loops:** a machine can't view itself, and a viewer can't connect to a host that is already viewing it, directly (A↔B) or through other machines (A→B→C→A). Each viewer's `Hello` lists the machines that currently see it (transitively); a host that finds itself on that list refuses with a clear "that would make a loop" message (protocol v7). The command line stays for CI and tests. Mac: a menu-bar icon so sharing runs without a window, the same settings. An app icon for both: one design drawn as SVG, exported to .icns (Mac, with the sizes macOS wants) and .ico (Windows exe, window and tray, with a small-size variant that stays readable at 16 px). Front end for the later Windows SYSTEM service.
9. **M3e – Wake-on-LAN (ADR 0014):** wake a sleeping paired machine from the device list. When a viewer connects, the host sends the hardware (MAC) addresses of its network adapters (protocol v8); the viewer stores them with the paired device and its last address. Paired devices that aren't on the network show in the list as “sleeping” with a **Wake** button, which sends the standard magic packet (UDP broadcast to port 9, to the subnet broadcast and to the last address) and then waits for the device to reappear, showing progress, and connects when it does. The host side helps the user enable it: Windows detects whether the adapter allows wake from magic packet and says where to turn it on (Device Manager > adapter > Power Management, and the BIOS/UEFI setting), without changing settings itself; the Mac relies on “Wake for network access” (System Settings > Energy/Battery), which Glass checks and links to. Honest limit, shown in the app: wake works reliably over **Ethernet**; most PCs can't wake from sleep over **Wi-Fi**, and a Mac on Wi-Fi wakes only through a Bonjour Sleep Proxy (an Apple TV or HomePod on the network). Waking from fully off (S5) depends on the BIOS. Both directions, any pair.
10. **M3f – Windows service: unattended access (moved up from M8, ADR 0015):** a small Windows service (`glass-cli service run`, LocalSystem) keeps an agent (`glass-cli agent`) running as SYSTEM in the PC's console session. The agent shares with DXGI Desktop Duplication, following the input desktop, so the lock screen, sign-in and UAC prompts can be seen and controlled from far away; this PC's identity moves to a protected machine folder so pairings carry over. Installed, updated and removed from the Glass window (Settings > Unattended access) or `glass-cli service install | uninstall`, as administrator: Program Files, a firewall rule on private networks, restart on failure. The window shows the service's status and pairing code and detects Remote Desktop sessions (“Move it to the PC's screen”). LATER: Ctrl+Alt+Del where required (SendSAS), updates through the service without an admin prompt, a Mac LaunchDaemon.
11. **M4 – Robustness (Wi-Fi, §6b):** NACK retransmission plus adaptive interleaved FEC, reference-frame invalidation / intra refresh instead of IDRs, paced sending on its own thread, QoS marking (qWAVE on Windows, service types on macOS), MTU probe, interface selection, radio keep-awake, receiver reports, Wi-Fi link stats and spike classification, loss simulator with a Gilbert–Elliott burst model fitted to real traces. Both directions.
12. **M5 – Smoothness:** adaptive jitter buffer, display-linked presentation on both viewers, capture rate matched to the viewer's refresh, delay-based congestion control and adaptive bitrate with the Auto resolution ladder (§5e, §6b), clock drift tracking, frame-interval metrics against the §5c pacing target on Wi-Fi. StarDesk baseline measured before it (§5d). Compare against StarDesk and Moonlight.
13. **M5c – Clipboard and files (both directions):** clipboard sync of text, rich text (RTF/HTML) and images, with a size cap. A **file copied** on one machine is sent in the background and lands on the other machine's clipboard as a real local file, so a normal paste works in Finder, Explorer, mail or chat; files above a limit (default 200 MB, adjustable) wait for the paste and show progress. **Drag and drop** onto the viewer window sends files to the host's Downloads, and the host can send files back. Everything goes over the paired TLS connection on its own channel, paced by M5's rate control so a transfer never stalls the picture on Wi-Fi. An on/off switch on both machines; files carry their name, size and a hash, and partial transfers are discarded. LATER: native "promised file" copy and paste (Windows virtual-file clipboard, macOS file promises) so huge files copy only when pasted. *Built (protocol v8, ADR 0016): text at once; pictures and files only when pasted (Windows virtual files, Mac lazy pasteboard items), no size limit. Still to do: stream huge files while pasting, drag and drop, pacing from M5.*
14. **M5b – Gaming: audio and feel (use case B):** audio (Opus, A/V sync), relative mouse mode, game-mode overlay, even cadence (vsync-locked sampling, cadence-error metric, frame-cap hint; §5c). Measured with a real Steam game (§5c), played with mouse and keyboard. Game controllers are split out to M11. The opt-in low-latency Wi-Fi toggle (§5d) goes here if M5 still shows AWDL spikes.
15. **M7 – Clarity (rest):** native resolution for PC → Mac (Windows virtual display, IddCx, at the viewer's size **and refresh**, so games lock to the Mac's rate), scene-change burst, still-region refinement on the Windows host.
16. **M8 – Extras:** direct NVENC SDK path, audio for the Mac-host direction.
17. **M9 – iPad viewer (future):** view and control a Mac or PC host from an iPad. Reuses `core` (Rust, built for iOS) and the Mac viewer's Swift pieces (VideoToolbox decode, Metal, the control bar); touch mapped to pointer gestures (tap = click, two-finger scroll, long press = right click, drag), on-screen and hardware keyboards, Magic Keyboard/trackpad and Pencil as pointer. Pairing and encryption as everywhere. Viewer only: iPadOS can't be remote-controlled.
18. **M10 – Android viewer (future, maybe):** the same for Android phones and tablets: `core` via JNI, MediaCodec decode, touch and keyboard input. Viewer only.
19. **M11 – Game controllers (future; split out of M5b, ADR 0013):** controllers on the viewer (GameController framework on the Mac, Xbox/PlayStation/MFi; XInput/GameInput on a Windows viewer) sent as button and axis state over the input channel (≥ 250 Hz); a Windows host presents a virtual Xbox 360 pad through ViGEmBus (optional one-time admin install, detected and explained; Glass works without it), a Mac host a virtual HID gamepad if macOS allows it. Rumble goes back to the viewer's controller. Both directions, any pair.

Each milestone ends with a short written result in `docs/results/Mx.md` (what works, measured numbers, known issues).

## 10. Risks and open questions

- 4:4:4 hardware decode support on the specific Mac (check early, in M1).
- How much control FFmpeg's wrappers expose vs the NVENC SDK (VBV bursts, intra-refresh, ROI).
- macOS `CAMetalLayer` presentation latency and ProMotion behaviour.
- Input into admin windows, UAC prompts and the lock screen on Windows needs a SYSTEM service (as RDP-style tools and TeamViewer/Jump Desktop do): the last step, after M8. Until then host.cmd asks for admin rights at start, which covers admin windows but not UAC or the lock screen (m6-test1 found that without it, an admin window in front blocks all input).
- Mac as host: Screen Recording + Accessibility permissions, no public virtual-display API, and VideoToolbox gives less rate-control precision than NVENC (check scene-change bursts work in M3b).
- Key mapping between Windows and macOS (Ctrl/Cmd, Alt/Option, Win key) needs a clear, configurable rule.
- Wi-Fi is the primary network (§5d): delay spikes, bursts, loss and changing capacity are the main threat to smoothness. The adaptive buffer and bitrate must handle them without hurting the wired case. Mac AWDL channel hopping causes periodic spikes we can only detect and explain.
- Gaming: ViGEmBus is a third-party driver that is no longer actively developed; if it stops installing, look at alternatives (e.g. a UMDF virtual HID device). Some games' anti-cheat rejects injected input or virtual pads. Games that change resolution or go exclusive-fullscreen can interrupt capture (M7's resolution handling).
- Aspect mismatch: the Mac panel is 16:10 (2940×1912) and the PC monitor 16:9 (2560×1440). PC → Mac full screen shows bars top and bottom; Mac → PC shows bars at the sides, and the Mac's Retina image is scaled to ~75%, so text gets small. Bars-free, 1:1 needs the host to render at the client's shape: a virtual display (Windows IddCx driver, Mac `CGVirtualDisplay`) or a matching host mode (M7/M8).

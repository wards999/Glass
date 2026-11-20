# Project spec: low-latency Mac ↔ PC remote desktop

Working name: **Glass** (rename freely)

## 0. Use cases and acceptance criteria (read first)

Both use cases run **over home Wi-Fi** (§5d). If Glass doesn't beat the
existing tools on the criteria below, there's no reason to build it, so every
milestone reports against them.

The setup: Windows PC with RTX 4070 and a 2560×1440 @ 165 Hz monitor; MacBook
Air M4 with a 2940×1912 @ 60 Hz built-in display.

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
    bitrate (§5d) but never the resolution.
- **Very responsive:** input → screen **p50 ≤ 30 ms, p99 ≤ 60 ms** on Wi-Fi
  (PC display at 165 Hz, Mac capturing at up to 120 Hz). The pointer is drawn
  locally on the PC, so it always moves with zero lag.

### B. Secondary: the Mac shows the PC, often a Steam game (Windows host → Mac viewer)
- **No stutter:** the §5c pacing target on Wi-Fi (≤ 1 missed refresh in 1000,
  present-interval stddev < 1 ms, p99 < 1.5 refreshes at 60 Hz).
- **Audio:** in sync with the picture (±20 ms), audio latency ≤ 40 ms, no
  dropouts on a good Wi-Fi link.
- **Very responsive:** input → screen **p50 ≤ 35 ms, p99 ≤ 60 ms** on Wi-Fi at
  60 Hz; game controller and mouse-look work (§5c).
- Text clarity matters less here than smoothness: the bitrate adapts first.

### How we prove it
Each result file (`docs/results/Mx.md`) states the network (Wi-Fi band, RTT,
spike count) and the numbers above for whichever use case the milestone
touches. "Feels fine" is recorded too, but the numbers decide.

## 1. Goal

A **two-way** remote desktop tool between a Windows PC and a Mac on the same LAN. Either machine can be the host, and either can connect to the other (A → B or B → A). Each app contains both roles. It:

- feels as **smooth** as StarDesk (even frame pacing, no micro-stutter),
- keeps **text sharp** enough to read (desktop work, not just games),
- is good enough to **play games on the PC from the Mac**: no micro-stutter, audio in sync, a game controller and mouse-look work (§5c),
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
- **Virtual display (required for use case A, §0):** create a display at the viewer's exact pixel size (2560×1440 for the PC monitor, at 1× so one Mac pixel = one PC pixel; up to 120 Hz) and stream that, so the PC never scales. macOS has no public API: use the private `CGVirtualDisplay` approach used by open-source tools (e.g. BetterDisplay, DeskPad), behind a fallback that streams the physical display (scaled, with bars) if it ever breaks.
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
- **Host capture rate = viewer refresh rate** (60 or 120 Hz), aligned to the
  viewer's vsync phase where possible, instead of the host's 165 Hz. The
  viewer reports its refresh rate in `Hello`.
- **Audio:** WASAPI loopback → Opus (10 ms frames, 48 kHz stereo) → UDP with
  its own sequence numbers and FEC; the viewer plays it through CoreAudio with
  a small adaptive buffer (target ≤ 20 ms). A/V sync: audio and video both
  carry host capture timestamps; keep them within ±20 ms and correct drift by
  resampling, never by dropping video frames.
- **Game controller:** the Mac reads controllers with the GameController
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
  Default ceiling 40–60 Mbps on Wi-Fi.
- **Jitter buffer** (M5) adapts to the measured arrival jitter, typically
  0–2 frames on Wi-Fi, 0–1 wired; drops late frames rather than queueing.
- **Wi-Fi QoS:** mark media and input packets with DSCP (video CS4/AF41, audio
  and input EF) so WMM-capable routers put them in the video/voice queues.
- **Mac Wi-Fi quirks:** AWDL (AirDrop/Continuity) can make the Mac's Wi-Fi
  hop channels and cause periodic delay spikes. StarDesk is smooth on the
  same Wi-Fi without touching AWDL, so Glass must absorb spikes the same way
  (pacing, FEC, jitter buffer, adaptive bitrate) rather than switch AWDL off.
  Only detect the pattern and report it in the stats. Never change system
  settings.
- **Baseline:** StarDesk on the same machines and Wi-Fi is the bar to beat.
  Measure it once (glass-to-glass with a phone slow-motion video, both
  directions) and keep the numbers in `docs/results/baseline-stardesk.md`.
- **Show the link:** the overlay shows Wi-Fi delay spikes, loss and current
  bitrate, so a bad link is visible as a link problem.

## 6. Transport protocol

- **Control channel:** TCP + TLS 1.3 using mutual authentication against pinned certificates (rustls on the host, Network.framework or `core` on the client).
- **Media and input channels:** UDP. Keys are derived from the control-channel TLS session via the TLS exporter (RFC 5705/8446). Each packet uses AES-128-GCM (or ChaCha20-Poly1305) with a sequence-number nonce and a replay window.
- **Video packets:** header = {stream id, frame id, packet index, packet count, FEC group info, flags (IDR, refinement), capture timestamp}. Packets are sized to fit the MTU (about 1200–1400 bytes).
- **FEC:** Reed-Solomon over packet groups, with a configurable percentage (default 10–20%). Request a keyframe over the control channel only if FEC can't recover.
- **Clock sync:** NTP-style ping exchange over the control channel so the client can calculate true host-to-display latency.
- **Spec:** keep `docs/protocol.md` as the source of truth, versioned.

## 7. Security

- **No cloud account.** Discovery via mDNS/Bonjour on the LAN.
- **Pairing:** one device shows a 6-digit PIN → the other enters it → a PAKE-style exchange (e.g. SPAKE2 or similar) → the devices swap long-term certificates and pin them. Pairing is done once per pair of devices and covers both directions.
- **Per-direction permissions:** each device decides separately whether a paired device may *connect to it* (be its client). "A may control B" doesn't imply "B may control A".
- Every later connection uses mutual TLS against the pinned certificates; anything unpaired is rejected.
- Per-session ephemeral keys (forward secrecy); periodic rekeying for long sessions.
- Host-side setting: "require confirmation for each connection" vs unattended access.
- Clipboard and audio are off until enabled per paired device.

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

Build PC → Mac first through M3, then add the reverse direction (M3b) before hardening, so `core` is proven symmetric early. §0 sets the priorities: use case A (PC views the Mac) is primary.

1. **M0 – Skeleton:** repo layout, `core` crate building for Windows and macOS, Swift app linking `core`, CI build for both.
2. **M1 – Offline pipeline:** PC captures and encodes to an HEVC file; Mac decodes and renders that file with Metal. Measure encode/decode times.
3. **M2 – Live stream (unencrypted, LAN):** UDP packetiser, no FEC; live PC video on the Mac. Stats overlay with in-band latency.
4. **M3 – Input:** mouse and keyboard back-channel, local cursor. Usable PC → Mac remote desktop.
5. **M3b – Reverse direction:** Mac host (ScreenCaptureKit + VideoToolbox encode + CGEvent) and Windows client (D3D11VA decode + flip-model swap chain + Raw Input). Same protocol, with no changes to `core` beyond bug fixes. Role selection in both apps.
6. **M4 – Robustness:** FEC (adaptive, interleaved, sized for Wi-Fi), send pacing, DSCP marking, packet-loss simulator in `/tools` with Wi-Fi-like burst loss and delay spikes (replayed from M2/M3 measurements).
7. **M5 – Smoothness:** adaptive jitter buffer, display-linked presentation, capture rate matched to the viewer's refresh, adaptive bitrate, frame-interval metrics against the §5c pacing target on Wi-Fi. Compare against Moonlight and StarDesk.
   - **M5b – Gaming:** audio (Opus, A/V sync), game controller (GameController → ViGEmBus), relative mouse mode, game-mode overlay. Measured with a real game (§5c).
8. **M6 – Security:** mDNS discovery, PIN pairing, pinned mutual TLS, encrypted UDP.
9. **M7 – Clarity:** native-resolution negotiation, 4:4:4 path, scene-change burst, still-region refinement.
10. **M8 – Extras:** clipboard, virtual display driver integration, direct NVENC SDK path, audio and controller for the Mac-host direction.

Each milestone ends with a short written result in `docs/results/Mx.md` (what works, measured numbers, known issues).

## 10. Risks and open questions

- 4:4:4 hardware decode support on the specific Mac (check early, in M1).
- How much control FFmpeg's wrappers expose vs the NVENC SDK (VBV bursts, intra-refresh, ROI).
- macOS `CAMetalLayer` presentation latency and ProMotion behaviour.
- The UAC/secure desktop on Windows needs a SYSTEM service; out of scope until after M8.
- Mac as host: Screen Recording + Accessibility permissions, no public virtual-display API, and VideoToolbox gives less rate-control precision than NVENC (check scene-change bursts work in M3b).
- Key mapping between Windows and macOS (Ctrl/Cmd, Alt/Option, Win key) needs a clear, configurable rule.
- Wi-Fi is the primary network (§5d): delay spikes, bursts, loss and changing capacity are the main threat to smoothness. The adaptive buffer and bitrate must handle them without hurting the wired case. Mac AWDL channel hopping causes periodic spikes we can only detect and explain.
- Gaming: ViGEmBus is a third-party driver that is no longer actively developed; if it stops installing, look at alternatives (e.g. a UMDF virtual HID device). Some games' anti-cheat rejects injected input or virtual pads. Games that change resolution or go exclusive-fullscreen can interrupt capture (M7's resolution handling).
- Aspect mismatch: the Mac panel is 16:10 (2940×1912) and the PC monitor 16:9 (2560×1440). PC → Mac full screen shows bars top and bottom; Mac → PC shows bars at the sides, and the Mac's Retina image is scaled to ~75%, so text gets small. Bars-free, 1:1 needs the host to render at the client's shape: a virtual display (Windows IddCx driver, Mac `CGVirtualDisplay`) or a matching host mode (M7/M8).

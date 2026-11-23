# CLAUDE.md

This repo builds **Glass**, a two-way low-latency remote desktop between a Windows PC and a Mac. Either machine can host, and either can connect to the other. `SPEC.md` is the design source of truth; read it before starting any milestone.

## Overriding rules (SPEC "The two overriding rules")
1. **Sharp, never fuzzy:** no avoidable scaling, full chroma for desktop, frame rate gives way before sharpness on desktop streams.
2. **Never laggy:** local pointer, no frame queues (newest wins), input first; on game streams smoothness and responsiveness give way to nothing.
Check every change against both; say in the results how a milestone moved them.

## How to work
- Work **one round at a time** (SPEC.md §9, ADR 0028): build a round's pieces one by one, then test them together. Don't start the next round until the current one runs end-to-end and each milestone's `docs/results/Mx.md` is written with measured numbers.
- End each milestone (and each re-test) with a **test build**: push a tag `mN-testK` (e.g. `m2-test1`). CI publishes a prerelease with the Windows zip and Mac dmg (`docs/testing.md`). Put that milestone's test steps in the release notes and `docs/testing.md`; the user runs them and sends back the results.
- Prefer the simplest thing that works for the current milestone; leave `// LATER(Mx):` notes rather than building ahead.
- Record any significant design decision as a short ADR in `docs/decisions/NNNN-title.md`.
- Keep `docs/protocol.md` in sync with any change to packet formats or the control messages; bump the protocol version.

## Code
- `/core` (Rust): protocol, packetiser, FEC, crypto, pairing, stats. No platform media APIs here. Exposes a C ABI via cbindgen for the Swift client.
- `/app-win` (Rust): Windows app containing both host and client roles. GPU-only capture → encode and decode → present.
- `/app-mac` (Swift): macOS app containing both host and client roles (ScreenCaptureKit, VideoToolbox, Metal, CGEvent/NSEvent). Zero-copy via IOSurface-backed buffers.
- Anything both directions need (protocol, FEC, crypto, jitter buffer, rate-control decisions) belongs in `/core`, not duplicated in the apps.
- Hot paths (capture, encode, packetise, receive, decode, present): no allocations per frame, no blocking locks, no logging above debug level.
- Every pipeline stage records timestamps into the stats system.
- Nothing is hardwired to the current test machines (RTX 4070 + 1440p165 monitor, MacBook Air M4). Sizes, refresh rates, shapes, codecs and chroma come from what each side reports at connect time (SPEC §0) and are re-negotiated when a display changes. Those machines may appear only as examples in tests and docs.

## Testing and measurement
- Unit tests for `core` (packetiser round-trip, FEC recovery under loss, replay window, key derivation).
- `/tools/losssim`: run the stream through simulated loss/jitter; FEC must recover at 5% random loss.
- Any change to the hot path reports before/after latency (p50/p99) and frame-interval jitter from the stats overlay/logs.

## Platform notes
- `/app-win` must be built on Windows and `/app-mac` on macOS (Apple Silicon). If you're running on one platform, say clearly which parts couldn't be compiled or tested there.
- Mac as host needs Screen Recording and Accessibility permissions; detect missing permissions and guide the user to System Settings. Mac as client needs none.

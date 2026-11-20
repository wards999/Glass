import AppKit
import CGlassCore
import Foundation
import GlassKit
import QuartzCore

// M2: live viewer. Core receives and reassembles on its own threads; a decode
// thread here decodes every frame (P-frames need their predecessors) and keeps
// only the newest picture; the display link presents that picture each refresh.
// LATER(M5): core's adaptive jitter buffer + presenting just ahead of vsync.

/// Local-clock (µs) timeline of one decoded frame.
struct FrameTiming {
    var captureLocal: UInt64 // 0 until clock sync has a sample
    var lastPacket: UInt64
    var decodeEnd: UInt64
}

/// Latency rings for one reporting window (all µs).
final class LiveStats {
    let encode = Stats() // host: capture -> encoded
    let network = Stats() // encoded (host) -> last packet here
    let queue = Stats() // last packet -> decode start
    let decode = Stats()
    let wait = Stats() // decoded -> display-link pickup (waiting for a refresh)
    let present = Stats() // submit -> on screen (presentedTime)
    let total = Stats() // capture (host) -> on screen
    let interval = Stats() // between presents
    var frames = 0
    var presented = 0
    var bytes = 0

    func reset() {
        for s in [encode, network, queue, decode, wait, present, total, interval] { s.reset() }
        frames = 0
        presented = 0
        bytes = 0
    }
}

final class LiveViewer: NSObject, NSWindowDelegate {
    let client: GlassClient
    let renderer: Renderer
    var onFinish: (String) -> Void = { _ in }

    private var window: NSWindow!
    private var view: MetalView!
    private var layer: CAMetalLayer!
    private var overlay: NSTextField!
    private var link: CADisplayLink?
    private var finished = false
    private let started = GlassClock.nowUs()
    private var windowStart = GlassClock.nowUs()
    private var lastOverlay: UInt64 = 0

    // Shared by the decode thread, presented handlers and main; guarded by `lock`.
    private let lock = NSLock()
    private var latest: (CVPixelBuffer, FrameTiming)?
    private var stopReason: String?
    private var superseded = 0
    private var submittedCount = 0 // handed to Core Animation (presented = confirmed on screen)
    private var decodeErrors = 0
    private var usingHardware = false
    private var lastPresentedUs: UInt64 = 0
    private var finalStats = GLASS_ClientStats()
    private let session = LiveStats()
    private let recent = LiveStats() // last ~2 s, for the overlay

    init(client: GlassClient) throws {
        self.client = client
        renderer = try Renderer()
    }

    private var streamDesc: String {
        let i = client.info
        return "\(i.width)x\(i.height) @ \(i.fps) Hz HEVC \(client.is444 ? "4:4:4" : "4:2:0")"
    }

    func open() {
        let w = CGFloat(client.info.width), h = CGFloat(client.info.height)
        let screen = NSScreen.main
        let scale = screen?.backingScaleFactor ?? 2
        // Windowed size: 1:1 pixels if that fits, else 80% of the screen.
        var size = NSSize(width: w / scale, height: h / scale)
        if let vf = screen?.visibleFrame, size.width > vf.width * 0.9 || size.height > vf.height * 0.9 {
            let s = min(vf.width * 0.8 / size.width, vf.height * 0.8 / size.height)
            size = NSSize(width: (size.width * s).rounded(), height: (size.height * s).rounded())
        }
        window = NSWindow(contentRect: NSRect(origin: .zero, size: size),
                          styleMask: [.titled, .closable, .miniaturizable, .resizable],
                          backing: .buffered, defer: false)
        window.title = "Glass — \(client.host)"
        window.isReleasedWhenClosed = false
        window.collectionBehavior = [.fullScreenPrimary]
        window.contentAspectRatio = NSSize(width: w, height: h)
        window.backgroundColor = .black
        window.delegate = self

        view = MetalView(frame: NSRect(origin: .zero, size: size))
        view.wantsLayer = true
        layer = (view.layer as! CAMetalLayer)
        layer.device = renderer.device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.displaySyncEnabled = true
        layer.backgroundColor = NSColor.black.cgColor
        view.onResize = { [weak self] in self?.updateDrawableSize() }

        overlay = NSTextField(labelWithString: "Connected to \(client.host). Waiting for the first frame…")
        overlay.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        overlay.textColor = .white
        overlay.drawsBackground = true
        overlay.backgroundColor = NSColor.black.withAlphaComponent(0.65)
        overlay.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(overlay)
        NSLayoutConstraint.activate([
            overlay.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 12),
            overlay.topAnchor.constraint(equalTo: view.topAnchor, constant: 12),
        ])

        window.contentView = view
        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        updateDrawableSize()
        window.toggleFullScreen(nil) // full screen by default (SPEC §5)

        let link = view.displayLink(target: self, selector: #selector(tick(_:)))
        link.add(to: .main, forMode: .common)
        self.link = link

        let t = Thread { self.decodeLoop() }
        t.name = "glass.decode"
        t.qualityOfService = .userInteractive
        t.start()
    }

    func toggleOverlay() {
        overlay?.isHidden.toggle()
    }

    /// Ends the session (from the UI); the decode thread closes the client.
    func stop(_ reason: String) {
        lock.withLock { if stopReason == nil { stopReason = reason } }
    }

    func windowWillClose(_ notification: Notification) {
        stop("viewer closed")
    }

    private func updateDrawableSize() {
        guard let layer, let view else { return }
        layer.contentsScale = view.window?.backingScaleFactor ?? layer.contentsScale
        let px = view.convertToBacking(view.bounds).size
        if px.width >= 1, px.height >= 1, layer.drawableSize != px { layer.drawableSize = px }
    }

    // MARK: decode thread

    private func decodeLoop() {
        let dec = HEVCDecoder(chroma444: client.is444)
        var reason: String?
        while reason == nil {
            if let r = lock.withLock({ stopReason }) {
                reason = r
                break
            }
            let r = client.waitFrame(timeoutMs: 100) { meta, bytes in
                let t0 = GlassClock.nowUs()
                let pb: CVPixelBuffer?
                do {
                    pb = try dec.decode(bytes)
                } catch {
                    lock.withLock { decodeErrors += 1 }
                    client.requestKeyframe()
                    return
                }
                let t1 = GlassClock.nowUs()
                let capture = client.hostToLocal(meta.capture_ts_us) ?? 0
                lock.withLock {
                    usingHardware = dec.usingHardware
                    for s in [session, recent] {
                        s.frames += 1
                        s.bytes += bytes.count
                        // VideoToolbox time only (as in M1); t0...t1 also covers session setup.
                        s.decode.push(UInt32(min(dec.lastDecodeSeconds * 1e6, Double(UInt32.max))))
                        s.queue.push(span(t0, meta.last_packet_us))
                        if capture > 0 {
                            s.encode.push(meta.encode_us)
                            s.network.push(span(meta.last_packet_us, capture + UInt64(meta.encode_us)))
                        }
                    }
                    guard let pb else { return }
                    if latest != nil { superseded += 1 }
                    latest = (pb, FrameTiming(captureLocal: capture, lastPacket: meta.last_packet_us, decodeEnd: t1))
                }
            }
            if r == .disconnected { reason = "the PC ended the session" }
        }
        let stats = client.stats
        lock.withLock { finalStats = stats }
        client.close()
        DispatchQueue.main.async { self.completed(reason ?? "closed") }
    }

    // MARK: main thread

    @objc private func tick(_ link: CADisplayLink) {
        let item: (CVPixelBuffer, FrameTiming)? = lock.withLock {
            defer { latest = nil }
            return latest
        }
        if let item { present(item.0, item.1) }
        let now = GlassClock.nowUs()
        if now - lastOverlay >= 250_000 {
            lastOverlay = now
            updateOverlay(now)
        }
    }

    private func present(_ pb: CVPixelBuffer, _ t: FrameTiming) {
        guard let drawable = layer.nextDrawable(), let cb = renderer.queue.makeCommandBuffer() else { return }
        renderer.encode(pb, to: drawable.texture, in: cb, fit: true)
        let submitted = GlassClock.nowUs()
        lock.withLock { submittedCount += 1 }
        drawable.addPresentedHandler { [weak self] d in
            guard let self, d.presentedTime > 0 else { return }
            let shown = GlassClock.us(fromMediaTime: d.presentedTime)
            self.lock.withLock {
                for s in [self.session, self.recent] {
                    s.presented += 1
                    s.wait.push(span(submitted, t.decodeEnd))
                    s.present.push(span(shown, submitted))
                    if t.captureLocal > 0 { s.total.push(span(shown, t.captureLocal)) }
                    if self.lastPresentedUs > 0 { s.interval.push(span(shown, self.lastPresentedUs)) }
                }
                self.lastPresentedUs = shown
            }
        }
        cb.present(drawable)
        cb.commit()
    }

    private func updateOverlay(_ now: UInt64) {
        let st = client.stats
        let text: String = lock.withLock {
            let secs = max(Double(now - windowStart) / 1e6, 0.001)
            let r = recent
            func p50(_ s: Stats) -> String { s.summary.count > 0 ? String(format: "%.1f", Double(s.summary.p50_us) / 1e3) : "-" }
            let total = r.total.summary
            let totalLine = total.count > 0
                ? String(format: "capture → screen  p50 %.1f  p99 %.1f ms", Double(total.p50_us) / 1e3, Double(total.p99_us) / 1e3)
                : (st.clock_valid ? "capture → screen  (waiting for frames)" : "capture → screen  (syncing clocks…)")
            let lines = [
                "Glass  \(client.host)  \(streamDesc)",
                String(format: "shown %.0f fps  received %.0f fps  %.1f Mbps", Double(r.presented) / secs, Double(r.frames) / secs, Double(r.bytes) * 8 / secs / 1e6),
                totalLine,
                "  encode \(p50(r.encode)) | network \(p50(r.network)) | queue \(p50(r.queue)) | decode \(p50(r.decode)) | wait \(p50(r.wait)) | present \(p50(r.present))   (p50 ms)",
                "lost \(st.rx.frames_lost)  skipped \(st.rx.frames_skipped)  keyframe req \(st.keyframe_requests)  decode err \(decodeErrors)  RTT \(String(format: "%.1f", Double(st.last_rtt_us) / 1e3)) ms",
                "⌃⌘F full screen · ⌘⇧S hide stats · ⌘W disconnect",
            ]
            if now - windowStart >= 2_000_000 {
                recent.reset()
                windowStart = now
            }
            return lines.joined(separator: "\n")
        }
        overlay.stringValue = text
    }

    private func completed(_ reason: String) {
        guard !finished else { return }
        finished = true
        link?.invalidate()
        link = nil
        window.delegate = nil
        if window.isVisible { window.close() }
        onFinish(report(reason))
    }

    private func report(_ reason: String) -> String {
        lock.withLock {
            let secs = max(Double(GlassClock.nowUs() - started) / 1e6, 0.001)
            let st = finalStats
            let s = session
            var out = [banner, systemInfo()]
            out.append("Live: \(client.host):\(client.port)  \(streamDesc), \(String(format: "%.1f", secs)) s (ended: \(reason))")
            out.append(String(format: "  received %ld frames (%.1f fps), submitted %ld, presented %ld (%.1f fps), superseded %ld, decode errors %ld",
                              s.frames, Double(s.frames) / secs, submittedCount, s.presented, Double(s.presented) / secs, superseded, decodeErrors))
            out.append("  packets \(st.rx.packets) | lost frames \(st.rx.frames_lost) | skipped \(st.rx.frames_skipped) | late \(st.rx.late) | dup \(st.rx.duplicates) | bad \(st.rx.bad) | queue full \(st.queue_full) | keyframe requests \(st.keyframe_requests)")
            out.append(String(format: "  bitrate %.1f Mbps avg | clock sync %@ (best RTT %.2f ms, last %.2f ms)",
                              Double(s.bytes) * 8 / secs / 1e6, st.clock_valid ? "ok" : "NOT SYNCED",
                              Double(st.clock_rtt_us) / 1e3, Double(st.last_rtt_us) / 1e3))
            out.append("  decoder: \(usingHardware ? "hardware" : "SOFTWARE")")
            out.append(s.encode.line("encode (host)"))
            out.append(s.network.line("network"))
            out.append(s.queue.line("queue"))
            out.append(s.decode.line("decode"))
            out.append(s.wait.line("wait for refresh"))
            out.append(s.present.line("present"))
            out.append(s.total.line("capture -> screen"))
            out.append(s.interval.line("present interval"))
            return out.joined(separator: "\n")
        }
    }
}

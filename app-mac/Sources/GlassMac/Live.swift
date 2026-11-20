import AppKit
import Carbon.HIToolbox
import CGlassCore
import Foundation
import GlassKit
import QuartzCore

// Live viewer. Core receives and reassembles on its own threads; a decode
// thread here decodes every frame (P-frames need their predecessors) and keeps
// only the newest picture; the display link presents that picture each refresh.
// LATER(M5): core's adaptive jitter buffer + presenting just ahead of vsync.
//
// M3: mouse and keyboard go to the PC. Every key combination is forwarded
// except Glass's own ⌃⌥⌘ chords (and what macOS keeps: ⌘Tab, ⌘Space,
// Mission Control). The PC's pointer shape is drawn locally as the Mac cursor.

/// Local-clock (µs) timeline of one decoded frame.
struct FrameTiming {
    var captureLocal: UInt64 // 0 until clock sync has a sample
    var lastPacket: UInt64
    var decodeEnd: UInt64
    var inputSeq: UInt32
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
    let input = Stats() // input sent -> first frame captured after it, on screen
    var frames = 0
    var presented = 0
    var bytes = 0

    func reset() {
        for s in [encode, network, queue, decode, wait, present, total, interval, input] { s.reset() }
        frames = 0
        presented = 0
        bytes = 0
    }
}

/// Glass's own shortcuts: ⌃⌥⌘ + key. Everything else goes to the PC.
let glassChord: NSEvent.ModifierFlags = [.control, .option, .command]

/// Whether the Mac keyboard is ISO (e.g. UK): two keys swap codes vs a PC.
func keyboardIsISO() -> Bool {
    Int(KBGetLayoutType(Int16(LMGetKbdType()))) == Int(kKeyboardISO)
}

final class LiveViewer: NSObject, NSWindowDelegate {
    let client: GlassClient
    let renderer: Renderer
    var onFinish: (String) -> Void = { _ in }
    /// ⌘ -> Win, ⌃ -> Ctrl instead of the default ⌘ -> Ctrl, ⌃ -> Win.
    var pcStyleModifiers = UserDefaults.standard.bool(forKey: "pcStyleModifiers")

    private var window: NSWindow!
    private var view: MetalView!
    private var layer: CAMetalLayer!
    private var overlay: NSTextField!
    private var link: CADisplayLink?
    private var keyMonitor: Any?
    private var finished = false
    private let started = GlassClock.nowUs()
    private var windowStart = GlassClock.nowUs()
    private var lastOverlay: UInt64 = 0
    private let iso = keyboardIsISO()

    // Main thread only.
    private var cursorGeneration: UInt32 = 0
    private var cursorShape: (info: GLASS_CursorInfo, rgba: [UInt8])?
    private var keysHeldWithCommand = Set<UInt16>() // macOS drops their keyUp
    private var scrollAccum = (dy: 0.0, dx: 0.0)
    private var lastPoint: (x: UInt16, y: UInt16) = (32768, 32768)

    // Shared by the decode thread, presented handlers and main; guarded by `lock`.
    private let lock = NSLock()
    private var latest: (CVPixelBuffer, FrameTiming)?
    private var stopReason: String?
    private var superseded = 0
    private var submittedCount = 0 // handed to Core Animation (presented = confirmed on screen)
    private var decodeErrors = 0
    private var usingHardware = false
    private var lastPresentedUs: UInt64 = 0
    private var lastInputMeasured: UInt32 = 0
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
        window.acceptsMouseMovedEvents = true
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
        view.onMouse = { [weak self] e in self?.handleMouse(e) }

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
        window.makeFirstResponder(view)
        NSApp.activate(ignoringOtherApps: true)
        updateDrawableSize()
        window.toggleFullScreen(nil) // full screen by default (SPEC §5)

        // Keys go through a local monitor: it sees ⌘ combinations (and their
        // keyUps) before the menus do.
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp, .flagsChanged]) { [weak self] e in
            guard let self else { return e }
            return self.handleKey(e)
        }

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

    /// Switching away (⌘Tab, another window): let go of everything on the PC.
    func windowDidResignKey(_ notification: Notification) {
        keysHeldWithCommand.removeAll()
        client.releaseAll()
    }

    private func updateDrawableSize() {
        guard let layer, let view else { return }
        layer.contentsScale = view.window?.backingScaleFactor ?? layer.contentsScale
        let px = view.convertToBacking(view.bounds).size
        if px.width >= 1, px.height >= 1, layer.drawableSize != px { layer.drawableSize = px }
        // Picture rect in view points, for the cursor area.
        let vp = viewport
        let s = Double(max(layer.contentsScale, 1)), H = Double(px.height)
        view.cursorArea = NSRect(x: vp.originX / s, y: (H - vp.originY - vp.height) / s,
                                 width: vp.width / s, height: vp.height / s)
        applyCursor()
    }

    private var viewport: MTLViewport {
        Renderer.fitViewport(src: CGSize(width: Int(client.info.width), height: Int(client.info.height)),
                             dst: layer?.drawableSize ?? .zero)
    }

    // MARK: input (main thread)

    private func handleMouse(_ e: NSEvent) {
        let p = view.convert(e.locationInWindow, from: nil)
        let s = layer.contentsScale
        let px = CGPoint(x: p.x * s, y: (view.bounds.height - p.y) * s)
        let (x, y) = Renderer.videoPoint(px, in: viewport)
        lastPoint = (x, y)
        switch e.type {
        case .mouseMoved, .leftMouseDragged, .rightMouseDragged, .otherMouseDragged:
            client.move(x: x, y: y)
        case .leftMouseDown, .leftMouseUp:
            // ⌃-click is a right-click on a Mac; on the PC it's Win+click, so don't translate.
            client.button(.left, down: e.type == .leftMouseDown, x: x, y: y)
        case .rightMouseDown, .rightMouseUp:
            client.button(.right, down: e.type == .rightMouseDown, x: x, y: y)
        case .otherMouseDown, .otherMouseUp:
            let buttons: [Int: GlassButton] = [2: .middle, 3: .back, 4: .forward]
            if let b = buttons[e.buttonNumber] { client.button(b, down: e.type == .otherMouseDown, x: x, y: y) }
        case .scrollWheel:
            // Precise (trackpad) deltas are points; wheel deltas are lines.
            // macOS has already applied the user's "natural scrolling" choice.
            let k = e.hasPreciseScrollingDeltas ? 6.0 : 40.0
            scrollAccum.dy += Double(e.scrollingDeltaY) * k
            scrollAccum.dx -= Double(e.scrollingDeltaX) * k
            let dy = scrollAccum.dy.rounded(.towardZero), dx = scrollAccum.dx.rounded(.towardZero)
            scrollAccum.dy -= dy
            scrollAccum.dx -= dx
            if dy != 0 || dx != 0 {
                client.scroll(dy: Int16(clamping: Int(dy)), dx: Int16(clamping: Int(dx)))
            }
        default:
            break
        }
    }

    /// Returns nil when the event went to the PC.
    private func handleKey(_ e: NSEvent) -> NSEvent? {
        guard e.window === window, !finished else { return e }
        let mods = e.modifierFlags.intersection(.deviceIndependentFlagsMask)
        switch e.type {
        case .keyDown, .keyUp:
            if mods.isSuperset(of: glassChord) { return e } // Glass menu shortcut
            let hid = GlassClient.hid(forMacKey: e.keyCode, iso: iso, pcStyle: pcStyleModifiers)
            guard hid != 0 else { return nil }
            let down = e.type == .keyDown
            if down, mods.contains(.command) { keysHeldWithCommand.insert(e.keyCode) }
            if !down { keysHeldWithCommand.remove(e.keyCode) }
            client.key(hid, down: down) // repeats arrive as further keyDowns
            return nil
        case .flagsChanged:
            flagsChanged(e)
            return nil
        default:
            return e
        }
    }

    private func flagsChanged(_ e: NSEvent) {
        let raw = e.modifierFlags.rawValue
        // Device-dependent bits tell left from right (IOLLEvent.h NX_DEVICE*).
        let bits: [UInt16: UInt] = [0x3B: 0x01, 0x38: 0x02, 0x3C: 0x04, 0x37: 0x08,
                                    0x36: 0x10, 0x3A: 0x20, 0x3D: 0x40, 0x3E: 0x2000]
        let code = e.keyCode
        if code == 0x39 { // Caps Lock: one event per press; PC toggles on each press
            let hid = GlassClient.hid(forMacKey: code, iso: iso, pcStyle: pcStyleModifiers)
            client.key(hid, down: true)
            client.key(hid, down: false)
            return
        }
        guard let bit = bits[code] else { return } // fn etc.
        let down = raw & bit != 0
        let hid = GlassClient.hid(forMacKey: code, iso: iso, pcStyle: pcStyleModifiers)
        client.key(hid, down: down)
        // macOS doesn't deliver keyUp for keys released while ⌘ was held.
        if !down, code == 0x37 || code == 0x36, !e.modifierFlags.contains(.command) {
            for k in keysHeldWithCommand {
                client.key(GlassClient.hid(forMacKey: k, iso: iso, pcStyle: pcStyleModifiers), down: false)
            }
            keysHeldWithCommand.removeAll()
        }
    }

    // MARK: pointer

    private func pollCursor() {
        let (info, rgba) = client.cursor(seen: cursorGeneration)
        guard info.generation != cursorGeneration else { return }
        cursorGeneration = info.generation
        if let rgba { cursorShape = (info, rgba) } else if var s = cursorShape { s.info.visible = info.visible; cursorShape = s }
        applyCursor()
    }

    private func applyCursor() {
        guard let view, let shape = cursorShape else { return }
        guard shape.info.visible, shape.info.width > 0 else {
            view.cursor = Self.hiddenCursor
            return
        }
        // Same scale as the picture: video px -> drawable px -> view points.
        let vp = viewport
        let k = vp.width / Double(max(client.info.width, 1)) / Double(max(layer.contentsScale, 1))
        let w = Int(shape.info.width), h = Int(shape.info.height)
        guard let provider = CGDataProvider(data: Data(shape.rgba) as CFData),
              let img = CGImage(width: w, height: h, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: w * 4,
                                space: CGColorSpaceCreateDeviceRGB(),
                                bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue),
                                provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
        else { return }
        let size = NSSize(width: Double(w) * k, height: Double(h) * k)
        let hot = NSPoint(x: Double(shape.info.hot_x) * k, y: Double(shape.info.hot_y) * k)
        view.cursor = NSCursor(image: NSImage(cgImage: img, size: size), hotSpot: hot)
    }

    private static let hiddenCursor = NSCursor(image: NSImage(size: NSSize(width: 1, height: 1)), hotSpot: .zero)

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
                    latest = (pb, FrameTiming(captureLocal: capture, lastPacket: meta.last_packet_us,
                                              decodeEnd: t1, inputSeq: meta.input_seq))
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
        pollCursor()
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
        let client = self.client
        drawable.addPresentedHandler { [weak self] d in
            guard let self, d.presentedTime > 0 else { return }
            let shown = GlassClock.us(fromMediaTime: d.presentedTime)
            // First frame showing a newer input: input -> screen.
            let sentAt = t.inputSeq != 0 ? client.inputSentUs(t.inputSeq) : nil
            self.lock.withLock {
                let newInput = t.inputSeq != self.lastInputMeasured && sentAt != nil
                if newInput { self.lastInputMeasured = t.inputSeq }
                for s in [self.session, self.recent] {
                    s.presented += 1
                    s.wait.push(span(submitted, t.decodeEnd))
                    s.present.push(span(shown, submitted))
                    if t.captureLocal > 0 { s.total.push(span(shown, t.captureLocal)) }
                    if self.lastPresentedUs > 0 { s.interval.push(span(shown, self.lastPresentedUs)) }
                    if newInput, let sentAt { s.input.push(span(shown, sentAt)) }
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
            let inp = r.input.summary
            let inputLine = inp.count > 0
                ? String(format: "input → screen    p50 %.1f  p99 %.1f ms", Double(inp.p50_us) / 1e3, Double(inp.p99_us) / 1e3)
                : "input → screen    (move or type to measure)"
            let lines = [
                "Glass  \(client.host)  \(streamDesc)  [\(pcStyleModifiers ? "⌘→Win ⌃→Ctrl" : "⌘→Ctrl ⌃→Win")]",
                String(format: "shown %.0f fps  received %.0f fps  %.1f Mbps", Double(r.presented) / secs, Double(r.frames) / secs, Double(r.bytes) * 8 / secs / 1e6),
                totalLine,
                inputLine,
                "  encode \(p50(r.encode)) | network \(p50(r.network)) | queue \(p50(r.queue)) | decode \(p50(r.decode)) | wait \(p50(r.wait)) | present \(p50(r.present))   (p50 ms)",
                "lost \(st.rx.frames_lost)  skipped \(st.rx.frames_skipped)  keyframe req \(st.keyframe_requests)  decode err \(decodeErrors)  RTT \(String(format: "%.1f", Double(st.last_rtt_us) / 1e3)) ms  input events \(st.input_events)",
                "⌃⌥⌘F full screen · ⌃⌥⌘S hide stats · ⌃⌥⌘W disconnect · ⌃⌥⌘Q quit",
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
        if let m = keyMonitor { NSEvent.removeMonitor(m) }
        keyMonitor = nil
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
            out.append("  input: \(st.input_events) events in \(st.input_packets) packets | keyboard \(iso ? "ISO" : "ANSI"), \(pcStyleModifiers ? "PC-style (⌘→Win)" : "Mac-style (⌘→Ctrl)")")
            out.append("  decoder: \(usingHardware ? "hardware" : "SOFTWARE")")
            out.append(s.encode.line("encode (host)"))
            out.append(s.network.line("network"))
            out.append(s.queue.line("queue"))
            out.append(s.decode.line("decode"))
            out.append(s.wait.line("wait for refresh"))
            out.append(s.present.line("present"))
            out.append(s.total.line("capture -> screen"))
            out.append(s.input.line("input -> screen"))
            out.append(s.interval.line("present interval"))
            return out.joined(separator: "\n")
        }
    }
}

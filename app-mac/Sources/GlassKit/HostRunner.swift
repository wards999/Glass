import AppKit
import CGlassCore
import CoreVideo
import Foundation
import VideoToolbox

/// Runs the Mac as a host: listens, hands pairing to core, and for each
/// paired viewer captures, encodes and sends a display while injecting its
/// input. One viewer at a time.
///
/// View modes (M3c, `GLASS_MODE_*`, the viewer's choice): the Mac's own
/// screen, or a virtual display at the viewer's pixel size (like now / more
/// space / larger). 4:4:4 when the viewer can decode it.
///
/// Threads: `accept` loop (and each session's control loop) on its own
/// thread; capture, encode and every session call on the session's serial
/// queue (the encoder's output runs on VideoToolbox's thread while that queue
/// waits); pointer polling on the main thread.
public final class HostRunner {
    public struct Options {
        public var port: UInt16 = GlassClient.defaultPort
        public var fixedPin: String?
        /// Synthetic frames and no input injection (CI; needs no permissions).
        public var testPattern = false
        public var mbps = 40
        /// Send 4:4:4 to viewers that can decode it.
        public var allow444 = true
        /// End each session after this long (CLI tests).
        public var seconds: Double?
        /// Share the clipboard with the viewer, both ways (M5c).
        public var clipboard = true
        public init() {}
    }

    /// Measure sharpness (PSNR of encoded frames against their source)
    /// while the clarity test card is open. Costs a decode per frame.
    public static var measureQuality: Bool {
        get { qualityLock.withLock { _measure } }
        set { qualityLock.withLock { _measure = newValue } }
    }
    private static let qualityLock = NSLock()
    private static var _measure = false

    public let options: Options
    /// The Mac's own main display (what `MODE_HOST_SCREEN` streams).
    public let display: HostDisplay
    public let host: GlassHost
    /// Main thread: pairing PINs, refusals and the like.
    public var onEvent: (GlassHostEvent) -> Void = { _ in }
    /// Main thread: a viewer connected (name, address).
    public var onConnected: (String, String) -> Void = { _, _ in }
    /// Main thread: what is being streamed changed (a line for the UI).
    public var onStreamChanged: (String) -> Void = { _ in }
    /// Main thread: a session ended; its report.
    public var onSessionEnd: (String) -> Void = { _ in }

    private var thread: Thread?
    private let stopFlag = NSLock()
    private var stopped = false
    private let banner: String

    public init(device: GlassDevice, options: Options, banner: String) throws {
        self.options = options
        self.banner = banner
        let d = HostDisplay.main()
        display = options.testPattern
            ? HostDisplay(id: d.id, width: 1280, height: 720, fps: 60, bounds: d.bounds)
            : d
        var forward: ((GlassHostEvent) -> Void)?
        host = try GlassHost(device: device, port: options.port, fixedPin: options.fixedPin) { e in
            DispatchQueue.main.async { forward?(e) }
        }
        forward = { [weak self] e in self?.onEvent(e) }
    }

    /// What `Welcome` announces: the Mac's own screen, 4:2:0. The session
    /// switches right after if the viewer asked for something else.
    public var info: GLASS_StreamInfo {
        GLASS_StreamInfo(codec: UInt8(GLASS_CODEC_HEVC), chroma: UInt8(GLASS_CHROMA_420),
                         width: UInt16(display.width), height: UInt16(display.height), fps: UInt16(display.fps),
                         mode: UInt8(GLASS_MODE_HOST_SCREEN))
    }

    fileprivate var isStopped: Bool { stopFlag.withLock { stopped } }

    public func start() {
        let t = Thread { [weak self] in self?.acceptLoop() }
        t.name = "glass.accept"
        t.qualityOfService = .userInitiated
        thread = t
        t.start()
    }

    /// Ends the current session only; sharing goes on.
    public func endSession() {
        stopFlag.withLock { endRequested = true }
    }

    fileprivate func takeEndRequest() -> Bool {
        stopFlag.withLock {
            defer { endRequested = false }
            return endRequested
        }
    }

    private var endRequested = false

    /// Ends any session and stops listening.
    public func stop() {
        stopFlag.withLock { stopped = true }
        host.stop()
    }

    private func acceptLoop() {
        while !isStopped {
            let input = MacInputInjector(display: display, dryRun: options.testPattern)
            switch host.accept(info: info, input: input) {
            case .session(let s):
                let name = s.peerName, addr = s.peerAddress
                DispatchQueue.main.async { self.onConnected(name, addr) }
                let report = HostSessionRun(runner: self, session: s, input: input).run()
                DispatchQueue.main.async { self.onSessionEnd(report) }
            case .handled, .failed:
                continue
            case .stopped:
                return
            }
        }
    }

    fileprivate var bannerText: String { banner }
}

/// Stats that live across view-mode changes within one session.
private final class Totals {
    let encode = Stats(), delivery = Stats(), vtEncode = Stats(), send = Stats(), interval = Stats()
    var frames = 0, idrs = 0, notSent = 0, idleIDRs = 0, bytes = 0
    /// Captured frames dropped because a newer one arrived while encoding.
    var stale = 0
    var frameId: UInt32 = 0
    var psnrY: [Double] = [], psnrC: [Double] = []
    var producedChroma: Int?
    var hardware = true, lowLatency = false
}

/// One viewer's session: view modes, the pipeline, the report.
private final class HostSessionRun {
    let runner: HostRunner
    let s: GlassHostSession
    let input: MacInputInjector
    let q = DispatchQueue(label: "glass.session", qos: .userInteractive)
    let totals = Totals()
    var pipeline: Pipeline?
    var virtual: VirtualDisplay?
    var current: GLASS_StreamInfo
    /// The encoder couldn't make 4:4:4 here: don't try again this session.
    var no444 = false
    var virtualDisp: HostDisplay?
    var virtualNote = ""
    var history: [String] = []
    var clipSummary = "off"
    let failLock = NSLock()
    var failure: String?
    var cursorWatch: CursorWatch // main thread
    var serial: UInt32 = 0 // q

    init(runner: HostRunner, session: GlassHostSession, input: MacInputInjector) {
        self.runner = runner
        s = session
        self.input = input
        current = runner.info
        cursorWatch = CursorWatch(display: runner.display)
    }

    func fail(_ why: String) { failLock.withLock { if failure == nil { failure = why } } }

    static func modeName(_ m: UInt8) -> String {
        switch Int32(m) {
        case GLASS_MODE_FIT: return "Fit"
        case GLASS_MODE_FIT_MORE_SPACE: return "More space"
        case GLASS_MODE_FIT_LARGER: return "Larger"
        default: return "Mac's screen"
        }
    }

    /// Position in the PC's Ctrl+Alt+Shift+R cycle (Fit is 1).
    static func viewNumber(_ m: UInt8) -> Int { Int(m) == 0 ? 4 : Int(m) }

    /// Sets up capture and encode for `mode`; tells the viewer if the stream
    /// changes. On the accept thread, with no pipeline running.
    func configure(mode: UInt8, allow444: Bool) {
        pipeline?.stop()
        pipeline = nil
        // Only the colour changes: keep the virtual display (re-creating it
        // re-arranges the Mac's windows for nothing).
        let reuse = virtual != nil && mode == current.mode
        if !reuse {
            virtual?.destroy()
            virtual = nil
        }
        let o = runner.options
        let viewer = q.sync { s.viewer }
        var disp = runner.display
        var actual = UInt8(GLASS_MODE_HOST_SCREEN)
        var note = ""
        if Int32(mode) != GLASS_MODE_HOST_SCREEN && Int32(mode) < GLASS_MODE_COUNT {
            // The viewer's screen size; ours if it didn't say.
            let vw = (viewer.display_w > 0 ? Int(viewer.display_w) : runner.display.width) & ~1
            let vh = (viewer.display_h > 0 ? Int(viewer.display_h) : runner.display.height) & ~1
            func even(_ v: Double) -> Int { max(2, Int(v.rounded()) & ~1) }
            var pw = vw, ph = vh, hidpi = false
            if Int32(mode) == GLASS_MODE_FIT {
                // About as big as on the Mac's own screen.
                pw = even(Double(vw) / 1.5)
                ph = even(Double(vh) / 1.5)
                hidpi = true
            } else if Int32(mode) == GLASS_MODE_FIT_LARGER {
                pw = even(Double(vw) / 2)
                ph = even(Double(vh) / 2)
                hidpi = true
            }
            if o.testPattern {
                disp = HostDisplay(id: disp.id, width: vw, height: vh, fps: 60, bounds: disp.bounds)
                actual = mode
            } else if reuse, let vd = virtualDisp {
                disp = vd
                actual = mode
                note = virtualNote
            } else if VirtualDisplay.available,
                      let v = VirtualDisplay(name: "Glass \(vw)x\(vh)", pointsW: pw, pointsH: ph, hiDPI: hidpi,
                                             hz: 60, mirror: runner.display.id) {
                virtual = v
                // Captured at the viewer's pixel size: 1:1 on its screen.
                disp = HostDisplay(id: v.id, width: vw, height: vh, fps: 60, bounds: CGDisplayBounds(v.id))
                actual = mode
                note = " (virtual display, looks like \(pw)x\(ph)\(hidpi ? " HiDPI" : ""))"
                virtualDisp = disp
                virtualNote = note
            } else {
                note = " (virtual display unavailable: showing the Mac's screen)"
            }
        }
        let use444 = allow444 && !no444 && o.allow444 && (viewer.caps & UInt8(GLASS_CAP_HEVC_444)) != 0
        let info = GLASS_StreamInfo(codec: UInt8(GLASS_CODEC_HEVC),
                                    chroma: UInt8(use444 ? GLASS_CHROMA_444 : GLASS_CHROMA_420),
                                    width: UInt16(disp.width), height: UInt16(disp.height),
                                    fps: UInt16(disp.fps), mode: actual)
        input.setDisplay(disp)
        let watch = CursorWatch(display: disp)
        DispatchQueue.main.sync { cursorWatch = watch }
        let line = "\(Self.modeName(actual)) \(disp.width)x\(disp.height) \(use444 ? "4:4:4" : "4:2:0")\(note)"
        history.append(line)
        let r = runner
        // Short, easy name for the test card banner and the Share window.
        let short = "View \(Self.viewNumber(actual)) of 4: \(Self.modeName(actual))  ·  \(disp.width)x\(disp.height)  ·  \(use444 ? "full colour 4:4:4" : "4:2:0")"
        DispatchQueue.main.async { r.onStreamChanged(short) }
        let changed = info.width != current.width || info.height != current.height
            || info.chroma != current.chroma || info.mode != current.mode
        if changed { q.sync { _ = s.sendStreamChange(info) } }
        current = info
        do {
            let p = try Pipeline(run: self, display: disp, chroma444: use444)
            pipeline = p
            p.start()
        } catch {
            fail("couldn't start the encoder: \(error)")
        }
    }

    func run() -> String {
        let started = GlassClock.nowUs()
        let viewer = q.sync { s.viewer }
        // 4:4:4 from the start unless the viewer prefers 4:2:0.
        configure(mode: viewer.mode, allow444: viewer.caps & UInt8(GLASS_CAP_PREFER_420) == 0)

        let cursorTimer = DispatchSource.makeTimerSource(queue: .main)
        cursorTimer.schedule(deadline: .now(), repeating: .milliseconds(33))
        cursorTimer.setEventHandler { [self] in
            guard let shape = cursorWatch.poll() else { return }
            q.async { [self] in
                serial &+= 1
                s.sendCursor(serial: serial, width: shape.width, height: shape.height,
                             hotX: shape.hotX, hotY: shape.hotY, rgba: shape.rgba)
            }
        }
        cursorTimer.resume()

        // The clipboard, both ways (M5c): pasteboard work on the main
        // thread, session calls on the session queue.
        var clip: ClipboardSync?
        var clipTimer: DispatchSourceTimer?
        if runner.options.clipboard {
            let sync = ClipboardSync(sendCurrent: false) { [self] item in q.async { [self] in s.sendClipboard(item) } }
            clip = sync
            let t = DispatchSource.makeTimerSource(queue: .main)
            t.schedule(deadline: .now(), repeating: .milliseconds(150))
            t.setEventHandler { [self] in
                sync.poll()
                q.async { [self] in
                    if let item = s.takeClipboard() { DispatchQueue.main.async { sync.put(item) } }
                }
            }
            t.resume()
            clipTimer = t
        }

        let deadline = runner.options.seconds.map { started + UInt64($0 * 1e6) }
        var reason = "viewer left"
        while true {
            if runner.isStopped { reason = "sharing stopped"; break }
            if runner.takeEndRequest() { reason = "ended on this Mac"; break }
            if !q.sync(execute: { s.connected }) { break }
            if let f = failLock.withLock({ failure }) { reason = f; break }
            if let deadline, GlassClock.nowUs() >= deadline { reason = "--seconds elapsed"; break }
            if let req = q.sync(execute: { s.takeModeRequest() }) {
                configure(mode: req.mode, allow444: req.allow444)
            }
            // Asked for 4:4:4 but the encoder made 4:2:0: say so and switch,
            // rather than leave the viewer failing to decode.
            if current.chroma == UInt8(GLASS_CHROMA_444),
               let produced = q.sync(execute: { pipeline?.encoder.producedChroma }), produced != 3 {
                no444 = true
                configure(mode: current.mode, allow444: false)
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        cursorTimer.cancel()
        clipTimer?.cancel()
        clipSummary = DispatchQueue.main.sync { clip.map { $0.summary } } ?? "off"
        pipeline?.stop()
        pipeline = nil
        virtual?.destroy()
        virtual = nil
        let counters: GLASS_HostCounters = q.sync {
            let c = s.counters
            s.close()
            return c
        }
        return report(started: started, reason: reason, counters: counters)
    }

    private func report(started: UInt64, reason: String, counters: GLASS_HostCounters) -> String {
        let o = runner.options
        let t = totals
        let d = runner.display
        let secs = max(Double(GlassClock.nowUs() - started) / 1e6, 0.001)
        let inp = counters.input
        let chroma = [1: "4:2:0", 2: "4:2:2", 3: "4:4:4"][t.producedChroma ?? 0] ?? "?"
        var out = [runner.bannerText]
        out.append("Host: Mac display \(d.width)x\(d.height) @ \(d.fps) Hz\(o.testPattern ? " (test pattern)" : "") | HEVC \(o.mbps) Mbps | encoder \(t.hardware ? "hardware" : "SOFTWARE")\(t.lowLatency ? ", low-latency rate control" : "") | encoded chroma \(chroma)")
        out.append("Session with \u{201C}\(s.peerName)\u{201D} (\(s.peerAddress)) ended after \(String(format: "%.1f", secs)) s (\(reason))")
        out.append("  streamed: " + history.joined(separator: " -> "))
        if no444 { out.append("  NOTE: the encoder produced 4:2:0 when asked for 4:4:4, so this session switched to 4:2:0.") }
        out.append(String(format: "  frames sent %ld (%.1f fps), IDRs %ld (idle re-sends %ld), viewer keyframe requests %ld, not sent (viewer not ready) %ld",
                          t.frames, Double(t.frames) / secs, t.idrs, t.idleIDRs, Int(counters.keyframe_requests), t.notSent))
        out.append(String(format: "  bitrate %.1f Mbps avg | %ld packets", Double(t.bytes) * 8 / secs / 1e6, Int(counters.packets_sent)))
        out.append(t.encode.line("encode"))
        out.append(t.delivery.line("  capture delivery"))
        out.append(t.vtEncode.line("  VideoToolbox"))
        out.append(t.send.line("send (packetise)"))
        out.append(t.interval.line("capture interval"))
        out.append("  stale frames skipped (encoder still busy, newest frame wins): \(t.stale)")
        if !t.psnrY.isEmpty {
            func p50(_ a: [Double]) -> Double { a.sorted()[a.count / 2] }
            let good = t.psnrY.filter { $0 >= 45 }.count
            out.append(String(format: "  sharpness (PSNR vs source, %ld frames): luma p50 %.1f dB min %.1f | chroma p50 %.1f dB min %.1f | luma >= 45 dB in %.0f%%",
                              t.psnrY.count, p50(t.psnrY), t.psnrY.min() ?? 0, p50(t.psnrC), t.psnrC.min() ?? 0,
                              Double(good) * 100 / Double(t.psnrY.count)))
        }
        out.append("  input: \(inp.packets) packets | \(inp.keys) keys, \(inp.clicks) clicks, \(inp.scrolls) scrolls, \(inp.moves) moves | repaired \(inp.repaired)\(o.testPattern ? " (test pattern: not injected)" : "")")
        out.append("  rejected UDP datagrams (failed decryption or replays): \(counters.rejected_datagrams)")
        out.append("  clipboard: \(clipSummary)")
        if !o.testPattern && !HostPermissions.accessibility {
            out.append("  NOTE: Accessibility is off for Glass, so the viewer's mouse and keyboard did nothing.")
        }
        return out.joined(separator: "\n")
    }
}

/// Capture -> (4:4:4 conversion) -> encode -> send for one stream
/// configuration. Everything here runs on the session's queue.
private final class Pipeline {
    unowned let run: HostSessionRun
    let source: FrameSource
    let encoder: HEVCEncoder
    let converter: Converter?
    var probe: QualityProbe?
    let idle: DispatchSourceTimer
    var lastFrame: CVPixelBuffer? // under lock
    var lastCapture: UInt64 = 0 // under lock
    var forceKey = true // a new stream starts with an IDR
    var count = 0
    /// Capture runs here; encoding on the session queue. If the encoder is
    /// still busy when frames arrive, only the newest waits: the picture is
    /// never more than one frame behind (m3c-test4: lag grew while dragging).
    let captureQ = DispatchQueue(label: "glass.capture", qos: .userInteractive)
    /// The sharpness probe decodes and compares off the hot path.
    let probeQ = DispatchQueue(label: "glass.probe", qos: .utility)
    private let lock = NSLock()
    private var pending: (CVPixelBuffer, UInt64)?
    private var busy = false

    init(run: HostSessionRun, display: HostDisplay, chroma444: Bool) throws {
        self.run = run
        let o = run.runner.options
        source = o.testPattern
            ? TestPatternSource(width: display.width, height: display.height, fps: display.fps, queue: captureQ)
            : ScreenSource(display: display, queue: captureQ, bgra: chroma444)
        encoder = try HEVCEncoder(.init(width: display.width, height: display.height, fps: display.fps,
                                        bitrate: o.mbps * 1_000_000, chroma444: chroma444))
        converter = chroma444 ? Converter(width: display.width, height: display.height) : nil
        idle = DispatchSource.makeTimerSource(queue: run.q)
        run.totals.hardware = encoder.usingHardware
        run.totals.lowLatency = encoder.lowLatencyRateControl
    }

    func start() {
        let t = run.totals
        source.start({ [self] pb, captureUs in
            lock.lock()
            if lastCapture > 0, captureUs > lastCapture { t.interval.push(UInt32(clamping: captureUs - lastCapture)) }
            lastCapture = captureUs
            lastFrame = pb
            if pending != nil { t.stale += 1 }
            pending = (pb, captureUs)
            let kick = !busy
            busy = true
            lock.unlock()
            if kick { run.q.async { [self] in drain() } }
        }, { [weak self] error in
            if let error { self?.run.fail("screen capture stopped: \(error.localizedDescription)") }
        })
        // Idle screen: ScreenCaptureKit sends nothing new, so answer keyframe
        // requests (and a new stream's first IDR) with the last picture.
        idle.schedule(deadline: .now() + .milliseconds(50), repeating: .milliseconds(50))
        idle.setEventHandler { [weak self] in
            guard let self else { return }
            let (frame, last) = self.lock.withLock { (self.lastFrame, self.lastCapture) }
            guard let pb = frame, GlassClock.nowUs() &- last > 50_000 else { return }
            if self.forceKey || self.run.s.takeKeyframeRequest() {
                t.idleIDRs += 1
                let now = GlassClock.nowUs()
                self.lock.withLock { self.lastCapture = now }
                self.encodeAndSend(pb, now, force: true)
            }
        }
        idle.resume()
    }

    /// Encodes the newest waiting frame until none is left (session queue).
    private func drain() {
        while true {
            lock.lock()
            guard let (pb, ts) = pending else {
                busy = false
                lock.unlock()
                return
            }
            pending = nil
            lock.unlock()
            encodeAndSend(pb, ts, force: false)
        }
    }

    /// Stops capture and waits until nothing of this pipeline runs.
    func stop() {
        source.stop()
        idle.cancel()
        captureQ.sync {}
        run.q.sync {}
        probeQ.sync {}
        lock.withLock {
            lastFrame = nil
            pending = nil
        }
    }

    private func encodeAndSend(_ pb: CVPixelBuffer, _ captureUs: UInt64, force: Bool) {
        let t = run.totals
        let s = run.s
        let arrival = GlassClock.nowUs()
        // Frame times can be a hair ahead of our clock: never wrap.
        func since(_ a: UInt64, _ b: UInt64) -> UInt32 { a > b ? UInt32(clamping: a - b) : 0 }
        t.delivery.push(since(arrival, captureUs))
        let key = s.takeKeyframeRequest() || force || forceKey
        forceKey = false
        let inputSeq = s.inputSeq
        var frame = pb
        if let converter {
            guard let c = converter.convert(pb) else {
                run.fail("4:4:4 conversion failed")
                return
            }
            frame = c
        }
        // Sharpness: decode what we send and compare (test card open).
        if HostRunner.measureQuality {
            if probe == nil { probe = QualityProbe(chroma444: converter != nil) }
        } else {
            probe = nil
        }
        count += 1
        let measure = probe != nil && (key || count % 10 == 0)
        do {
            try encoder.encode(frame, ptsUs: captureUs, keyframe: key) { data, isKey, _ in
                let now = GlassClock.nowUs()
                t.frameId &+= 1
                let f = GLASS_OutFrame(frame_id: t.frameId, flags: isKey ? 1 : 0, capture_ts_us: captureUs,
                                       encode_us: since(now, captureUs), input_seq: inputSeq)
                let r = s.send(f, data)
                let sent = GlassClock.nowUs()
                t.encode.push(since(now, captureUs))
                t.vtEncode.push(UInt32(clamping: now &- arrival))
                t.send.push(UInt32(clamping: sent &- now))
                if r == 1 {
                    t.frames += 1
                    t.bytes += data.count
                    if isKey { t.idrs += 1 }
                } else if r == 0 {
                    t.notSent += 1
                } else {
                    run.fail("sending failed")
                }
                if let probe, let base = data.baseAddress {
                    // Decode and compare on the probe queue (a copy: `data`
                    // is only valid here).
                    let copy = Data(bytes: base, count: data.count)
                    let source = frame
                    probeQ.async {
                        let p = copy.withUnsafeBytes { probe.feed($0, source: source, measure: measure) }
                        if let p {
                            t.psnrY.append(p.y)
                            t.psnrC.append(p.c)
                        }
                    }
                }
            }
            if t.producedChroma == nil { t.producedChroma = encoder.producedChroma }
        } catch {
            run.fail("encode failed: \(error)")
        }
    }
}

/// BGRA (or 4:2:0) -> 4:4:4 video-range YCbCr, BT.709, on the GPU.
final class Converter {
    private var session: VTPixelTransferSession?
    private var pool: CVPixelBufferPool?

    init?(width: Int, height: Int) {
        var s: VTPixelTransferSession?
        guard VTPixelTransferSessionCreate(allocator: kCFAllocatorDefault, pixelTransferSessionOut: &s) == noErr, let s else { return nil }
        VTSessionSetProperty(s, key: kVTPixelTransferPropertyKey_DestinationYCbCrMatrix, value: kCVImageBufferYCbCrMatrix_ITU_R_709_2)
        VTSessionSetProperty(s, key: kVTPixelTransferPropertyKey_DestinationColorPrimaries, value: kCVImageBufferColorPrimaries_ITU_R_709_2)
        VTSessionSetProperty(s, key: kVTPixelTransferPropertyKey_DestinationTransferFunction, value: kCVImageBufferTransferFunction_ITU_R_709_2)
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferWidthKey: width,
            kCVPixelBufferHeightKey: height,
            kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any]() as CFDictionary,
        ]
        guard CVPixelBufferPoolCreate(kCFAllocatorDefault, nil, attrs as CFDictionary, &pool) == kCVReturnSuccess else {
            VTPixelTransferSessionInvalidate(s)
            return nil
        }
        session = s
    }

    deinit { if let s = session { VTPixelTransferSessionInvalidate(s) } }

    func convert(_ src: CVPixelBuffer) -> CVPixelBuffer? {
        guard let session, let pool else { return nil }
        var dst: CVPixelBuffer?
        guard CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &dst) == kCVReturnSuccess, let dst,
              VTPixelTransferSessionTransferImage(session, from: src, to: dst) == noErr else { return nil }
        return dst
    }
}

/// Decodes the host's own output and compares it with the encoder's input
/// (same pixel format): PSNR of luma and chroma, every other row.
final class QualityProbe {
    private let decoder: HEVCDecoder

    init(chroma444: Bool) { decoder = HEVCDecoder(chroma444: chroma444) }

    /// Decodes `annexB` (every frame: later frames need earlier ones); with
    /// `measure`, returns its PSNR against `source`.
    func feed(_ annexB: UnsafeRawBufferPointer, source: CVPixelBuffer, measure: Bool) -> (y: Double, c: Double)? {
        guard let out = try? decoder.decode(annexB), measure else { return nil }
        return Self.psnr(source, out)
    }

    static func psnr(_ a: CVPixelBuffer, _ b: CVPixelBuffer) -> (y: Double, c: Double)? {
        guard CVPixelBufferGetPlaneCount(a) == 2, CVPixelBufferGetPlaneCount(b) == 2,
              CVPixelBufferGetWidth(a) == CVPixelBufferGetWidth(b),
              CVPixelBufferGetHeight(a) == CVPixelBufferGetHeight(b) else { return nil }
        CVPixelBufferLockBaseAddress(a, .readOnly)
        CVPixelBufferLockBaseAddress(b, .readOnly)
        defer {
            CVPixelBufferUnlockBaseAddress(a, .readOnly)
            CVPixelBufferUnlockBaseAddress(b, .readOnly)
        }
        func plane(_ i: Int) -> Double? {
            guard let pa = CVPixelBufferGetBaseAddressOfPlane(a, i)?.assumingMemoryBound(to: UInt8.self),
                  let pb = CVPixelBufferGetBaseAddressOfPlane(b, i)?.assumingMemoryBound(to: UInt8.self) else { return nil }
            let rowA = CVPixelBufferGetBytesPerRowOfPlane(a, i), rowB = CVPixelBufferGetBytesPerRowOfPlane(b, i)
            let w = CVPixelBufferGetWidthOfPlane(a, i) * (i == 0 ? 1 : 2) // CbCr interleaved
            let h = CVPixelBufferGetHeightOfPlane(a, i)
            var se: UInt64 = 0, n: UInt64 = 0
            for y in stride(from: 0, to: h, by: 2) {
                let ra = pa + y * rowA, rb = pb + y * rowB
                for x in 0..<w {
                    let d = Int(ra[x]) - Int(rb[x])
                    se &+= UInt64(d * d)
                }
                n += UInt64(w)
            }
            guard n > 0 else { return nil }
            let mse = Double(se) / Double(n)
            return mse == 0 ? 99 : 10 * log10(255 * 255 / mse)
        }
        guard let y = plane(0), let c = plane(1) else { return nil }
        return (y, c)
    }
}

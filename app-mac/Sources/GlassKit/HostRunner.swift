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
    var frames = 0, idrs = 0, notSent = 0, idleIDRs = 0, bytes = 0, refines = 0, refineBytes = 0
    /// Whether the encoder took the quality frames' QP cap.
    var qpCapTaken: Bool?
    /// Captured frames dropped because a newer one arrived while encoding.
    var stale = 0
    var frameId: UInt32 = 0
    var psnrY: [Double] = [], psnrC: [Double] = []
    var producedChroma: Int?
    var hardware = true, lowLatency = false
    /// The frame-rate cap: the viewer's refresh, at most this display's.
    var sendHz = 0
    /// Rate control (M5): the bitrate now, the lowest set, changes.
    var rateNow = 0, rateLow = 0, rateChanges = 0
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
    /// The viewer's screen refresh (Hz, 0 = unknown), from its Hello.
    var viewerHz: UInt16 = 0
    /// Streaming the scorecard's test scene (ADR 0033).
    var scorecard = false
    /// The Mac's sound, to the viewer (M4b step 3).
    var audio: HostAudio?
    /// This Mac's Wi-Fi link during the session (M4 step 5).
    let wifi = WifiWatch()
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

    /// A virtual display at the viewer's refresh (30 to 240 Hz; 60 if it
    /// didn't say, or if macOS refuses the rate), and the rate it got.
    static func virtualDisplay(name: String, pointsW: Int, pointsH: Int, hiDPI: Bool, viewerHz: Int,
                               mirror: CGDirectDisplayID) -> (VirtualDisplay, Int)? {
        let want = viewerHz > 0 ? min(max(viewerHz, 30), 240) : 60
        for hz in want == 60 ? [60] : [want, 60] {
            if let v = VirtualDisplay(name: name, pointsW: pointsW, pointsH: pointsH, hiDPI: hiDPI,
                                      hz: Double(hz), mirror: mirror) {
                return (v, hz)
            }
        }
        return nil
    }

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
        // The scorecard (ADR 0033): the test scene at the viewer's size and
        // refresh; no virtual display, nothing on this Mac's screen changes.
        scorecard = (viewer.caps & UInt8(GLASS_CAP_SCORECARD)) != 0
        if scorecard {
            let vw = (viewer.display_w > 0 ? Int(viewer.display_w) : runner.display.width) & ~1
            let vh = (viewer.display_h > 0 ? Int(viewer.display_h) : runner.display.height) & ~1
            disp = HostDisplay(id: disp.id, width: vw, height: vh,
                               fps: viewer.refresh_hz > 0 ? Int(viewer.refresh_hz) : 60, bounds: disp.bounds)
            note = " (scorecard test scene, ADR 0033)"
        } else if Int32(mode) != GLASS_MODE_HOST_SCREEN && Int32(mode) < GLASS_MODE_COUNT {
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
                      let made = Self.virtualDisplay(name: "Glass \(vw)x\(vh)", pointsW: pw, pointsH: ph,
                                                        hiDPI: hidpi, viewerHz: Int(viewer.refresh_hz),
                                                        mirror: runner.display.id) {
                let (v, hz) = made
                virtual = v
                // Captured at the viewer's pixel size and refresh: 1:1 on
                // its screen, motion at its rate (was a fixed 60 Hz).
                disp = HostDisplay(id: v.id, width: vw, height: vh, fps: hz, bounds: CGDisplayBounds(v.id))
                actual = mode
                note = " (virtual display, looks like \(pw)x\(ph)\(hidpi ? " HiDPI" : "") at \(hz) Hz)"
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
        let log = SessionLog(kind: "host")
        _ = log.due(started)
        let viewer = q.sync { s.viewer }
        viewerHz = viewer.refresh_hz
        // 4:4:4 from the start unless the viewer prefers 4:2:0.
        configure(mode: viewer.mode, allow444: viewer.caps & UInt8(GLASS_CAP_PREFER_420) == 0)
        // The sound (M4b step 3): its own capture, kept across view changes.
        if !runner.options.testPattern, let out = q.sync(execute: { s.audioOut() }) {
            let a = HostAudio(out: out)
            a.start(display: runner.display.id)
            audio = a
        }

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
        var clipTimer: DispatchSourceTimer?
        if runner.options.clipboard {
            let dir = ClipboardSync.fileDir(role: "host")
            q.sync { s.setFileDir(dir) }
            let sync = ClipboardSync(sendCurrent: false, endpoint: .init(
                send: { [self] item in q.async { [self] in s.sendClipboard(item) } },
                takeText: { [self] in q.sync { s.takeClipboard() } },
                takeOffer: { [self] in q.sync { s.takeOffer() } },
                fetch: { [self] id in q.sync { s.fetch(id) } },
                progress: { [self] in q.sync { s.clipProgress } },
                peer: s.peerName))
            let t = DispatchSource.makeTimerSource(queue: .main)
            t.schedule(deadline: .now(), repeating: .milliseconds(150))
            t.setEventHandler { sync.poll() }
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
            // Rate control (M5): the viewer's limit, never above ours.
            if let limit = q.sync(execute: { s.takeVideoRate() }) {
                let ours = runner.options.mbps * 1_000_000
                let bps = limit == 0 ? ours : min(Int(limit), ours)
                if bps != totals.rateNow {
                    q.async { [self] in pipeline?.encoder.setBitrate(bps) }
                    totals.rateChanges += 1
                    totals.rateNow = bps
                    totals.rateLow = totals.rateLow == 0 ? bps : min(totals.rateLow, bps)
                }
            }
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
            // The running log: a later session or a crash can't lose it.
            if log.due(GlassClock.nowUs()) {
                if clipTimer != nil { clipSummary = ClipboardSync.summary(q.sync { s.clipProgress }, other: "viewer") }
                log.write(q.sync { report(started: started, reason: SessionLog.running, counters: s.counters, running: true) })
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        cursorTimer.cancel()
        clipTimer?.cancel()
        audio?.stop()
        if clipTimer != nil { clipSummary = ClipboardSync.summary(q.sync { s.clipProgress }, other: "viewer") }
        pipeline?.stop()
        pipeline = nil
        virtual?.destroy()
        virtual = nil
        let counters: GLASS_HostCounters = q.sync {
            let c = s.counters
            s.close()
            return c
        }
        let text = report(started: started, reason: reason, counters: counters)
        log.write(text)
        return text
    }

    /// `running`: a report while the session goes on (on `q`; leaves out
    /// the sharpness probe, whose numbers another queue is adding to).
    private func report(started: UInt64, reason: String, counters: GLASS_HostCounters, running: Bool = false) -> String {
        let o = runner.options
        let t = totals
        let d = runner.display
        let secs = max(Double(GlassClock.nowUs() - started) / 1e6, 0.001)
        let inp = counters.input
        let chroma = [1: "4:2:0", 2: "4:2:2", 3: "4:4:4"][t.producedChroma ?? 0] ?? "?"
        var out = [runner.bannerText]
        out.append("Host: Mac display \(d.width)x\(d.height) @ \(d.fps) Hz\(o.testPattern ? " (test pattern)" : "") | HEVC \(o.mbps) Mbps | encoder \(t.hardware ? "hardware" : "SOFTWARE")\(t.lowLatency ? ", low-latency rate control" : "") | encoded chroma \(chroma)")
        out.append(running
            ? "Session with \u{201C}\(s.peerName)\u{201D} (\(s.peerAddress)) still running after \(String(format: "%.1f", secs)) s"
            : "Session with \u{201C}\(s.peerName)\u{201D} (\(s.peerAddress)) ended after \(String(format: "%.1f", secs)) s (\(reason))")
        out.append("  streamed: " + history.joined(separator: " -> "))
        if no444 { out.append("  NOTE: the encoder produced 4:2:0 when asked for 4:4:4, so this session switched to 4:2:0.") }
        let qpCap: String
        switch t.qpCapTaken {
        case .some(true): qpCap = "QP cap taken"
        case .some(false): qpCap = "QP cap not taken, bitrate raised only"
        case .none: qpCap = "none sent"
        }
        out.append(String(format: "  frames sent %ld (%.1f fps), IDRs %ld (idle re-sends %ld), viewer keyframe requests %ld, not sent (viewer not ready) %ld",
                          t.frames, Double(t.frames) / secs, t.idrs, t.idleIDRs, Int(counters.keyframe_requests), t.notSent)
                   + " | still-picture quality frames \(t.refines) (\(t.refineBytes / 1024) KB, \(qpCap))")
        out.append(String(format: "  bitrate %.1f Mbps avg | %ld packets", Double(t.bytes) * 8 / secs / 1e6, Int(counters.packets_sent)))
        out.append(String(format: "  FEC (M4): %llu parity packets (%.1f%% of packets) | the viewer's latest reported loss %.2f%% | NACKs answered %llu (%llu packets resent)",
                          counters.parity_packets,
                          Double(counters.parity_packets) * 100 / Double(max(counters.packets_sent, 1)),
                          Double(counters.loss_ppm) / 1e4,
                          counters.nacks, counters.packets_resent))
        out.append("  network (M4): video marked for Wi-Fi priority: \(counters.qos == 1 ? "yes" : counters.qos == 2 ? "no" : "not yet") | keep-awakes sent \(counters.keepalives)")
        out.append(String(format: "  pacing (M5): %llu frames spread over %.1f ms on average (longest %.1f ms) | %llu sent at once for a newer frame",
                          counters.paced_frames,
                          Double(counters.pace_us_total) / Double(max(counters.paced_frames, 1)) / 1e3,
                          Double(counters.pace_us_max) / 1e3, counters.hurried_frames))
        out.append("  " + wifi.line())
        out.append(t.encode.line("encode"))
        out.append(t.delivery.line("  capture delivery"))
        out.append(t.vtEncode.line("  VideoToolbox"))
        out.append(t.send.line("send (packetise)"))
        out.append(t.interval.line("capture interval"))
        out.append("  stale frames skipped (encoder busy or the frame-rate cap, newest frame wins): \(t.stale)")
        if t.sendHz > 0 { out.append("  frame rate (M5): at most \(t.sendHz) fps, twice the viewer's screen") }
        let ours = runner.options.mbps * 1_000_000
        let limited = t.rateChanges > 0 && t.rateNow != ours
        out.append(String(format: "  rate control (M5): bitrate %@ | %ld changes%@",
                          limited ? String(format: "%.1f Mbps now, the viewer's limit", Double(t.rateNow) / 1e6)
                                  : "\(o.mbps) Mbps (no limit from the viewer)",
                          t.rateChanges,
                          t.rateLow > 0 && t.rateLow < ours ? String(format: ", lowest %.1f Mbps", Double(t.rateLow) / 1e6) : ""))
        if let a = audio {
            out.append("  sound (M4b): \(a.frames) frames sent (\(a.silent) silent) | this Mac's speakers \(a.muted ? "muted during the session" : "not muted (this output has no mute)")\(a.problem.map { " | \($0)" } ?? "")")
        }
        if !running, !t.psnrY.isEmpty {
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
    /// Still-picture refinement (M7, brought forward): passes left for the
    /// current picture (under lock), and when the last went (idle timer).
    var refinesLeft = 0
    private var lastRefine: UInt64 = 0
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
    /// The frame-rate cap (M5's first step): at most one frame per refresh
    /// of the viewer's screen (95 % of its interval), the newest one. 0 = no
    /// cap. `lastSent` on the session queue.
    let minGapUs: UInt64
    let sendHz: Int
    private var lastSent: UInt64 = 0

    init(run: HostSessionRun, display: HostDisplay, chroma444: Bool) throws {
        self.run = run
        let o = run.runner.options
        let s = run.s
        source = run.scorecard
            ? SceneSource(width: display.width, height: display.height, fps: display.fps, queue: captureQ,
                          inputSeq: { s.inputSeq })
            : o.testPattern
            ? TestPatternSource(width: display.width, height: display.height, fps: display.fps, queue: captureQ)
            : ScreenSource(display: display, queue: captureQ, bgra: chroma444)
        encoder = try HEVCEncoder(.init(width: display.width, height: display.height, fps: display.fps,
                                        bitrate: o.mbps * 1_000_000, chroma444: chroma444))
        converter = chroma444 ? Converter(width: display.width, height: display.height) : nil
        idle = DispatchSource.makeTimerSource(queue: run.q)
        let viewerHz = Int(run.viewerHz)
        // Twice the viewer's refresh (exactly it made motion uneven: the
        // two clocks drift, m5-test1..3), at most this display's.
        sendHz = viewerHz > 0 ? min(viewerHz * 2, display.fps) : display.fps
        minGapUs = sendHz > 0 && sendHz < display.fps ? UInt64(900_000 / sendHz) : 0
        run.totals.sendHz = sendHz
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
            refinesLeft = 1
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
            } else {
                // Still for 200 ms: the picture once more at high quality, at
                // most every 0.5 s (as the PC host, ADR 0032; three passes at
                // the normal budget never sharpened photos there).
                let now = GlassClock.nowUs()
                let go: Bool = self.lock.withLock {
                    guard self.refinesLeft > 0, now &- last >= 200_000, now &- self.lastRefine >= 500_000 else { return false }
                    self.refinesLeft -= 1
                    return true
                }
                if go {
                    self.lastRefine = now
                    t.refines += 1
                    let ours = self.run.runner.options.mbps * 1_000_000
                    let limit = t.rateNow > 0 && t.rateNow < ours ? t.rateNow : 0
                    self.encodeAndSend(pb, now, force: false, qualityQP: Int(glass_still_qp(Int64(limit))))
                }
            }
        }
        idle.resume()
    }

    /// Encodes the newest waiting frame until none is left (session queue).
    /// Under the frame-rate cap, a frame that comes too soon waits for its
    /// turn; a newer one replaces it meanwhile.
    private func drain() {
        while true {
            let now = GlassClock.nowUs()
            let since = now &- lastSent
            let wait = minGapUs > 0 && since < minGapUs ? minGapUs - since : 0
            lock.lock()
            guard let (pb, ts) = pending else {
                busy = false
                lock.unlock()
                return
            }
            if wait > 0 {
                lock.unlock() // still busy: the capture callback won't kick again
                run.q.asyncAfter(deadline: .now() + .microseconds(Int(wait))) { [self] in drain() }
                return
            }
            pending = nil
            lock.unlock()
            lastSent = now
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

    /// `qualityQP`: a still picture's quality frame (see `encodeQuality`).
    private func encodeAndSend(_ pb: CVPixelBuffer, _ captureUs: UInt64, force: Bool, qualityQP: Int? = nil) {
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
            func output(_ data: UnsafeRawBufferPointer, _ isKey: Bool, _: UInt64) {
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
                    if qualityQP != nil { t.refineBytes += data.count }
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
            if let qp = qualityQP, !key {
                try encoder.encodeQuality(frame, ptsUs: captureUs, maxQP: qp, output: output)
                t.qpCapTaken = encoder.qpCapTaken
            } else {
                try encoder.encode(frame, ptsUs: captureUs, keyframe: key, output: output)
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

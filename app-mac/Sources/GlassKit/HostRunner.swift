import AppKit
import CGlassCore
import CoreVideo
import Foundation

/// Runs the Mac as a host: listens, hands pairing to core, and for each
/// paired viewer captures, encodes and sends the display while injecting its
/// input. One viewer at a time.
///
/// Threads: `accept` loop on its own thread; capture, encode and every
/// session call on the frame source's serial queue (the encoder's output
/// runs on VideoToolbox's thread while that queue waits); pointer polling on
/// the main thread.
public final class HostRunner {
    public struct Options {
        public var port: UInt16 = GlassClient.defaultPort
        public var fixedPin: String?
        /// Synthetic frames and no input injection (CI; needs no permissions).
        public var testPattern = false
        public var mbps = 40
        /// End each session after this long (CLI tests).
        public var seconds: Double?
        public init() {}
    }

    public let options: Options
    public let display: HostDisplay
    public let host: GlassHost
    /// Main thread: pairing PINs, refusals and the like.
    public var onEvent: (GlassHostEvent) -> Void = { _ in }
    /// Main thread: a viewer connected (name, address).
    public var onConnected: (String, String) -> Void = { _, _ in }
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

    public var info: GLASS_StreamInfo {
        GLASS_StreamInfo(codec: UInt8(GLASS_CODEC_HEVC), chroma: UInt8(GLASS_CHROMA_420),
                         width: UInt16(display.width), height: UInt16(display.height), fps: UInt16(display.fps))
    }

    private var isStopped: Bool { stopFlag.withLock { stopped } }

    public func start() {
        let t = Thread { [weak self] in self?.acceptLoop() }
        t.name = "glass.accept"
        t.qualityOfService = .userInitiated
        thread = t
        t.start()
    }

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
                let report = runSession(s, input)
                DispatchQueue.main.async { self.onSessionEnd(report) }
            case .handled, .failed:
                continue
            case .stopped:
                return
            }
        }
    }

    // MARK: one session

    private func runSession(_ s: GlassHostSession, _ input: MacInputInjector) -> String {
        let started = GlassClock.nowUs()
        let source: FrameSource = options.testPattern
            ? TestPatternSource(width: display.width, height: display.height, fps: display.fps)
            : ScreenSource(display: display)
        let q = source.queue
        let encode = Stats(), send = Stats(), interval = Stats()
        var frames = 0, idrs = 0, notSent = 0, idleIDRs = 0, bytes = 0, frameId: UInt32 = 0
        var lastCapture: UInt64 = 0
        var lastFrame: (CVPixelBuffer, UInt64)?
        var failure: String?
        let failLock = NSLock()
        func fail(_ why: String) { failLock.withLock { if failure == nil { failure = why } } }

        let encoder: HEVCEncoder
        do {
            encoder = try HEVCEncoder(.init(width: display.width, height: display.height, fps: display.fps,
                                            bitrate: options.mbps * 1_000_000))
        } catch {
            s.close()
            return "\(banner)\nCouldn't start the encoder: \(error)"
        }

        // On q only.
        func encodeAndSend(_ pb: CVPixelBuffer, _ captureUs: UInt64, forceKey: Bool) {
            let key = s.takeKeyframeRequest() || forceKey
            let inputSeq = s.inputSeq
            do {
                try encoder.encode(pb, ptsUs: captureUs, keyframe: key) { data, isKey, _ in
                    let now = GlassClock.nowUs()
                    frameId &+= 1
                    let f = GLASS_OutFrame(frame_id: frameId, flags: isKey ? 1 : 0, capture_ts_us: captureUs,
                                           encode_us: UInt32(clamping: now &- captureUs), input_seq: inputSeq)
                    let r = s.send(f, data)
                    let sent = GlassClock.nowUs()
                    encode.push(UInt32(clamping: now &- captureUs))
                    send.push(UInt32(clamping: sent &- now))
                    if r == 1 {
                        frames += 1
                        bytes += data.count
                        if isKey { idrs += 1 }
                    } else if r == 0 {
                        notSent += 1
                    } else {
                        fail("sending failed")
                    }
                }
            } catch {
                fail("encode failed: \(error)")
            }
        }

        source.start({ pb, captureUs in
            if lastCapture > 0 { interval.push(UInt32(clamping: captureUs &- lastCapture)) }
            lastCapture = captureUs
            lastFrame = (pb, captureUs)
            encodeAndSend(pb, captureUs, forceKey: false)
        }, { error in
            if let error { fail("screen capture stopped: \(error.localizedDescription)") }
        })

        // Idle screen: ScreenCaptureKit sends nothing new, so answer keyframe
        // requests (and a viewer that has just started listening) with the
        // last picture.
        let idle = DispatchSource.makeTimerSource(queue: q)
        idle.schedule(deadline: .now() + .milliseconds(50), repeating: .milliseconds(50))
        idle.setEventHandler {
            guard let (pb, _) = lastFrame, GlassClock.nowUs() &- lastCapture > 50_000 else { return }
            if s.takeKeyframeRequest() {
                idleIDRs += 1
                let now = GlassClock.nowUs()
                lastCapture = now
                encodeAndSend(pb, now, forceKey: true)
            }
        }
        idle.resume()

        // The pointer, drawn by the viewer itself.
        let cursorWatch = CursorWatch(display: display)
        var serial: UInt32 = 0
        let cursorTimer = DispatchSource.makeTimerSource(queue: .main)
        cursorTimer.schedule(deadline: .now(), repeating: .milliseconds(33))
        cursorTimer.setEventHandler {
            guard let shape = cursorWatch.poll() else { return }
            q.async {
                serial &+= 1
                s.sendCursor(serial: serial, width: shape.width, height: shape.height,
                             hotX: shape.hotX, hotY: shape.hotY, rgba: shape.rgba)
            }
        }
        cursorTimer.resume()

        // Until the viewer leaves (or we stop).
        let deadline = options.seconds.map { started + UInt64($0 * 1e6) }
        var reason = "viewer left"
        while true {
            if isStopped { reason = "sharing stopped"; break }
            if !q.sync(execute: { s.connected }) { break }
            if let f = failLock.withLock({ failure }) { reason = f; break }
            if let deadline, GlassClock.nowUs() >= deadline { reason = "--seconds elapsed"; break }
            Thread.sleep(forTimeInterval: 0.1)
        }
        cursorTimer.cancel()
        source.stop()
        idle.cancel()
        let counters: GLASS_HostCounters = q.sync {
            let c = s.counters
            s.close()
            lastFrame = nil
            return c
        }

        let secs = max(Double(GlassClock.nowUs() - started) / 1e6, 0.001)
        let inp = counters.input
        return q.sync {
            var out = [banner]
            out.append("Host: display \(display.width)x\(display.height) @ \(display.fps) Hz\(options.testPattern ? " (test pattern)" : "") | HEVC 4:2:0 \(options.mbps) Mbps | encoder \(encoder.usingHardware ? "hardware" : "SOFTWARE")\(encoder.lowLatencyRateControl ? ", low-latency rate control" : "")")
            out.append("Session with \u{201C}\(s.peerName)\u{201D} (\(s.peerAddress)) ended after \(String(format: "%.1f", secs)) s (\(reason))")
            out.append(String(format: "  frames sent %ld (%.1f fps), IDRs %ld (idle re-sends %ld), viewer keyframe requests %ld, not sent (viewer not ready) %ld",
                              frames, Double(frames) / secs, idrs, idleIDRs, Int(counters.keyframe_requests), notSent))
            out.append(String(format: "  bitrate %.1f Mbps avg | %ld packets", Double(bytes) * 8 / secs / 1e6, Int(counters.packets_sent)))
            out.append(encode.line("encode"))
            out.append(send.line("send (packetise)"))
            out.append(interval.line("capture interval"))
            out.append("  input: \(inp.packets) packets | \(inp.keys) keys, \(inp.clicks) clicks, \(inp.scrolls) scrolls, \(inp.moves) moves | repaired \(inp.repaired)\(options.testPattern ? " (test pattern: not injected)" : "")")
            out.append("  rejected UDP datagrams (failed decryption or replays): \(counters.rejected_datagrams)")
            if !options.testPattern && !HostPermissions.accessibility {
                out.append("  NOTE: Accessibility is off for Glass, so the viewer's mouse and keyboard did nothing.")
            }
            return out.joined(separator: "\n")
        }
    }
}

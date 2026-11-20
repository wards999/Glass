import AppKit
import Foundation
import GlassKit
import QuartzCore

// M1: offline playback of a .glsr recording.

func describe(_ rec: Recording) -> String {
    "\(rec.header.width)x\(rec.header.height) @ \(rec.header.fps) Hz, HEVC \(rec.is444 ? "4:4:4" : "4:2:0"), \(rec.frames.count) frames"
}

/// Decode-only benchmark: every frame, synchronously, back to back.
func bench(_ rec: Recording) throws -> String {
    let dec = HEVCDecoder(chroma444: rec.is444)
    let decode = Stats()
    var decoded = 0
    var first: CVPixelBuffer?
    let t0 = CACurrentMediaTime()
    for f in rec.frames {
        let pb = try rec.withFrameBytes(f) { try dec.decode($0) }
        if let pb {
            decoded += 1
            decode.push(seconds: dec.lastDecodeSeconds)
            if first == nil { first = pb }
        }
    }
    let wall = CACurrentMediaTime() - t0
    var out = ["Bench: decoded \(decoded)/\(rec.frames.count) frames in \(String(format: "%.2f", wall)) s (\(String(format: "%.0f", Double(decoded) / wall)) fps)"]
    out.append("  decoder: \(dec.usingHardware ? "hardware" : "SOFTWARE")\(rec.is444 ? "  <- HEVC 4:4:4 hardware decode: \(dec.usingHardware ? "YES" : "NO")" : "")")
    if let pb = first {
        let fmt = CVPixelBufferGetPixelFormatType(pb)
        let fourcc = String(bytes: [24, 16, 8, 0].map { UInt8((fmt >> $0) & 0xff) }, encoding: .ascii) ?? "?"
        out.append("  output: \(CVPixelBufferGetWidth(pb))x\(CVPixelBufferGetHeight(pb)) '\(fourcc)' IOSurface=\(CVPixelBufferGetIOSurface(pb) != nil) matrix=\(Renderer.isBT709(pb) ? "BT.709" : "BT.601")")
    }
    out.append(decode.line("decode"))
    let enc = Stats()
    for f in rec.frames where f.meta.encode_us > 0 { enc.push(f.meta.encode_us) }
    if enc.summary.count > 0 { out.append(enc.line("encode (host)")) }
    return out.joined(separator: "\n")
}

/// Windowed playback: CADisplayLink-paced, decoding every frame due by now and
/// presenting the latest. LATER(M5): adaptive jitter buffer + display-linked pacing from core.
final class Player: NSObject {
    let rec: Recording
    let dec: HEVCDecoder
    let renderer: Renderer
    var onFinish: (String) -> Void = { _ in }
    var window: NSWindow!
    var layer: CAMetalLayer!
    var link: CADisplayLink?
    var next = 0
    var startHost: Double = 0
    var baseTs: UInt64 = 0
    var finishedAt: Double?
    let decode = Stats()
    let render = Stats()
    let presentInterval = Stats()
    var lastPresented: Double = 0
    var presented = 0
    var dropped = 0 // decoded but superseded before display

    init(_ rec: Recording) throws {
        self.rec = rec
        dec = HEVCDecoder(chroma444: rec.is444)
        renderer = try Renderer()
    }

    func open() {
        let scale = NSScreen.main?.backingScaleFactor ?? 2
        let w = CGFloat(rec.header.width), h = CGFloat(rec.header.height)
        // Window sized so one video pixel = one screen pixel.
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: w / scale, height: h / scale),
                          styleMask: [.titled, .closable, .miniaturizable], backing: .buffered, defer: false)
        window.title = "Glass — \(describe(rec))"
        window.isReleasedWhenClosed = false
        let view = MetalView(frame: window.contentLayoutRect)
        view.wantsLayer = true
        layer = (view.layer as! CAMetalLayer)
        layer.device = renderer.device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.contentsScale = scale
        layer.drawableSize = CGSize(width: w, height: h)
        layer.displaySyncEnabled = true
        window.contentView = view
        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        let link = view.displayLink(target: self, selector: #selector(tick(_:)))
        link.add(to: .main, forMode: .common)
        self.link = link
    }

    @objc func tick(_ link: CADisplayLink) {
        let now = CACurrentMediaTime()
        if let done = finishedAt {
            if now - done > 0.5 { finish(nil) }
            return
        }
        if startHost == 0, let f = rec.frames.first {
            startHost = now
            baseTs = f.meta.capture_ts_us
        }
        let due = baseTs + UInt64((now - startHost) * 1e6)
        var latest: CVPixelBuffer?
        do {
            while next < rec.frames.count, rec.frames[next].meta.capture_ts_us <= due {
                let pb = try rec.withFrameBytes(rec.frames[next]) { try dec.decode($0) }
                if pb != nil {
                    decode.push(seconds: dec.lastDecodeSeconds)
                    if latest != nil { dropped += 1 }
                    latest = pb
                }
                next += 1
            }
        } catch {
            finish("error: \(error)")
            return
        }
        if let pb = latest { present(pb) }
        if next == rec.frames.count { finishedAt = now }
    }

    func present(_ pb: CVPixelBuffer) {
        guard let drawable = layer.nextDrawable(), let cb = renderer.queue.makeCommandBuffer() else { return }
        renderer.encode(pb, to: drawable.texture, in: cb)
        let submitted = CACurrentMediaTime()
        drawable.addPresentedHandler { [weak self] d in
            let t = d.presentedTime
            guard t > 0 else { return }
            DispatchQueue.main.async {
                guard let self else { return }
                self.render.push(seconds: t - submitted)
                if self.lastPresented > 0 { self.presentInterval.push(seconds: t - self.lastPresented) }
                self.lastPresented = t
                self.presented += 1
            }
        }
        cb.present(drawable)
        cb.commit()
    }

    func finish(_ error: String?) {
        guard link != nil else { return }
        link?.invalidate()
        link = nil
        window.orderOut(nil)
        var out = ["Playback: presented \(presented), superseded \(dropped)"]
        if let error { out.append(error) }
        out.append("  decoder: \(dec.usingHardware ? "hardware" : "SOFTWARE")")
        out.append(decode.line("decode"))
        out.append(render.line("render+present"))
        out.append(presentInterval.line("present interval"))
        onFinish(out.joined(separator: "\n"))
    }
}

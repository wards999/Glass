import AppKit
import CVirtualDisplay
import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import ScreenCaptureKit

/// Screen Recording and Accessibility, which the Mac needs to be a host.
public enum HostPermissions {
    /// Screen Recording granted (capture works).
    public static var screenRecording: Bool { CGPreflightScreenCaptureAccess() }
    /// Accessibility granted (injected mouse and keyboard reach apps).
    public static var accessibility: Bool { AXIsProcessTrusted() }

    /// Asks macOS once; it adds Glass to the list in System Settings.
    public static func requestScreenRecording() { _ = CGRequestScreenCaptureAccess() }
    public static func requestAccessibility() {
        let key = kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String
        _ = AXIsProcessTrustedWithOptions([key: true] as CFDictionary)
    }

    public static func openScreenRecordingSettings() {
        open("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
    }
    public static func openAccessibilitySettings() {
        open("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
    }
    private static func open(_ s: String) {
        if let u = URL(string: s) { NSWorkspace.shared.open(u) }
    }
}

/// The display a host shares, in pixels.
public struct HostDisplay {
    public let id: CGDirectDisplayID
    public let width: Int
    public let height: Int
    public let fps: Int
    /// Bounds in global points (for input).
    public let bounds: CGRect

    /// The main display (the one with the menu bar).
    public static func main() -> HostDisplay { of(CGMainDisplayID()) }

    /// A display at its native pixel size.
    public static func of(_ id: CGDirectDisplayID) -> HostDisplay {
        let mode = CGDisplayCopyDisplayMode(id)
        let w = mode?.pixelWidth ?? CGDisplayPixelsWide(id)
        let h = mode?.pixelHeight ?? CGDisplayPixelsHigh(id)
        var fps = Int((mode?.refreshRate ?? 0).rounded())
        if fps <= 0 { fps = NSScreen.main?.maximumFramesPerSecond ?? 60 }
        return HostDisplay(id: id, width: w, height: h, fps: max(fps, 30), bounds: CGDisplayBounds(id))
    }
}

/// Delivers frames to the host: `(pixelBuffer, captureTimeUs)` on one serial queue.
public protocol FrameSource: AnyObject {
    var queue: DispatchQueue { get }
    func start(_ onFrame: @escaping (CVPixelBuffer, UInt64) -> Void, _ done: @escaping (Error?) -> Void)
    func stop()
}

/// ScreenCaptureKit capture of one display: IOSurface-backed 4:2:0 video-range
/// buffers (straight into the encoder), only when something changed, no
/// pointer (the viewer draws its own).
public final class ScreenSource: NSObject, FrameSource, SCStreamOutput, SCStreamDelegate {
    public let queue: DispatchQueue
    private let display: HostDisplay
    private var stream: SCStream?
    private var onFrame: ((CVPixelBuffer, UInt64) -> Void)?
    private var onStop: ((Error?) -> Void)?

    /// 4:4:4 needs full-colour BGRA (ScreenCaptureKit has no 4:4:4 YCbCr).
    private let bgra: Bool

    public init(display: HostDisplay, queue: DispatchQueue, bgra: Bool = false) {
        self.display = display
        self.queue = queue
        self.bgra = bgra
    }

    public func start(_ onFrame: @escaping (CVPixelBuffer, UInt64) -> Void, _ done: @escaping (Error?) -> Void) {
        self.onFrame = onFrame
        onStop = done
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { [self] content, error in
            guard let content, let scDisplay = content.displays.first(where: { $0.displayID == display.id }) else {
                done(error ?? GlassError.decoder("ScreenCaptureKit: display not found", -1))
                return
            }
            let filter = SCContentFilter(display: scDisplay, excludingWindows: [])
            let cfg = SCStreamConfiguration()
            cfg.width = display.width
            cfg.height = display.height
            cfg.pixelFormat = bgra ? kCVPixelFormatType_32BGRA : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            cfg.colorMatrix = kCVImageBufferYCbCrMatrix_ITU_R_709_2
            cfg.colorSpaceName = CGColorSpace.sRGB
            cfg.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(display.fps))
            cfg.queueDepth = 4
            cfg.showsCursor = false
            cfg.capturesAudio = false
            let s = SCStream(filter: filter, configuration: cfg, delegate: self)
            do {
                try s.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
            } catch {
                done(error)
                return
            }
            stream = s
            s.startCapture { error in
                if let error { done(error) }
            }
        }
    }

    public func stop() {
        stream?.stopCapture { _ in }
        stream = nil
        onFrame = nil
    }

    public func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, let pb = CMSampleBufferGetImageBuffer(sb),
              let att = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let raw = att.first?[.status] as? Int, SCFrameStatus(rawValue: raw) == .complete else { return }
        let pts = CMSampleBufferGetPresentationTimeStamp(sb)
        let us = pts.isValid ? UInt64(max(0, pts.seconds * 1e6)) : GlassClock.nowUs()
        onFrame?(pb, us)
    }

    public func stream(_ stream: SCStream, didStopWithError error: Error) {
        onStop?(error)
    }
}

/// Moving bars and a clock, generated at `fps` without any permission: CI
/// and quick checks of the encode -> network path.
public final class TestPatternSource: FrameSource {
    public let queue: DispatchQueue
    private let width: Int
    private let height: Int
    private let fps: Int
    private var timer: DispatchSourceTimer?
    private var pool: CVPixelBufferPool?
    private var n = 0

    public init(width: Int, height: Int, fps: Int, queue: DispatchQueue) {
        self.queue = queue
        self.width = width
        self.height = height
        self.fps = fps
    }

    public func start(_ onFrame: @escaping (CVPixelBuffer, UInt64) -> Void, _ done: @escaping (Error?) -> Void) {
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferWidthKey: width,
            kCVPixelBufferHeightKey: height,
            kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any]() as CFDictionary,
        ]
        CVPixelBufferPoolCreate(kCFAllocatorDefault, nil, attrs as CFDictionary, &pool)
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now(), repeating: .nanoseconds(1_000_000_000 / fps))
        t.setEventHandler { [weak self] in
            guard let self, let pool = self.pool else { return }
            var pb: CVPixelBuffer?
            guard CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &pb) == kCVReturnSuccess, let pb else { return }
            self.draw(pb)
            onFrame(pb, GlassClock.nowUs())
        }
        timer = t
        t.resume()
    }

    public func stop() {
        timer?.cancel()
        timer = nil
    }

    private func draw(_ pb: CVPixelBuffer) {
        n += 1
        CVPixelBufferLockBaseAddress(pb, [])
        defer { CVPixelBufferUnlockBaseAddress(pb, []) }
        guard let y = CVPixelBufferGetBaseAddressOfPlane(pb, 0)?.assumingMemoryBound(to: UInt8.self),
              let uv = CVPixelBufferGetBaseAddressOfPlane(pb, 1)?.assumingMemoryBound(to: UInt8.self) else { return }
        let yStride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0)
        let uvStride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1)
        let bar = (n * 8) % max(width, 1)
        for row in 0..<height {
            let line = y + row * yStride
            for col in 0..<width {
                let inBar = col >= bar && col < bar + 64
                line[col] = inBar ? 235 : UInt8(16 + ((col + row) / 8 + n) % 200)
            }
        }
        for row in 0..<(height / 2) {
            let line = uv + row * uvStride
            for col in stride(from: 0, to: width, by: 2) {
                line[col] = UInt8(128 &+ (row % 64))
                line[col + 1] = UInt8(128 &- (col % 64))
            }
        }
    }
}

/// A virtual display at the viewer's size (M3c). While it exists the Mac's
/// own screen mirrors it, so windows are where the viewer sees them; the
/// mirroring is "for this app only", so macOS undoes it if Glass quits.
public final class VirtualDisplay {
    public let id: CGDirectDisplayID
    private var handle: UnsafeMutableRawPointer?
    private var mirrored: CGDirectDisplayID?

    public static var available: Bool { glass_vd_available() }

    private static func online(_ id: CGDirectDisplayID) -> Bool {
        var ids = [CGDirectDisplayID](repeating: 0, count: 32)
        var n: UInt32 = 0
        guard CGGetOnlineDisplayList(UInt32(ids.count), &ids, &n) == .success else { return false }
        return ids.prefix(Int(n)).contains(id)
    }

    /// `points`: the display's size as macOS lays it out; `hiDPI` doubles
    /// its pixels. Blocks up to a few seconds while macOS adds it.
    public init?(name: String, pointsW: Int, pointsH: Int, hiDPI: Bool, hz: Double, mirror physical: CGDirectDisplayID) {
        var did: UInt32 = 0
        guard let h = glass_vd_create(name, UInt32(pointsW), UInt32(pointsH), hz, hiDPI, &did), did != 0 else { return nil }
        handle = h
        id = did
        let deadline = Date().addingTimeInterval(3)
        while !Self.online(did), Date() < deadline { Thread.sleep(forTimeInterval: 0.05) }
        guard Self.online(did) else {
            glass_vd_destroy(h)
            handle = nil
            return nil
        }
        var cfg: CGDisplayConfigRef?
        if CGBeginDisplayConfiguration(&cfg) == .success, let cfg {
            _ = CGConfigureDisplayMirrorOfDisplay(cfg, physical, did)
            if CGCompleteDisplayConfiguration(cfg, .forAppOnly) == .success { mirrored = physical }
        }
        // Let the new layout settle before capturing it.
        Thread.sleep(forTimeInterval: 0.7)
    }

    deinit { destroy() }

    /// Gives the Mac its screen back and removes the display.
    public func destroy() {
        if let m = mirrored {
            var cfg: CGDisplayConfigRef?
            if CGBeginDisplayConfiguration(&cfg) == .success, let cfg {
                _ = CGConfigureDisplayMirrorOfDisplay(cfg, m, 0)
                _ = CGCompleteDisplayConfiguration(cfg, .forAppOnly)
            }
            mirrored = nil
        }
        if let h = handle {
            handle = nil
            glass_vd_destroy(h)
        }
    }
}

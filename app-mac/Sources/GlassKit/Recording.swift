import CGlassCore
import Foundation

public enum GlassError: Error, CustomStringConvertible {
    case recording(Int32)
    case decoder(String, OSStatus)
    case metal(String)

    public var description: String {
        switch self {
        case .recording(let code): return "bad recording (core error \(code))"
        case .decoder(let what, let status): return "\(what) failed (OSStatus \(status))"
        case .metal(let what): return "Metal: \(what)"
        }
    }
}

/// A `.glsr` recording, memory-mapped and parsed by core. Frames are byte
/// ranges into `data`; nothing is copied.
public final class Recording {
    public struct Frame {
        public let meta: GLASS_FrameMeta
        public let range: Range<Int>
        public var isIDR: Bool { meta.flags & UInt8(GLASS_FLAG_IDR) != 0 }
    }

    public let header: GLASS_RecordingHeader
    public let data: Data
    public let frames: [Frame]

    public var is444: Bool { header.chroma == UInt8(GLASS_CHROMA_444) }

    public convenience init(contentsOf url: URL) throws {
        try self.init(data: Data(contentsOf: url, options: .alwaysMapped))
    }

    public init(data: Data) throws {
        self.data = data
        var header = GLASS_RecordingHeader()
        var frames: [Frame] = []
        let rc: Int32 = data.withUnsafeBytes { raw -> Int32 in
            let base = raw.bindMemory(to: UInt8.self).baseAddress
            let rc = glass_recording_header(base, raw.count, &header)
            if rc != 0 { return rc }
            var offset = 0
            var meta = GLASS_FrameMeta()
            var bytes = GLASS_Bytes()
            while true {
                let r = glass_recording_next_frame(base, raw.count, &offset, &meta, &bytes)
                if r == -1 { return 0 }
                if r != 0 { return r }
                let start = bytes.ptr.map { UnsafeRawPointer($0) - UnsafeRawPointer(base!) } ?? 0
                frames.append(Frame(meta: meta, range: start..<(start + Int(bytes.len))))
            }
        }
        if rc != 0 { throw GlassError.recording(rc) }
        self.header = header
        self.frames = frames
    }

    /// Calls `body` with the frame's Annex-B bytes (valid only inside `body`).
    public func withFrameBytes<R>(_ frame: Frame, _ body: (UnsafeRawBufferPointer) throws -> R) rethrows -> R {
        try data.withUnsafeBytes { raw in
            try body(UnsafeRawBufferPointer(rebasing: raw[frame.range]))
        }
    }
}

/// Fixed-size latency ring from core (p50/p99 etc.).
public final class Stats {
    private let ring: OpaquePointer

    public init() { ring = glass_stats_new() }
    deinit { glass_stats_free(ring) }

    public func push(_ us: UInt32) { glass_stats_push(ring, us) }
    public func reset() { glass_stats_reset(ring) }
    public func push(seconds: Double) { push(UInt32(max(0, min(seconds * 1e6, Double(UInt32.max))))) }
    public var summary: GLASS_Summary { glass_stats_summary(ring) }

    public func line(_ name: String) -> String {
        let s = summary
        func ms(_ us: UInt32) -> String { String(format: "%7.2f", Double(us) / 1000) }
        return "  \(name.padding(toLength: 18, withPad: " ", startingAt: 0)) p50 \(ms(s.p50_us)) ms  p99 \(ms(s.p99_us)) ms  max \(ms(s.max_us)) ms  sd \(String(format: "%6.2f", s.stddev_us / 1000)) ms  (n=\(s.count))"
    }
}

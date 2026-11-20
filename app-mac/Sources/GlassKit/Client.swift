import CGlassCore
import Foundation

/// Media clock in µs, shared with core. Equals `CACurrentMediaTime() * 1e6`
/// (both are `mach_absolute_time`), so Core Animation timestamps mix freely.
public enum GlassClock {
    public static func nowUs() -> UInt64 { glass_clock_now_us() }
    public static func us(fromMediaTime t: Double) -> UInt64 { UInt64(max(0, t * 1e6)) }
}

public struct ConnectFailure: Error, CustomStringConvertible {
    public let code: Int32
    public let host: String

    // Codes are GLASS_ConnectError values (core/src/net.rs). Matched as
    // literals: the C enum's typedef is int32_t before C23.
    public var description: String {
        switch code {
        case 1: // GLASS_ConnectError_Resolve
            return "\"\(host)\" isn't an IPv4 address or a name that resolves to one."
        case 2: // GLASS_ConnectError_Connect
            return "Couldn't reach Glass on \(host). Is host.cmd running on the PC, and is glass.exe allowed through Windows Firewall (Windows Security > Firewall > Allow an app)? Both machines must be on the same network."
        case 3: // GLASS_ConnectError_Handshake
            return "Reached \(host), but the Glass handshake failed."
        case 4: // GLASS_ConnectError_Version
            return "The PC runs a different Glass version. Use the zip and dmg from the same test build."
        case 5: // GLASS_ConnectError_Socket
            return "Couldn't open a UDP socket."
        default:
            return "Connecting to \(host) failed (error \(code))."
        }
    }
}

/// Mouse buttons as core numbers them.
public enum GlassButton: UInt8 {
    case left = 0, right = 1, middle = 2, back = 3, forward = 4
}

/// Live-stream viewer connection (core does networking, reassembly, clock
/// sync and input sending on its own threads). Exactly one thread may call
/// `waitFrame` and `close`; everything else may be called from any thread.
public final class GlassClient {
    public enum Wait { case frame, timeout, disconnected }

    // Guards `ptr` for calls from other threads against `close()`.
    private let lock = NSLock()
    private var ptr: OpaquePointer?
    public let info: GLASS_StreamInfo
    public let host: String
    public let port: UInt16

    public static let defaultPort = UInt16(GLASS_DEFAULT_PORT)

    public init(host: String, port: UInt16 = GlassClient.defaultPort) throws {
        var err: Int32 = 0
        guard let p = glass_client_connect(host, port, &err) else {
            throw ConnectFailure(code: err, host: host)
        }
        ptr = p
        info = glass_client_info(p)
        self.host = host
        self.port = port
    }

    deinit { close() }

    public var is444: Bool { info.chroma == UInt8(GLASS_CHROMA_444) }

    private func with<R>(_ fallback: R, _ body: (OpaquePointer) -> R) -> R {
        lock.lock()
        defer { lock.unlock() }
        guard let ptr else { return fallback }
        return body(ptr)
    }

    /// Waits up to `timeoutMs`; on `.frame`, `body` runs with the frame's
    /// metadata and Annex-B bytes (valid only inside `body`).
    @discardableResult
    public func waitFrame(timeoutMs: UInt32, _ body: (GLASS_LiveFrameMeta, UnsafeRawBufferPointer) throws -> Void) rethrows -> Wait {
        guard let ptr else { return .disconnected } // same thread as close()
        var meta = GLASS_LiveFrameMeta()
        var bytes = GLASS_Bytes()
        switch glass_client_wait_frame(ptr, timeoutMs, &meta, &bytes) {
        case 1:
            defer { glass_client_release_frame(ptr) }
            try body(meta, UnsafeRawBufferPointer(start: bytes.ptr.map { UnsafeRawPointer($0) }, count: Int(bytes.len)))
            return .frame
        case 0:
            return .timeout
        default:
            return .disconnected
        }
    }

    public func requestKeyframe() {
        with(()) { glass_client_request_keyframe($0) }
    }

    public var stats: GLASS_ClientStats {
        with(GLASS_ClientStats()) { glass_client_stats($0) }
    }

    /// Host timestamp on the local clock, once clock sync has a sample.
    public func hostToLocal(_ hostUs: UInt64) -> UInt64? {
        let v = with(UInt64(0)) { glass_client_host_to_local_us($0, hostUs) }
        return v == 0 ? nil : v
    }

    // MARK: input (sent immediately)

    /// Mac key code -> HID usage (0 = not forwarded).
    public static func hid(forMacKey kvk: UInt16, iso: Bool, pcStyle: Bool) -> UInt8 {
        glass_keymap_mac_to_hid(kvk, iso, pcStyle)
    }

    public func key(_ hid: UInt8, down: Bool) {
        with(()) { glass_client_input_key($0, hid, down) }
    }

    public func button(_ b: GlassButton, down: Bool, x: UInt16, y: UInt16) {
        with(()) { glass_client_input_button($0, b.rawValue, down, x, y) }
    }

    public func move(x: UInt16, y: UInt16) {
        with(()) { glass_client_input_move($0, x, y) }
    }

    /// Wheel units (120 = one notch); `dy > 0` scrolls up, `dx > 0` right.
    public func scroll(dy: Int16, dx: Int16) {
        with(()) { glass_client_input_scroll($0, dy, dx) }
    }

    public func releaseAll() {
        with(()) { glass_client_input_release_all($0) }
    }

    /// When input packet `seq` was sent (local µs).
    public func inputSentUs(_ seq: UInt32) -> UInt64? {
        let v = with(UInt64(0)) { glass_client_input_sent_us($0, seq) }
        return v == 0 ? nil : v
    }

    /// The host's pointer. `rgba` (straight alpha, top row first) is set only
    /// when the generation differs from `seen` and a shape exists.
    public func cursor(seen: UInt32) -> (info: GLASS_CursorInfo, rgba: [UInt8]?) {
        var buf = [UInt8](repeating: 0, count: 96 * 96 * 4)
        let info = with(GLASS_CursorInfo()) { p in
            buf.withUnsafeMutableBytes { glass_client_cursor(p, seen, $0.bindMemory(to: UInt8.self).baseAddress, $0.count) }
        }
        let n = Int(info.width) * Int(info.height) * 4
        guard info.generation != seen, n > 0, n <= buf.count else { return (info, nil) }
        return (info, Array(buf[0..<n]))
    }

    /// Releases held input, sends Bye and stops core's threads. Call from the
    /// thread that calls `waitFrame` (or once it has stopped).
    public func close() {
        lock.lock()
        let p = ptr
        ptr = nil
        lock.unlock()
        if let p { glass_client_close(p) }
    }
}

/// Serves a recording as a live stream on loopback (tests, demos). Logs the
/// input it receives and sends a 16x16 test pointer.
public final class ReplayHost {
    private var ptr: OpaquePointer?
    public let port: UInt16

    public init(data: Data, port: UInt16 = 0) throws {
        var out: UInt16 = 0
        let p = data.withUnsafeBytes { raw in
            glass_replay_start(raw.bindMemory(to: UInt8.self).baseAddress, raw.count, port, &out)
        }
        guard let p else { throw GlassError.recording(-2) }
        ptr = p
        self.port = out
    }

    deinit { stop() }

    /// Input actions received so far.
    public var inputActions: UInt64 { ptr.map { glass_replay_input_actions($0) } ?? 0 }

    public func stop() {
        if let p = ptr {
            ptr = nil
            glass_replay_stop(p)
        }
    }
}

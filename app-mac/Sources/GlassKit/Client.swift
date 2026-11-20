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
            return "Couldn't reach Glass on \(host). Is host.cmd running on the PC, and was glass.exe allowed through Windows Firewall? Both machines must be on the same network."
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

/// Live-stream viewer connection (core does networking, reassembly and clock
/// sync on its own threads). Exactly one thread may call `waitFrame`.
public final class GlassClient {
    public enum Wait { case frame, timeout, disconnected }

    private var ptr: OpaquePointer?
    public let info: GLASS_StreamInfo
    public let host: String
    public let port: UInt16

    public init(host: String, port: UInt16 = UInt16(GLASS_DEFAULT_PORT)) throws {
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

    /// Waits up to `timeoutMs`; on `.frame`, `body` runs with the frame's
    /// metadata and Annex-B bytes (valid only inside `body`).
    @discardableResult
    public func waitFrame(timeoutMs: UInt32, _ body: (GLASS_LiveFrameMeta, UnsafeRawBufferPointer) throws -> Void) rethrows -> Wait {
        guard let ptr else { return .disconnected }
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
        if let ptr { glass_client_request_keyframe(ptr) }
    }

    public var stats: GLASS_ClientStats {
        guard let ptr else { return GLASS_ClientStats() }
        return glass_client_stats(ptr)
    }

    /// Host timestamp on the local clock, once clock sync has a sample.
    public func hostToLocal(_ hostUs: UInt64) -> UInt64? {
        guard let ptr else { return nil }
        let v = glass_client_host_to_local_us(ptr, hostUs)
        return v == 0 ? nil : v
    }

    /// Sends Bye and stops core's threads. Call from the waiting thread (or
    /// once it has stopped waiting).
    public func close() {
        if let p = ptr {
            ptr = nil
            glass_client_close(p)
        }
    }
}

/// Serves a recording as a live stream on loopback (tests, demos).
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

    public func stop() {
        if let p = ptr {
            ptr = nil
            glass_replay_stop(p)
        }
    }
}

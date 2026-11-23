import AppKit
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

    public init(code: Int32, host: String) {
        self.code = code
        self.host = host
    }

    /// Pair first (unknown host, or the host doesn't know this Mac).
    public var needsPairing: Bool { code == 6 }
    public var wrongPin: Bool { code == 8 }

    // Codes are GLASS_ConnectError values (core/src/net.rs). Matched as
    // literals: the C enum's typedef is int32_t before C23.
    public var description: String {
        switch code {
        case 1: // GLASS_ConnectError_Resolve
            return "\"\(host)\" isn't an IPv4 address or a name that resolves to one."
        case 2: // GLASS_ConnectError_Connect
            return "Couldn't reach Glass on \(host). Is host.cmd running on the PC, and is Glass allowed through Windows Firewall (Windows Security > Firewall > Allow an app)? Both machines must be on the same network."
        case 3: // GLASS_ConnectError_Handshake
            return "Reached \(host), but the secure handshake failed. Make sure the PC and the Mac run the same Glass test build (zip and dmg from one release)."
        case 4: // GLASS_ConnectError_Version
            return "The PC runs a different Glass version. Use the zip and dmg from the same test build."
        case 5: // GLASS_ConnectError_Socket
            return "Couldn't open a UDP socket."
        case 6: // GLASS_ConnectError_NotPaired
            return "This Mac isn't paired with \(host) (or the PC has forgotten it)."
        case 7: // GLASS_ConnectError_Refused
            return "\(host) didn't allow the connection."
        case 8: // GLASS_ConnectError_WrongPin
            return "That PIN wasn't right, or it expired. Use the PIN the PC is showing now."
        case 9: // GLASS_ConnectError_PairingLocked
            return "Too many wrong PINs, so the PC has switched pairing off. Restart host.cmd on the PC, then pair again."
        case 11: // GLASS_ConnectError_Loop
            return "That would make a loop: \(host) is this Mac, or it's already viewing this Mac (directly or through another machine). Disconnect that first."
        case 10: // GLASS_ConnectError_Identity
            return "Couldn't read or create this Mac's Glass identity in ~/Library/Application Support/Glass."
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
    /// Current stream parameters (they change when the host switches view
    /// or colour; see `infoChanges`).
    public var info: GLASS_StreamInfo { with(GLASS_StreamInfo()) { glass_client_info($0) } }
    /// What the host is (`GLASS_HOST_OS_*`) and can do (`GLASS_HOST_CAP_*`).
    public let hostOS: UInt8
    public let hostCaps: UInt8
    public var hostIsMac: Bool { Int32(hostOS) == GLASS_HOST_OS_MAC }
    public let host: String
    public let port: UInt16
    /// The paired host's name ("QUIETPC22").
    public let hostName: String

    public static let defaultPort = UInt16(GLASS_DEFAULT_PORT)

    /// Connects to a paired host (TLS with pinned certificates; encrypted
    /// media). Throws `ConnectFailure` with `needsPairing` if not paired.
    /// `mode`: view mode to ask for (`GLASS_MODE_*`); `fullColour`: 4:4:4
    /// from the start (else 4:2:0, switchable later).
    public init(host: String, port: UInt16 = GlassClient.defaultPort, device: GlassDevice? = GlassDevice.shared,
                mode: UInt8 = UInt8(GLASS_MODE_FIT), fullColour: Bool = false) throws {
        guard let device else { throw ConnectFailure(code: 10, host: host) }
        var err: Int32 = 0
        // Tell the host about this Mac's screen, so it can fit it.
        var hello = GLASS_ViewerHello()
        if let s = NSScreen.main {
            let px = s.convertRectToBacking(s.frame).size
            hello.display_w = UInt16(clamping: Int(px.width))
            hello.display_h = UInt16(clamping: Int(px.height))
            hello.refresh_hz = UInt16(clamping: s.maximumFramesPerSecond)
        }
        hello.caps = UInt8(GLASS_CAP_HEVC_444) | (fullColour ? 0 : UInt8(GLASS_CAP_PREFER_420))
        hello.mode = mode
        guard let p = glass_client_connect(device.ptr, host, port, hello, &err) else {
            throw ConnectFailure(code: err, host: host)
        }
        ptr = p
        hostOS = glass_client_host_os(p)
        hostCaps = glass_client_host_caps(p)
        var name = [CChar](repeating: 0, count: 128)
        glass_client_host_name(p, &name, name.count)
        hostName = String(cString: name)
        self.host = host
        self.port = port
    }

    deinit { close() }

    public var is444: Bool { info.chroma == UInt8(GLASS_CHROMA_444) }

    /// The host ended the session on purpose (said Bye), rather than the
    /// connection dropping (then the viewer reconnects).
    public var hostSaidBye: Bool { with(false) { glass_client_host_said_bye($0) } }

    /// Sends a clipboard item (`GLASS_CLIP_*`) to the host in the background
    /// (M5c); a files item sends the files themselves.
    public func sendClipboard(_ item: ClipboardSync.Item) {
        with(()) { p in
            item.data.withUnsafeBytes { b in
                let base = b.bindMemory(to: UInt8.self).baseAddress
                if item.kind == UInt8(GLASS_CLIP_FILES) {
                    glass_client_send_files(p, base, b.count)
                } else {
                    _ = glass_client_send_clipboard(p, item.kind, base, b.count)
                }
            }
        }
    }

    /// Text from the host, once.
    public func takeClipboard() -> ClipboardSync.Item? {
        with(nil) { p in
            guard let h = glass_client_take_clipboard(p) else { return nil }
            defer { glass_clip_free(h) }
            return ClipboardSync.item(h)
        }
    }

    /// A new offer (a picture or files) from the host, once.
    public func takeOffer() -> ClipboardSync.Offer? {
        with(nil) { p in
            guard let h = glass_client_take_offer(p) else { return nil }
            defer { glass_clip_free(h) }
            return ClipboardSync.offer(h)
        }
    }

    /// Pasted here: fetches offer `id` (0 still coming, 1 done, 2 failed).
    public func fetch(_ id: UInt32) -> (UInt8, ClipboardSync.Item?) {
        with((UInt8(2), ClipboardSync.Item?.none)) { p in
            var state: UInt8 = 0
            guard let h = glass_client_fetch(p, id, &state) else { return (state, nil) }
            defer { glass_clip_free(h) }
            return (state, ClipboardSync.item(h))
        }
    }

    /// Accept files from the host into `dir` (nil: refuse them).
    public func setFileDir(_ dir: URL?) {
        with(()) { p in
            if let path = dir?.path { glass_client_set_file_dir(p, path) } else { glass_client_set_file_dir(p, nil) }
        }
    }

    public var clipProgress: GLASS_ClipProgress { with(GLASS_ClipProgress()) { glass_client_clip_progress($0) } }

    /// The ring of decoded sound to play (M4b), once the host sends sound;
    /// the caller releases it (`AudioPlayer` does).
    public func audioRing() -> OpaquePointer? {
        with(nil) { glass_client_audio_ring($0) }
    }

    public var audioStats: GLASS_AudioStats {
        with(GLASS_AudioStats()) { glass_client_audio_stats($0) }
    }

    /// "sound (M4b): …", as on the PC.
    public var audioLine: String {
        with("") { p in
            var s = glass_client_audio_stats(p)
            var buf = [CChar](repeating: 0, count: 512)
            _ = glass_audio_stats_text(&s, &buf, buf.count)
            return String(cString: buf)
        }
    }

    /// "Wi-Fi spikes (M4): …", as on the PC (M4 step 5).
    public var spikesLine: String {
        with("") { p in
            var s = glass_client_spikes(p)
            var buf = [CChar](repeating: 0, count: 512)
            _ = glass_spike_text(&s, &buf, buf.count)
            return String(cString: buf)
        }
    }

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

    /// Counts the host's stream changes; re-read `info` when it moves.
    public var infoChanges: UInt32 { with(0) { glass_client_info_changes($0) } }

    /// Asks the host for view mode `mode`; `allow444` false asks for 4:2:0.
    public func setMode(_ mode: UInt8, allow444: Bool) {
        with(()) { glass_client_set_mode($0, mode, allow444) }
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

/// Serves a recording as a live stream on loopback (tests, demos), as an
/// in-memory host "Glass test host" that pairs with the fixed `pin`. Logs
/// the input it receives and sends a 16x16 test pointer.
public final class ReplayHost {
    private var ptr: OpaquePointer?
    public let port: UInt16

    public init(data: Data, pin: String, port: UInt16 = 0) throws {
        var out: UInt16 = 0
        let p = data.withUnsafeBytes { raw in
            glass_replay_start(raw.bindMemory(to: UInt8.self).baseAddress, raw.count, port, pin, &out)
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

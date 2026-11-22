import CGlassCore
import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

// Mac as host (M3b): core does pairing, TLS, sealing and packets; this file
// wraps that C ABI. Capture, encode and input injection live in
// HostCapture.swift, HostEncoder.swift and HostInput.swift.

/// Something the host's user should see.
public struct GlassHostEvent {
    public enum Kind { case pairingPin, paired, wrongPin, pairingLocked, refused, denied, loop, other }
    public let kind: Kind
    public let device: String
    public let pin: String
    public let address: String
    public let newPin: Bool
    public let triesLeft: Int

    init(_ e: GLASS_HostEventC) {
        func str<T>(_ tuple: T) -> String {
            withUnsafeBytes(of: tuple) { raw in
                String(cString: raw.bindMemory(to: CChar.self).baseAddress!)
            }
        }
        switch Int32(e.kind) {
        case GLASS_HOST_EVENT_PAIRING_PIN: kind = .pairingPin
        case GLASS_HOST_EVENT_PAIRED: kind = .paired
        case GLASS_HOST_EVENT_WRONG_PIN: kind = .wrongPin
        case GLASS_HOST_EVENT_PAIRING_LOCKED: kind = .pairingLocked
        case GLASS_HOST_EVENT_REFUSED: kind = .refused
        case GLASS_HOST_EVENT_DENIED: kind = .denied
        case GLASS_HOST_EVENT_LOOP: kind = .loop
        default: kind = .other
        }
        device = str(e.device)
        pin = str(e.pin)
        address = str(e.addr)
        newPin = e.new_pin
        triesLeft = Int(e.left)
    }

    /// One line for the host window / console.
    public var text: String {
        switch kind {
        case .pairingPin:
            let p = pin.count == 6 ? "\(pin.prefix(3)) \(pin.suffix(3))" : pin
            return "Pairing request from \u{201C}\(device)\u{201D} (\(address)). PIN: \(p) — type it on that device. Valid for 2 minutes."
        case .paired:
            return "Paired with \u{201C}\(device)\u{201D}. It can now view and control this Mac."
        case .wrongPin:
            return triesLeft == 0
                ? "Wrong PIN from \u{201C}\(device)\u{201D}. A new PIN will be shown on the next try."
                : "Wrong PIN from \u{201C}\(device)\u{201D}. \(triesLeft) more tries with this PIN."
        case .pairingLocked:
            return "Pairing is switched off after too many wrong PINs. Stop and start sharing to pair a device."
        case .refused:
            return "Refused a device that isn't paired with this Mac (\(address), id \(device))."
        case .denied:
            return "Did not allow \u{201C}\(device)\u{201D} to connect."
        case .loop:
            return "Refused \u{201C}\(device)\u{201D}: this Mac is already viewing it (directly or through another machine), so it would be a loop."
        case .other:
            return ""
        }
    }
}

/// Receives the viewer's input on core's UDP thread, in order.
public protocol GlassInputHandler: AnyObject {
    func handle(_ action: GLASS_InputActionC)
}

private final class EventBox {
    let onEvent: (GlassHostEvent) -> Void
    init(_ f: @escaping (GlassHostEvent) -> Void) { onEvent = f }
}

private let eventTrampoline: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<GLASS_HostEventC>?) -> Void = { ctx, e in
    guard let ctx, let e else { return }
    Unmanaged<EventBox>.fromOpaque(ctx).takeUnretainedValue().onEvent(GlassHostEvent(e.pointee))
}

private let inputTrampoline: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<GLASS_InputActionC>?) -> Void = { ctx, a in
    guard let ctx, let a else { return }
    (Unmanaged<AnyObject>.fromOpaque(ctx).takeUnretainedValue() as? GlassInputHandler)?.handle(a.pointee)
}

public struct HostStartFailure: Error, CustomStringConvertible {
    public let code: Int32
    public var description: String {
        code == 10
            ? "Couldn't read or create this Mac's Glass identity in ~/Library/Application Support/Glass."
            : "Couldn't listen for connections (is another copy of Glass sharing this Mac?)."
    }
}

/// Listens for paired viewers (and pairing requests) on a port.
public final class GlassHost {
    private let ptr: OpaquePointer
    private let events: EventBox
    public let port: UInt16
    public let device: GlassDevice

    /// `onEvent` runs on the accepting thread.
    public init(device: GlassDevice, port: UInt16 = GlassClient.defaultPort, fixedPin: String? = nil,
                onEvent: @escaping (GlassHostEvent) -> Void) throws {
        events = EventBox(onEvent)
        var err: Int32 = 0
        let ctx = Unmanaged.passUnretained(events).toOpaque()
        let p: OpaquePointer?
        // A Mac host: view modes if the virtual display works here, and 4:4:4.
        let os = UInt8(GLASS_HOST_OS_MAC)
        let caps = (VirtualDisplay.available ? UInt8(GLASS_HOST_CAP_VIEW_MODES) : 0) | UInt8(GLASS_HOST_CAP_444)
        if let fixedPin {
            p = glass_host_listen(device.ptr, port, fixedPin, os, caps, eventTrampoline, ctx, &err)
        } else {
            p = glass_host_listen(device.ptr, port, nil, os, caps, eventTrampoline, ctx, &err)
        }
        guard let p else { throw HostStartFailure(code: err) }
        ptr = p
        self.device = device
        self.port = glass_host_port(p)
    }

    deinit {
        glass_host_wake(ptr)
        glass_host_free(ptr)
    }

    public enum Accepted {
        case session(GlassHostSession)
        /// A pairing request or a refused device (see the events).
        case handled
        case failed
        case stopped
    }

    /// Blocks until a connection arrives and is handled. `input` stays alive
    /// for the session (the session holds it).
    public func accept(info: GLASS_StreamInfo, input: GlassInputHandler) -> Accepted {
        var status: Int32 = 0
        let ctx = Unmanaged.passUnretained(input as AnyObject).toOpaque()
        let s = glass_host_accept(ptr, info, inputTrampoline, ctx, &status)
        switch status {
        case 1:
            guard let s else { return .failed }
            return .session(GlassHostSession(s, input: input))
        case 0: return .handled
        case -2: return .stopped
        default: return .failed
        }
    }

    /// Makes a blocked `accept` return `.stopped` (and every later one).
    public func stop() { glass_host_wake(ptr) }
}

/// One connected viewer. Calls must not overlap: make them from one queue.
public final class GlassHostSession {
    private var ptr: OpaquePointer?
    private let input: GlassInputHandler // kept alive for core's callbacks
    public let peerName: String
    public let peerAddress: String

    init(_ p: OpaquePointer, input: GlassInputHandler) {
        ptr = p
        self.input = input
        var buf = [CChar](repeating: 0, count: 128)
        glass_session_peer_name(p, &buf, buf.count)
        peerName = String(cString: buf)
        glass_session_peer_addr(p, &buf, buf.count)
        peerAddress = String(cString: buf)
    }

    deinit { close() }

    public var connected: Bool { ptr.map { glass_session_connected($0) } ?? false }
    public var streaming: Bool { ptr.map { glass_session_streaming($0) } ?? false }
    public func takeKeyframeRequest() -> Bool { ptr.map { glass_session_take_keyframe_request($0) } ?? false }
    public var inputSeq: UInt32 { ptr.map { glass_session_input_seq($0) } ?? 0 }
    /// What the viewer said about itself: screen size, refresh, decode caps, wanted mode.
    public var viewer: GLASS_ViewerHello { ptr.map { glass_session_viewer($0) } ?? GLASS_ViewerHello() }

    /// A view-mode change the viewer asked for.
    public func takeModeRequest() -> (mode: UInt8, allow444: Bool)? {
        guard let ptr else { return nil }
        var m: UInt8 = 0
        var a = false
        return glass_session_take_mode_request(ptr, &m, &a) ? (m, a) : nil
    }

    /// Tells the viewer the stream changes from the next IDR on.
    @discardableResult
    public func sendStreamChange(_ info: GLASS_StreamInfo) -> Bool {
        ptr.map { glass_session_send_stream_change($0, info) } ?? false
    }
    public var counters: GLASS_HostCounters { ptr.map { glass_session_counters($0) } ?? GLASS_HostCounters() }

    /// Sends an Annex-B frame. 1 sent, 0 not yet (viewer address unknown), -1 error.
    @discardableResult
    public func send(_ frame: GLASS_OutFrame, _ data: UnsafeRawBufferPointer) -> Int32 {
        guard let ptr else { return -1 }
        var f = frame
        return glass_session_send_frame(ptr, &f, data.bindMemory(to: UInt8.self).baseAddress, data.count)
    }

    /// RGBA (straight alpha), top row first; at most 96 x 96.
    public func sendCursor(serial: UInt32, width: Int, height: Int, hotX: Int, hotY: Int, rgba: [UInt8]) {
        guard let ptr else { return }
        rgba.withUnsafeBufferPointer {
            _ = glass_session_send_cursor(ptr, serial, UInt16(width), UInt16(height), UInt16(hotX), UInt16(hotY), $0.baseAddress, $0.count)
        }
    }

    public func sendNotice(_ code: UInt8) {
        guard let ptr else { return }
        _ = glass_session_send_notice(ptr, code)
    }

    /// Sends a clipboard item to the viewer in the background (M5c).
    public func sendClipboard(_ item: ClipboardSync.Item) {
        guard let ptr else { return }
        item.data.withUnsafeBytes { b in
            _ = glass_session_send_clipboard(ptr, item.kind, b.bindMemory(to: UInt8.self).baseAddress, b.count)
        }
    }

    /// The latest clipboard item from the viewer, once.
    public func takeClipboard() -> ClipboardSync.Item? {
        guard let ptr, let h = glass_session_take_clipboard(ptr) else { return nil }
        defer { glass_clip_free(h) }
        return (glass_clip_kind(h), Data(bytes: glass_clip_data(h), count: glass_clip_len(h)))
    }

    /// Ends the session (keys the viewer held are released first).
    public func close() {
        if let p = ptr {
            ptr = nil
            glass_session_close(p)
        }
    }
}

/// HID usage -> Mac key code (nil if the Mac has no such key).
public func glassMacKey(hid: UInt8, iso: Bool) -> UInt16? {
    let k = glass_keymap_hid_to_mac(hid, iso)
    return k == 0xFFFF ? nil : k
}

/// This Mac's IPv4 addresses on the local network (Wi-Fi / Ethernet first).
public func localIPv4Addresses() -> [String] {
    var out: [(String, String)] = []
    var ifap: UnsafeMutablePointer<ifaddrs>?
    guard getifaddrs(&ifap) == 0, let first = ifap else { return [] }
    defer { freeifaddrs(ifap) }
    for p in sequence(first: first, next: { $0.pointee.ifa_next }) {
        let i = p.pointee
        guard let sa = i.ifa_addr, sa.pointee.sa_family == UInt8(AF_INET),
              (Int32(i.ifa_flags) & IFF_UP) != 0, (Int32(i.ifa_flags) & IFF_LOOPBACK) == 0 else { continue }
        var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
        if getnameinfo(sa, socklen_t(sa.pointee.sa_len), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0 {
            let name = String(cString: i.ifa_name)
            let ip = String(cString: host)
            if !ip.hasPrefix("169.254.") { out.append((name, ip)) }
        }
    }
    // en0/en1 (Wi-Fi, Ethernet) before bridges and VPN tunnels.
    return out.sorted { ($0.0.hasPrefix("en") ? 0 : 1, $0.0) < ($1.0.hasPrefix("en") ? 0 : 1, $1.0) }.map(\.1)
}

/// Advertises this host as `_glass._tcp` so viewers can list it. Advisory:
/// trust comes from pairing, never from discovery.
public final class GlassAdvertiser: NSObject, NetServiceDelegate {
    private var service: NetService?

    override public init() { super.init() }

    public func start(name: String, port: UInt16, fingerprintPrefix: String, ip: String?) {
        let s = NetService(domain: "local.", type: "_glass._tcp.", name: name, port: Int32(port))
        var txt: [String: Data] = [
            "v": Data(String(glass_protocol_version()).utf8),
            "fp": Data(fingerprintPrefix.utf8),
            "port": Data(String(port).utf8),
            "os": Data("mac".utf8),
        ]
        if let ip { txt["ip"] = Data(ip.utf8) }
        s.setTXTRecord(NetService.data(fromTXTRecord: txt))
        s.delegate = self
        s.publish()
        service = s
    }

    public func stop() {
        service?.stop()
        service = nil
    }
}

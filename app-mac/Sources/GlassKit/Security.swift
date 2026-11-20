import CGlassCore
import Foundation
import Network
import SystemConfiguration

/// This Mac's Glass identity (a key and certificate made on first run) and
/// the devices it has paired with. Stored by core in
/// ~/Library/Application Support/Glass (or `GLASS_HOME`), private to this
/// user (mode 0600). LATER: the Keychain, once builds are signed with a
/// stable certificate (the Keychain re-prompts for each ad-hoc build).
public final class GlassDevice {
    let ptr: OpaquePointer

    /// How other devices see this Mac: its Computer Name ("Simon's MacBook Air").
    public static var computerName: String {
        (SCDynamicStoreCopyComputerName(nil, nil) as String?) ?? "Mac"
    }

    /// The identity in the default folder; nil if it can't be read or created.
    public static let shared: GlassDevice? = GlassDevice(directory: nil, name: computerName)

    public init?(directory: String?, name: String) {
        let p: OpaquePointer?
        if let directory {
            p = glass_device_open(directory, name)
        } else {
            p = glass_device_open(nil, name)
        }
        guard let p else { return nil }
        ptr = p
    }

    deinit { glass_device_free(ptr) }

    public struct Peer: Equatable {
        public let fingerprint: [UInt8]
        public let name: String
        /// GLASS_ROLE_* bits: 1 = a host this Mac views, 2 = may view this Mac.
        public let roles: UInt8
        public var isHost: Bool { roles & 1 != 0 }
        public var hex: String { fingerprint.map { String(format: "%02x", $0) }.joined() }
        /// "3f2a 91c0 77de 10ab", as `glass devices` prints it on the PC.
        public var shortID: String {
            stride(from: 0, to: 8, by: 2).map { String(format: "%02x%02x", fingerprint[$0], fingerprint[$0 + 1]) }.joined(separator: " ")
        }
    }

    public var peers: [Peer] {
        var out: [Peer] = []
        let n = glass_device_peer_count(ptr)
        for i in 0..<n {
            var fp = [UInt8](repeating: 0, count: 32)
            var name = [CChar](repeating: 0, count: 128)
            let roles = glass_device_peer(ptr, i, &fp, &name, name.count)
            if roles != 0 { out.append(Peer(fingerprint: fp, name: String(cString: name), roles: roles)) }
        }
        return out
    }

    @discardableResult
    public func forget(_ peer: Peer) -> Bool {
        glass_device_forget(ptr, peer.fingerprint)
    }

    /// The paired host whose fingerprint starts with `hexPrefix` (from discovery).
    public func pairedHost(prefix hexPrefix: String?) -> Peer? {
        guard let p = hexPrefix?.lowercased(), p.count >= 8 else { return nil }
        return peers.first { $0.isHost && $0.hex.hasPrefix(p) }
    }
}

/// A pairing in progress: the host is showing a 6-digit PIN.
public final class GlassPairing {
    private var ptr: OpaquePointer?
    public let hostName: String
    public let host: String

    /// Asks `host` to pair (it then shows a PIN). Blocks briefly; call off the main thread.
    public init(device: GlassDevice, host: String, port: UInt16) throws {
        var err: Int32 = 0
        guard let p = glass_pair_begin(device.ptr, host, port, &err) else {
            throw ConnectFailure(code: err, host: host)
        }
        ptr = p
        var buf = [CChar](repeating: 0, count: 128)
        glass_pairing_host_name(p, &buf, buf.count)
        hostName = String(cString: buf)
        self.host = host
    }

    /// Runs the PIN exchange (once). Throws `ConnectFailure` (code 8 = wrong PIN).
    public func finish(pin: String) throws {
        guard let p = ptr else { return }
        ptr = nil
        var err: Int32 = 0
        if !glass_pair_finish(p, pin, &err) {
            throw ConnectFailure(code: err, host: host)
        }
    }

    /// Tells the host to stop waiting.
    public func cancel() {
        if let p = ptr {
            ptr = nil
            glass_pairing_cancel(p)
        }
    }

    deinit { cancel() }
}

/// A Glass host found on the local network (`_glass._tcp`).
public struct DiscoveredHost: Equatable {
    public let name: String
    public let address: String
    public let port: UInt16
    /// First 8 bytes of the host's certificate fingerprint, hex (from TXT).
    public let fingerprintPrefix: String?
    public let version: String?
}

/// Lists Glass hosts via Bonjour. Typing an address always works too; some
/// routers (client isolation, mDNS filtering) hide hosts from each other.
public final class GlassBrowser {
    private var browser: NWBrowser?
    public var onChange: (([DiscoveredHost]) -> Void)?

    public init() {}

    public func start() {
        guard browser == nil else { return }
        let b = NWBrowser(for: .bonjourWithTXTRecord(type: "_glass._tcp", domain: nil), using: .tcp)
        b.browseResultsChangedHandler = { [weak self] results, _ in
            let hosts = results.compactMap { r -> DiscoveredHost? in
                guard case let .service(name, _, _, _) = r.endpoint,
                      case let .bonjour(txt) = r.metadata,
                      let ip = txt["ip"] else { return nil }
                let port = txt["port"].flatMap { UInt16($0) } ?? GlassClient.defaultPort
                return DiscoveredHost(name: name, address: ip, port: port, fingerprintPrefix: txt["fp"], version: txt["v"])
            }.sorted { $0.name < $1.name }
            self?.onChange?(hosts)
        }
        b.start(queue: .main)
        browser = b
    }

    public func stop() {
        browser?.cancel()
        browser = nil
    }
}

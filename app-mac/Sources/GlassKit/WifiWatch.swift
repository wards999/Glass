import CoreWLAN
import CGlassCore
import Foundation

/// This Mac's Wi-Fi link during a session (M4 step 5): signal, transmit
/// rate, channel and (where macOS tells, which needs Location access) the
/// access point, sampled once a second by CoreWLAN and tracked by core, so
/// the results say "Wi-Fi here: …" as on the PC. Read only.
public final class WifiWatch {
    private let track: OpaquePointer
    private let lock = NSLock()
    private let q = DispatchQueue(label: "glass.wifi", qos: .utility)
    private var timer: DispatchSourceTimer?

    public init() {
        track = glass_link_track_new()
        let t = DispatchSource.makeTimerSource(queue: q)
        t.schedule(deadline: .now(), repeating: 1.0)
        t.setEventHandler { [weak self] in self?.sample() }
        t.resume()
        timer = t
    }

    deinit {
        timer?.cancel()
        glass_link_track_free(track)
    }

    private func sample() {
        var s = GLASS_LinkSample()
        if let i = CWWiFiClient.shared().interface(), i.powerOn(), i.rssiValue() != 0 {
            s.wifi = true
            s.rssi_dbm = Int32(i.rssiValue())
            s.rate_mbps = UInt32(max(0, i.transmitRate()))
            s.channel = UInt32(max(0, i.wlanChannel()?.channelNumber ?? 0))
            if let b = i.bssid() {
                let bytes = b.split(separator: ":").compactMap { UInt8($0, radix: 16) }
                if bytes.count == 6 { s.bssid = (bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]) }
            }
        }
        lock.withLock { glass_link_track_feed(track, &s) }
    }

    /// "Wi-Fi here: signal … | rate … | roams …".
    public func line() -> String {
        var buf = [CChar](repeating: 0, count: 512)
        lock.withLock { _ = glass_link_track_text(track, &buf, buf.count) }
        return String(cString: buf)
    }
}

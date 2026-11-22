import AppKit
import CGlassCore

/// Clipboard sharing for a session (M5c, step 1): text and images, both
/// ways. Main thread: `poll()` notices when this Mac's pasteboard changed
/// (its change count) and hands a new item to `send`; `put` puts an item
/// from the other side on the pasteboard. Text travels as UTF-8, images as
/// PNG (TIFF-only images are converted off the main thread).
public final class ClipboardSync {
    public typealias Item = (kind: UInt8, data: Data)
    private let pb = NSPasteboard.general
    private var seen: Int
    /// The last item sent or put: re-copying the same thing isn't news, and
    /// what arrived isn't sent back.
    private var last: Int?
    private let send: (Item) -> Void
    private let convertQ = DispatchQueue(label: "glass.clipboard", qos: .utility)
    public private(set) var sent = 0
    public private(set) var received = 0

    /// `sendCurrent`: also send what's on the pasteboard now (the viewer
    /// does: copy, connect, paste). `send` runs on the main thread.
    public init(sendCurrent: Bool, send: @escaping (Item) -> Void) {
        seen = sendCurrent ? -1 : NSPasteboard.general.changeCount
        self.send = send
    }

    private static func key(_ kind: UInt8, _ data: Data) -> Int {
        var h = Hasher()
        h.combine(kind)
        data.withUnsafeBytes { h.combine(bytes: $0) } // all of it
        return h.finalize()
    }

    private func deliver(_ kind: UInt8, _ data: Data) {
        guard !data.isEmpty, data.count <= 16 << 20 else { return } // GLASS_CLIP_MAX
        let k = Self.key(kind, data)
        guard k != last else { return }
        last = k
        sent += 1
        send((kind, data))
    }

    /// Call every 100-250 ms on the main thread.
    public func poll() {
        let now = pb.changeCount
        guard now != seen else { return }
        seen = now
        // Text first (a spreadsheet's cells also come as a picture).
        if let s = pb.string(forType: .string), !s.isEmpty {
            deliver(UInt8(GLASS_CLIP_TEXT), Data(s.replacingOccurrences(of: "\r\n", with: "\n").utf8))
        } else if let png = pb.data(forType: .png) {
            deliver(UInt8(GLASS_CLIP_PNG), png)
        } else if let tiff = pb.data(forType: .tiff) {
            let count = now
            convertQ.async { [weak self] in
                guard let png = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:]) else { return }
                DispatchQueue.main.async {
                    guard let self, self.pb.changeCount == count else { return } // copied again meanwhile
                    self.deliver(UInt8(GLASS_CLIP_PNG), png)
                }
            }
        }
    }

    /// An item from the other side (main thread).
    public func put(_ item: Item) {
        received += 1
        last = Self.key(item.kind, item.data)
        switch item.kind {
        case UInt8(GLASS_CLIP_TEXT):
            guard let s = String(data: item.data, encoding: .utf8) else { return }
            pb.clearContents()
            pb.setString(s, forType: .string)
        case UInt8(GLASS_CLIP_PNG):
            pb.clearContents()
            pb.setData(item.data, forType: .png)
        default:
            return
        }
        seen = pb.changeCount
    }

    public var summary: String { "\(sent) sent, \(received) received" }

    /// The Settings switch (on unless turned off).
    public static var enabled: Bool {
        UserDefaults.standard.object(forKey: "shareClipboard") as? Bool ?? true
    }
}

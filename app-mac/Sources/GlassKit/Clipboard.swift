import AppKit
import CGlassCore

/// Clipboard sharing for a session (M5c): text, images and files, both
/// ways. Main thread: `poll()` notices when this Mac's pasteboard changed
/// (its change count) and hands a new item to `send`; `put` puts an item
/// from the other side on the pasteboard. Text travels as UTF-8, images as
/// PNG (TIFF-only images are converted off the main thread). Files copied
/// in Finder are a `GLASS_CLIP_FILES` item of NUL-separated paths: the
/// session sends the files themselves (core), and received files go on the
/// pasteboard as file URLs, so Finder pastes copies of them.
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

    /// Where this Mac keeps files that arrive over the clipboard.
    public static func fileDir(role: String) -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("Glass/clipboard-\(role)", isDirectory: true)
    }

    /// A files item for these paths, and back.
    public static func filesItem(_ urls: [URL]) -> Item {
        (UInt8(GLASS_CLIP_FILES), Data(urls.map { $0.path }.joined(separator: "\0").utf8))
    }

    public static func itemFiles(_ item: Item) -> [URL] {
        String(decoding: item.data, as: UTF8.self).split(separator: "\0").map { URL(fileURLWithPath: String($0)) }
    }

    /// Files that came over a Glass clipboard: never sent back.
    private static func fromGlass(_ urls: [URL]) -> Bool {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("Glass").standardizedFileURL.path
        return urls.allSatisfy { $0.standardizedFileURL.path.hasPrefix(root + "/") }
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
        // Files first (Finder also puts their names as text), then text (a
        // spreadsheet's cells also come as a picture), then a picture.
        if let urls = pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL],
           !urls.isEmpty {
            if !Self.fromGlass(urls) {
                let f = Self.filesItem(urls)
                deliver(f.kind, f.data)
            }
        } else if let s = pb.string(forType: .string), !s.isEmpty {
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
        case UInt8(GLASS_CLIP_FILES):
            let urls = Self.itemFiles(item)
            guard !urls.isEmpty else { return }
            pb.clearContents()
            pb.writeObjects(urls as [NSURL])
        default:
            return
        }
        seen = pb.changeCount
    }

    public var summary: String { "\(sent) sent, \(received) received" }

    /// "Sending files to “QUIETPC” … 45%" while files are on their way.
    public static func progressLine(_ p: GLASS_ClipProgress, peer: String) -> String? {
        let t: (verb: String, done: UInt64, total: UInt64)
        if p.out_total > 0, p.out_done < p.out_total {
            t = ("Sending files to", p.out_done, p.out_total)
        } else if p.in_total > 0, p.in_done < p.in_total {
            t = ("Receiving files from", p.in_done, p.in_total)
        } else {
            return nil
        }
        let mb = { (b: UInt64) in String(format: "%.1f", Double(b) / 1e6) }
        return "\(t.verb) \u{201C}\(peer)\u{201D} \u{2026} \(t.done * 100 / t.total)%  (\(mb(t.done)) of \(mb(t.total)) MB). They can be pasted once they\u{2019}re all there."
    }

    /// File counts for the results.
    public static func filesSummary(_ p: GLASS_ClipProgress) -> String {
        "files: \(p.files_sent) copies sent, \(p.files_received) received, \(p.not_sent) not sent (over 200 MB, too many or unreadable)"
    }

    /// The Settings switch (on unless turned off).
    public static var enabled: Bool {
        UserDefaults.standard.object(forKey: "shareClipboard") as? Bool ?? true
    }
}

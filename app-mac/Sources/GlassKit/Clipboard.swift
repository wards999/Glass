import AppKit
import CGlassCore

/// Clipboard sharing for a session (M5c): text, pictures and files, both
/// ways, on the main thread, through the session's clipboard link (core).
///
/// `poll()` (every ~150 ms):
/// - text copied on this Mac goes to the other side at once; text from
///   there goes straight on the pasteboard;
/// - a picture or files copied here are only *offered*. An offer from the
///   other side goes on the pasteboard as lazy items (a data provider):
///   only when an app pastes does it ask, and the picture or files are
///   fetched then, as Remote Desktop does. While a paste waits, the run
///   loop keeps going, so the viewer keeps drawing.
///
/// Pictures travel as PNG (TIFF-only ones are converted off the main
/// thread). Files copied in Finder are offered by path; pasted files land
/// in Glass's folder first and are handed over as file URLs.
public final class ClipboardSync: NSObject, NSPasteboardItemDataProvider {
    public typealias Item = (kind: UInt8, data: Data)

    /// Something the other side copied and offers.
    public struct Offer {
        public let id: UInt32
        /// `GLASS_CLIP_PNG` or `GLASS_CLIP_FILES`.
        public let kind: UInt8
        public let size: UInt64
        /// Files: where the top-level files will be once fetched.
        public let files: [URL]
        public init(id: UInt32, kind: UInt8, size: UInt64, files: [URL]) {
            self.id = id; self.kind = kind; self.size = size; self.files = files
        }
    }

    /// The session's side of it (all called on the main thread).
    public struct Endpoint {
        public var send: (Item) -> Void
        public var takeText: () -> Item?
        public var takeOffer: () -> Offer?
        /// 0 still coming, 1 done (with the picture or file paths), 2 failed.
        public var fetch: (UInt32) -> (UInt8, Item?)
        /// How far a paste's files have come, for the progress panel.
        public var progress: (() -> GLASS_ClipProgress)?
        /// The other side's name, for the progress panel.
        public var peer: String
        public init(send: @escaping (Item) -> Void, takeText: @escaping () -> Item?,
                    takeOffer: @escaping () -> Offer?, fetch: @escaping (UInt32) -> (UInt8, Item?),
                    progress: (() -> GLASS_ClipProgress)? = nil, peer: String = "") {
            self.send = send; self.takeText = takeText; self.takeOffer = takeOffer; self.fetch = fetch
            self.progress = progress; self.peer = peer
        }
    }

    /// Marks Glass's own promises (never read back: reading would fetch).
    private static let marker = NSPasteboard.PasteboardType("com.glass.promise")
    /// Asks clipboard managers to leave them alone (nspasteboard.org).
    private static let transient = NSPasteboard.PasteboardType("org.nspasteboard.TransientType")

    private let pb = NSPasteboard.general
    private let ep: Endpoint
    private var seen: Int
    /// The last thing sent or put: re-copying the same thing isn't news,
    /// and what arrived isn't sent back.
    private var last: Int?
    private var offer: Offer?
    private var got: Item?
    /// Pasteboard item -> its file (for a files offer).
    private var itemFile: [ObjectIdentifier: URL] = [:]
    private var waiting = false
    private let convertQ = DispatchQueue(label: "glass.clipboard", qos: .utility)

    /// `sendCurrent`: also send what's on the pasteboard now (the viewer
    /// does: copy, connect, paste).
    public init(sendCurrent: Bool, endpoint: Endpoint) {
        seen = sendCurrent ? -1 : NSPasteboard.general.changeCount
        ep = endpoint
        super.init()
    }

    private static func key(_ kind: UInt8, _ data: Data) -> Int {
        var h = Hasher()
        h.combine(kind)
        data.withUnsafeBytes { h.combine(bytes: $0) } // all of it
        return h.finalize()
    }

    /// An offer handle from core (`glass_*_take_offer`) as an `Offer`.
    public static func offer(_ h: OpaquePointer) -> Offer {
        let kind = glass_clip_kind(h)
        let data = Data(bytes: glass_clip_data(h), count: glass_clip_len(h))
        let files = kind == UInt8(GLASS_CLIP_FILES) ? itemFiles((kind, data)) : []
        return Offer(id: glass_clip_id(h), kind: kind, size: glass_clip_size(h), files: files)
    }

    /// A clipboard handle's kind and bytes.
    public static func item(_ h: OpaquePointer) -> Item {
        (glass_clip_kind(h), Data(bytes: glass_clip_data(h), count: glass_clip_len(h)))
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

    /// Files that came over a Glass clipboard: never offered back.
    private static func fromGlass(_ urls: [URL]) -> Bool {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("Glass").standardizedFileURL.path
        return urls.allSatisfy { $0.standardizedFileURL.path.hasPrefix(root + "/") }
    }

    private func deliver(_ kind: UInt8, _ data: Data) {
        guard !data.isEmpty else { return }
        if kind != UInt8(GLASS_CLIP_FILES), data.count > 16 << 20 { return } // GLASS_CLIP_MAX
        let k = Self.key(kind, data)
        guard k != last else { return }
        last = k
        ep.send((kind, data))
    }

    /// Call every 100-250 ms on the main thread.
    public func poll() {
        guard !waiting else { return } // a paste is being answered
        if let t = ep.takeText(), let s = String(data: t.data, encoding: .utf8) {
            last = Self.key(t.kind, t.data)
            pb.clearContents()
            pb.setString(s, forType: .string)
            seen = pb.changeCount
        }
        if let o = ep.takeOffer() { promise(o) }
        let now = pb.changeCount
        guard now != seen else { return }
        seen = now
        if pb.types?.contains(Self.marker) == true { return } // Glass's own promise
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

    /// Puts a promise of the other side's offer on the pasteboard.
    private func promise(_ o: Offer) {
        var items: [NSPasteboardItem] = []
        itemFile = [:]
        if o.kind == UInt8(GLASS_CLIP_FILES) {
            for url in o.files {
                let item = NSPasteboardItem()
                item.setDataProvider(self, forTypes: [.fileURL])
                itemFile[ObjectIdentifier(item)] = url
                items.append(item)
            }
        } else if o.kind == UInt8(GLASS_CLIP_PNG) {
            let item = NSPasteboardItem()
            item.setDataProvider(self, forTypes: [.png])
            items.append(item)
        }
        guard let first = items.first else { return }
        first.setString("1", forType: Self.marker)
        first.setString("1", forType: Self.transient)
        pb.clearContents()
        pb.writeObjects(items)
        seen = pb.changeCount
        offer = o
        got = nil
    }

    /// Fetches the offer (once), keeping the run loop going meanwhile.
    private func fetched() -> Item? {
        if let got { return got }
        guard let o = offer else { return nil }
        waiting = true
        // Files: a panel once it takes a moment (Finder is stuck until
        // they're all here, with a beachball).
        var panel: PastePanel?
        defer {
            waiting = false
            panel?.close()
        }
        let start = Date()
        var shown = Date.distantPast
        let limit = Date(timeIntervalSinceNow: 30 + Double(o.size) / 3_750_000) // fetch_timeout
        while Date() < limit {
            let (state, item) = ep.fetch(o.id)
            if state == 1 { got = item; return item }
            if state == 2 { return nil }
            if o.kind == UInt8(GLASS_CLIP_FILES), let progress = ep.progress,
               Date().timeIntervalSince(start) > 0.3, Date().timeIntervalSince(shown) > 0.25 {
                if panel == nil {
                    panel = PastePanel(peer: ep.peer)
                    panel?.show()
                }
                let p = progress()
                panel?.update(done: p.in_done, total: max(p.in_total, o.size))
                shown = Date()
            }
            RunLoop.current.run(mode: .default, before: Date(timeIntervalSinceNow: 0.02))
        }
        return nil
    }

    // MARK: NSPasteboardItemDataProvider

    public func pasteboard(_ pasteboard: NSPasteboard?, item: NSPasteboardItem, provideDataForType type: NSPasteboard.PasteboardType) {
        guard let o = offer, fetched() != nil else { return }
        if o.kind == UInt8(GLASS_CLIP_FILES), type == .fileURL, let url = itemFile[ObjectIdentifier(item)] {
            item.setString(url.absoluteString, forType: .fileURL)
        } else if type == .png, let g = got {
            item.setData(g.data, forType: .png)
        }
    }

    public func pasteboardFinishedWithDataProvider(_ pasteboard: NSPasteboard) {}

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
        return "\(t.verb) \u{201C}\(peer)\u{201D} \u{2026} \(t.done * 100 / t.total)%  (\(mb(t.done)) of \(mb(t.total)) MB). The paste finishes once they\u{2019}re all there."
    }

    /// The results line (same wording as on Windows).
    public static func summary(_ p: GLASS_ClipProgress, other: String) -> String {
        let why = p.not_sent_why == 1 ? " (last: over 10 000 files)"
            : p.not_sent_why == 2 ? " (last: a file couldn\u{2019}t be read)" : ""
        return "text and pictures: \(p.items_sent) sent to the \(other), \(p.items_received) received | files: \(p.files_offered) copies offered, \(p.files_sent) pasted on the \(other), \(p.files_received) pasted here | not offered: \(p.not_sent)\(why)\(dataLine(p))"
    }

    /// File data moved, how long it took and the speeds set (same wording
    /// as `ClipProgress::data_line` in core).
    static func dataLine(_ p: GLASS_ClipProgress) -> String {
        let all = p.bytes_out + p.bytes_in
        if all == 0 { return "" }
        let mb = { (b: UInt64) in Double(b) / 1e6 }
        let secs = Double(p.busy_us) / 1e6
        let avg = secs > 0 ? mb(all) / secs : 0
        let f = { (x: Double) in String(format: "%.1f", x) }
        return " | file data: \(f(mb(p.bytes_out))) MB sent, \(f(mb(p.bytes_in))) MB received in \(f(secs)) s (\(f(avg)) MB/s), speed set \(f(mb(p.rate_lo)))\u{2013}\(f(mb(p.rate_hi))) MB/s"
    }

    /// The Settings switch (on unless turned off).
    public static var enabled: Bool {
        UserDefaults.standard.object(forKey: "shareClipboard") as? Bool ?? true
    }
}

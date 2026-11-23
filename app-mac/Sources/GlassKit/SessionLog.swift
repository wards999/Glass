import Foundation

/// Running session logs, as on Windows (`slog.rs`): while a session is on,
/// its results are rewritten every 10 s, so a session that ends abruptly
/// (a crash, Glass quit, a later session replacing "the last results")
/// still leaves its numbers. `~/Library/Logs/Glass/<kind>-<unix seconds>.txt`
/// (Console.app shows them); the newest 20 are kept.
public final class SessionLog {
    /// How often a running session's log is rewritten (µs).
    public static let every: UInt64 = 10_000_000
    /// The reason a running report gives.
    public static let running = "still running"
    private static let keep = 20

    public static var dir: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Logs/Glass", isDirectory: true)
    }

    private let url: URL
    private var last: UInt64 = 0

    /// A new log for a `kind` session ("viewer", "host").
    public init(kind: String) {
        let d = Self.dir
        try? FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        Self.prune(d)
        url = d.appendingPathComponent("\(kind)-\(Int(Date().timeIntervalSince1970)).txt")
    }

    /// True once every [`every`] µs: time to write a running report.
    public func due(_ nowUs: UInt64) -> Bool {
        guard nowUs &- last >= Self.every else { return false }
        last = nowUs
        return true
    }

    /// Replaces the log with `text` (written whole, then moved into place).
    public func write(_ text: String) {
        try? text.write(to: url, atomically: true, encoding: .utf8)
    }

    private static func logs(_ d: URL) -> [(Date, URL)] {
        let urls = (try? FileManager.default.contentsOfDirectory(
            at: d, includingPropertiesForKeys: [.contentModificationDateKey])) ?? []
        return urls.filter { $0.pathExtension == "txt" }
            .compactMap { u in
                (try? u.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate).map { ($0, u) }
            }
            .sorted { $0.0 < $1.0 }
    }

    private static func prune(_ d: URL) {
        let v = logs(d)
        for (_, u) in v.prefix(max(0, v.count - (keep - 1))) {
            try? FileManager.default.removeItem(at: u)
        }
    }

    /// The newest session log, if any.
    public static func latest() -> String? {
        guard let (_, u) = logs(dir).last else { return nil }
        return try? String(contentsOf: u, encoding: .utf8)
    }
}

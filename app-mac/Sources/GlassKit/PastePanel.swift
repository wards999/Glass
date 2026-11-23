import AppKit
import CGlassCore

/// A small floating panel while a paste waits for files from the other side
/// (Finder blocks on Cmd+V until they're all here, and shows a beachball:
/// m4-test6, 1.37 GB took 44 s). Top centre of the screen, above other
/// windows, never takes focus. The viewer sees it too: it's on the screen
/// being shared.
final class PastePanel {
    private let panel: NSPanel
    private let title = NSTextField(labelWithString: "")
    private let detail = NSTextField(labelWithString: "")
    private let bar = NSProgressIndicator()
    /// For the speed: (time, bytes) a second or so ago.
    private var mark: (Date, UInt64)?
    private var speed: Double = 0

    init(peer: String) {
        panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 420, height: 92),
                        styleMask: [.nonactivatingPanel, .titled, .hudWindow, .utilityWindow],
                        backing: .buffered, defer: false)
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.hidesOnDeactivate = false
        panel.isReleasedWhenClosed = false
        panel.title = "Glass"
        title.stringValue = peer.isEmpty ? "Pasting\u{2026}" : "Pasting from \u{201C}\(peer)\u{201D}\u{2026}"
        title.font = .systemFont(ofSize: 13, weight: .semibold)
        detail.font = .monospacedDigitSystemFont(ofSize: 11, weight: .regular)
        detail.textColor = .secondaryLabelColor
        bar.isIndeterminate = false
        bar.minValue = 0
        bar.maxValue = 1
        bar.controlSize = .small
        let stack = NSStackView(views: [title, bar, detail])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 6
        stack.edgeInsets = NSEdgeInsets(top: 10, left: 14, bottom: 10, right: 14)
        stack.translatesAutoresizingMaskIntoConstraints = false
        let content = panel.contentView!
        content.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            stack.topAnchor.constraint(equalTo: content.topAnchor),
            stack.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            bar.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -28),
        ])
        if let screen = NSScreen.main {
            let v = screen.visibleFrame
            panel.setFrameOrigin(NSPoint(x: v.midX - panel.frame.width / 2, y: v.maxY - panel.frame.height - 24))
        }
    }

    func show() { panel.orderFrontRegardless() }

    func close() { panel.orderOut(nil) }

    /// `done` of `total` bytes here so far.
    func update(done: UInt64, total: UInt64) {
        let now = Date()
        if let m = mark {
            let (t, b) = m
            let dt = now.timeIntervalSince(t)
            if dt >= 1 {
                let s = Double(done &- b) / dt
                speed = speed == 0 ? s : speed * 0.6 + s * 0.4
                mark = (now, done)
            }
        } else {
            mark = (now, done)
        }
        let mb = { (b: UInt64) in String(format: "%.0f", Double(b) / 1e6) }
        let frac = total > 0 ? Double(done) / Double(total) : 0
        bar.doubleValue = frac
        var line = "\(mb(done)) of \(mb(total)) MB \u{00B7} \(Int(frac * 100))%"
        if speed > 0 {
            line += String(format: " \u{00B7} %.0f MB/s", speed / 1e6)
            let left = Double(total &- done) / speed
            if left >= 1 { line += " \u{00B7} about \(Self.duration(left)) left" }
        }
        detail.stringValue = line + "\nFinder waits until it\u{2019}s all here."
    }

    private static func duration(_ s: Double) -> String {
        s < 60 ? "\(Int(s.rounded())) s" : "\(Int(s / 60)) min \(Int(s) % 60) s"
    }
}

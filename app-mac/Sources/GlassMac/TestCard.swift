import AppKit
import GlassKit

/// Clarity test card (M3c): small and coloured text, 1-pixel lines,
/// checkerboards and coloured stripes, which show at a glance whether text
/// stays sharp and colours stay clean (4:2:0 smears 1-pixel colour detail).
/// While it is open the host measures sharpness (PSNR) of what it sends.
final class TestCardController: NSObject, NSWindowDelegate {
    /// One card, opened from Settings; sharing tells it what's being sent.
    static let shared = TestCardController()
    private var window: NSWindow?
    private var observer: Any?

    /// What the PC is being sent ("fit, more space 2560x1440 4:4:4 ..."),
    /// shown in a banner so each view can be told apart on the PC.
    var nowShowing = "the Mac's screen" {
        didSet {
            (window?.contentView as? TestCardView)?.nowShowing = nowShowing
            // A view change resizes the screen: keep the card on it.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in self?.fit() }
        }
    }

    /// Sizes the card to fit inside the screen it's on (the "larger" view
    /// is only 720 points tall).
    private func fit() {
        guard let w = window, let vf = (w.screen ?? NSScreen.main)?.visibleFrame else { return }
        let size = NSSize(width: min(1100, vf.width - 40), height: min(720, vf.height - 40))
        let frame = w.frameRect(forContentRect: NSRect(origin: .zero, size: size))
        let fitted = NSSize(width: min(frame.width, vf.width), height: min(frame.height, vf.height))
        w.setFrame(NSRect(x: vf.midX - fitted.width / 2, y: vf.midY - fitted.height / 2,
                          width: fitted.width, height: fitted.height), display: true)
    }

    func show() {
        if let w = window {
            w.makeKeyAndOrderFront(nil)
            return
        }
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1100, height: 720),
                         styleMask: [.titled, .closable, .resizable, .miniaturizable], backing: .buffered, defer: false)
        w.title = "Glass Clarity Test Card"
        w.isReleasedWhenClosed = false
        let view = TestCardView()
        view.nowShowing = nowShowing
        w.contentView = view
        w.delegate = self
        window = w
        fit()
        observer = NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main
        ) { [weak self] _ in self?.fit() }
        w.makeKeyAndOrderFront(nil)
        HostRunner.measureQuality = true
    }

    func windowWillClose(_ notification: Notification) {
        HostRunner.measureQuality = false
        if let o = observer { NotificationCenter.default.removeObserver(o) }
        observer = nil
        window = nil
    }
}

final class TestCardView: NSView {
    override var isFlipped: Bool { true }
    var nowShowing = "" { didSet { needsDisplay = true } }

    override func draw(_ dirty: NSRect) {
        let px = 1 / (window?.backingScaleFactor ?? 2) // one device pixel
        let half = bounds.width / 2
        NSColor.white.setFill()
        NSRect(x: 0, y: 0, width: half, height: bounds.height).fill()
        NSColor.black.setFill()
        NSRect(x: half, y: 0, width: half, height: bounds.height).fill()

        let sample = "The quick brown fox jumps over the lazy dog 0123456789 ({[;:,.]})"
        let colours: [(NSColor, String)] = [
            (.black, "black"), (.systemRed, "red"), (NSColor(srgbRed: 0, green: 0, blue: 1, alpha: 1), "blue"),
            (NSColor(srgbRed: 0, green: 0.6, blue: 0, alpha: 1), "green"), (.systemPurple, "purple"),
        ]
        // Which view this is, readable on the PC.
        NSColor.systemYellow.setFill()
        NSRect(x: 0, y: 0, width: bounds.width, height: 34).fill()
        let banner: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: 18, weight: .semibold),
                                                     .foregroundColor: NSColor.black]
        ("Now showing: \(nowShowing)" as NSString).draw(at: NSPoint(x: 12, y: 6), withAttributes: banner)
        var y: CGFloat = 46
        func text(_ s: String, _ size: CGFloat, _ colour: NSColor, x: CGFloat) {
            let a: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: size), .foregroundColor: colour]
            (s as NSString).draw(at: NSPoint(x: x, y: y), withAttributes: a)
        }
        text("Glass clarity test card: compare with the Mac's own screen.", 13, .black, x: 12)
        text("Light text on dark", 13, .white, x: half + 12)
        y += 26
        for size in [9.0, 10, 11, 12, 13, 14, 16] as [CGFloat] {
            for (c, _) in colours {
                text("\(Int(size)) pt  \(sample)", size, c, x: 12)
                let light = c == .black ? NSColor.white : c.blended(withFraction: 0.35, of: .white) ?? c
                text("\(Int(size)) pt  \(sample)", size, light, x: half + 12)
                y += size + 5
            }
        }
        y += 10
        // 1-pixel patterns: lines, checkerboard, coloured stripes (chroma test).
        func pattern(_ x0: CGFloat, _ label: String, _ colourA: NSColor, _ colourB: NSColor, checker: Bool) {
            let w: CGFloat = 120, h: CGFloat = 60
            let a: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: 11),
                                                     .foregroundColor: x0 < half ? NSColor.black : NSColor.white]
            (label as NSString).draw(at: NSPoint(x: x0, y: y), withAttributes: a)
            let top = y + 16
            var col = 0
            var x = x0
            while x < x0 + w {
                var row = 0
                var yy = top
                while yy < top + h {
                    let even = checker ? (col + row) % 2 == 0 : col % 2 == 0
                    (even ? colourA : colourB).setFill()
                    NSRect(x: x, y: yy, width: px, height: checker ? px : h).fill()
                    if !checker { break }
                    yy += px
                    row += 1
                }
                x += px
                col += 1
            }
        }
        pattern(12, "1 px black/white lines", .black, .white, checker: false)
        pattern(150, "1 px checkerboard", .black, .white, checker: true)
        pattern(288, "1 px red/blue stripes", .red, .blue, checker: false)
        pattern(426, "1 px red/white stripes", .red, .white, checker: false)
        pattern(half + 12, "1 px white/black lines", .white, .black, checker: false)
        pattern(half + 150, "1 px green/magenta", .green, .magenta, checker: false)
        y += 90
        text("Sharp = the stripes and checkerboards look even and grey/purple, not blotchy; small red and blue text has clean edges.", 11, .darkGray, x: 12)
    }
}

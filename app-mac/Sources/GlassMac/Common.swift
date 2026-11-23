import AppKit
import Foundation
import GlassKit
import QuartzCore

var appVersion: String {
    Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "dev"
}

var banner: String {
    "Glass (macOS) \(appVersion) | core \(GlassCore.version) | protocol v\(GlassCore.protocolVersion) | build \(GlassCore.build)"
}

func systemInfo() -> String {
    var size = 0
    sysctlbyname("hw.model", nil, &size, nil, 0)
    var model = [CChar](repeating: 0, count: max(size, 1))
    sysctlbyname("hw.model", &model, &size, nil, 0)
    var chip = [CChar](repeating: 0, count: 128)
    var chipSize = chip.count
    sysctlbyname("machdep.cpu.brand_string", &chip, &chipSize, nil, 0)
    let screen = NSScreen.main.map {
        let px = $0.convertRectToBacking($0.frame).size
        return "\(Int(px.width))x\(Int(px.height)) @ \($0.maximumFramesPerSecond) Hz"
    } ?? "no screen"
    return "Mac: \(String(cString: model)) (\(String(cString: chip))), \(ProcessInfo.processInfo.operatingSystemVersionString), display \(screen)"
}

/// `later - earlier` in µs, clamped to 0...UInt32.max (for Stats).
func span(_ later: UInt64, _ earlier: UInt64) -> UInt32 {
    later > earlier ? UInt32(min(later - earlier, UInt64(UInt32.max))) : 0
}

/// NSView backed by a CAMetalLayer; reports size/scale changes. With
/// `onMouse` set it also takes the mouse (moves, all buttons, scroll) and
/// shows `cursor` over `cursorArea`.
final class MetalView: NSView {
    var onResize: (() -> Void)?
    var onMouse: ((NSEvent) -> Void)?
    var cursor: NSCursor = .arrow {
        didSet { window?.invalidateCursorRects(for: self) }
    }
    /// Picture area in view points (cursor shown there; arrow over the bars).
    var cursorArea: NSRect? {
        didSet { window?.invalidateCursorRects(for: self) }
    }
    private var tracking: NSTrackingArea?

    override func makeBackingLayer() -> CALayer { CAMetalLayer() }
    override var wantsUpdateLayer: Bool { true }
    override var acceptsFirstResponder: Bool { onMouse != nil }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func layout() {
        super.layout()
        onResize?()
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        onResize?()
    }

    override func resetCursorRects() {
        guard onMouse != nil else { return }
        addCursorRect(cursorArea ?? bounds, cursor: cursor)
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let t = tracking { removeTrackingArea(t) }
        let t = NSTrackingArea(rect: .zero, options: [.mouseMoved, .activeInKeyWindow, .inVisibleRect], owner: self, userInfo: nil)
        addTrackingArea(t)
        tracking = t
    }

    override func mouseMoved(with e: NSEvent) { onMouse?(e) }
    override func mouseDragged(with e: NSEvent) { onMouse?(e) }
    override func rightMouseDragged(with e: NSEvent) { onMouse?(e) }
    override func otherMouseDragged(with e: NSEvent) { onMouse?(e) }
    override func mouseDown(with e: NSEvent) { onMouse?(e) }
    override func mouseUp(with e: NSEvent) { onMouse?(e) }
    override func rightMouseDown(with e: NSEvent) { onMouse?(e) }
    override func rightMouseUp(with e: NSEvent) { onMouse?(e) }
    override func otherMouseDown(with e: NSEvent) { onMouse?(e) }
    override func otherMouseUp(with e: NSEvent) { onMouse?(e) }
    override func scrollWheel(with e: NSEvent) { onMouse?(e) }
    // Right-click must not open a context menu here.
    override func menu(for event: NSEvent) -> NSMenu? { nil }
}

/// Shows results in a selectable monospace text view with a Copy button.
func showResults(_ text: String) {
    let alert = NSAlert()
    alert.messageText = "Glass test results"
    alert.informativeText = "Copy these and paste them back to Claude."
    let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: 820, height: 340))
    scroll.hasVerticalScroller = true
    let tv = NSTextView(frame: scroll.bounds)
    tv.isEditable = false
    tv.isSelectable = true
    tv.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
    tv.string = text
    scroll.documentView = tv
    alert.accessoryView = scroll
    alert.addButton(withTitle: "Copy results")
    alert.addButton(withTitle: "Close")
    NSApp.activate(ignoringOtherApps: true)
    if alert.runModal() == .alertFirstButtonReturn {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}

func showError(_ title: String, _ message: String) {
    let alert = NSAlert()
    alert.alertStyle = .warning
    alert.messageText = title
    alert.informativeText = message
    NSApp.activate(ignoringOtherApps: true)
    alert.runModal()
}

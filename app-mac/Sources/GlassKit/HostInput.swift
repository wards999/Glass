import AppKit
import Carbon.HIToolbox
import CGlassCore
import CoreGraphics
import Foundation

/// Whether this Mac's keyboard is ISO (e.g. UK): two keys swap codes vs a PC.
public func macKeyboardIsISO() -> Bool {
    Int(KBGetLayoutType(Int16(LMGetKbdType()))) == Int(kKeyboardISO)
}

/// Injects the viewer's input with CGEvent (needs Accessibility). Runs on
/// core's UDP thread; all state is touched only there.
public final class MacInputInjector: GlassInputHandler {
    private var _bounds: CGRect
    private let iso: Bool
    private let source = CGEventSource(stateID: .hidSystemState)
    /// Count only (CI / test pattern): nothing reaches macOS.
    private let dryRun: Bool
    private var pos: CGPoint
    private var buttons: UInt8 = 0
    private var flags: CGEventFlags = []
    private var capsLock = false
    private var lastDown: (button: UInt8, time: TimeInterval, pos: CGPoint, count: Int64) = (255, 0, .zero, 0)
    private var scrollRemainder: (dy: Double, dx: Double) = (0, 0)
    private let lock = NSLock()
    private var _counts = (keys: 0, clicks: 0, scrolls: 0, moves: 0)

    public init(display: HostDisplay, dryRun: Bool = false) {
        _bounds = display.bounds
        iso = macKeyboardIsISO()
        self.dryRun = dryRun
        pos = CGPoint(x: display.bounds.midX, y: display.bounds.midY)
    }

    public var counts: (keys: Int, clicks: Int, scrolls: Int, moves: Int) { lock.withLock { _counts } }

    /// The display input lands on (a view-mode change switches it).
    public func setDisplay(_ d: HostDisplay) { lock.withLock { _bounds = d.bounds } }

    private func point(_ x: UInt16, _ y: UInt16) -> CGPoint {
        let bounds = lock.withLock { _bounds }
        return CGPoint(x: bounds.minX + Double(x) / 65535 * max(bounds.width - 1, 1),
                y: bounds.minY + Double(y) / 65535 * max(bounds.height - 1, 1))
    }

    private func post(_ e: CGEvent?) {
        guard let e, !dryRun else { return }
        e.flags = flags
        e.post(tap: .cghidEventTap)
    }

    public func handle(_ a: GLASS_InputActionC) {
        switch Int32(a.kind) {
        case GLASS_INPUT_MOVE:
            pos = point(a.x, a.y)
            lock.withLock { _counts.moves += 1 }
            let (type, button): (CGEventType, CGMouseButton) =
                buttons & 1 != 0 ? (.leftMouseDragged, .left)
                : buttons & 2 != 0 ? (.rightMouseDragged, .right)
                : buttons != 0 ? (.otherMouseDragged, .center)
                : (.mouseMoved, .left)
            post(CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: pos, mouseButton: button))
        case GLASS_INPUT_BUTTON:
            button(a.button, down: a.down)
        case GLASS_INPUT_KEY:
            key(a.hid, down: a.down)
        case GLASS_INPUT_SCROLL:
            scroll(dy: Int(a.dy), dx: Int(a.dx))
        default:
            break
        }
    }

    private func button(_ b: UInt8, down: Bool) {
        let bit: UInt8 = 1 << min(b, 7)
        if down { buttons |= bit } else { buttons &= ~bit }
        let type: CGEventType
        let cgButton: CGMouseButton
        switch b {
        case 0:
            type = down ? .leftMouseDown : .leftMouseUp
            cgButton = .left
        case 1:
            type = down ? .rightMouseDown : .rightMouseUp
            cgButton = .right
        default:
            type = down ? .otherMouseDown : .otherMouseUp
            cgButton = .center
        }
        // Double and triple clicks: macOS counts them from the click state.
        if down {
            let now = ProcessInfo.processInfo.systemUptime
            let near = abs(pos.x - lastDown.pos.x) < 5 && abs(pos.y - lastDown.pos.y) < 5
            let count = (lastDown.button == b && near && now - lastDown.time < NSEvent.doubleClickInterval) ? lastDown.count + 1 : 1
            lastDown = (b, now, pos, count)
            lock.withLock { _counts.clicks += 1 }
        }
        let e = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: pos, mouseButton: cgButton)
        e?.setIntegerValueField(.mouseEventClickState, value: lastDown.button == b ? lastDown.count : 1)
        if b >= 2 { e?.setIntegerValueField(.mouseEventButtonNumber, value: Int64(b)) }
        post(e)
    }

    private static let modifierFlags: [UInt16: CGEventFlags] = [
        0x37: .maskCommand, 0x36: .maskCommand,
        0x38: .maskShift, 0x3C: .maskShift,
        0x3A: .maskAlternate, 0x3D: .maskAlternate,
        0x3B: .maskControl, 0x3E: .maskControl,
    ]
    private var heldModifiers = Set<UInt16>()

    private func key(_ hid: UInt8, down: Bool) {
        guard let code = glassMacKey(hid: hid, iso: iso) else { return }
        lock.withLock { _counts.keys += down ? 1 : 0 }
        if let flag = Self.modifierFlags[code] {
            if down { heldModifiers.insert(code) } else { heldModifiers.remove(code) }
            // A flag stays while either key (left or right) holds it.
            let stillHeld = heldModifiers.contains { Self.modifierFlags[$0] == flag }
            if stillHeld { flags.insert(flag) } else { flags.remove(flag) }
            let e = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down)
            e?.type = .flagsChanged
            post(e)
            return
        }
        if code == 0x39 { // Caps Lock toggles on press
            if down {
                capsLock.toggle()
                if capsLock { flags.insert(.maskAlphaShift) } else { flags.remove(.maskAlphaShift) }
            }
            let e = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down)
            e?.type = .flagsChanged
            post(e)
            return
        }
        post(CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down))
    }

    private func scroll(dy: Int, dx: Int) {
        lock.withLock { _counts.scrolls += 1 }
        // A PC wheel notch (120) is three lines; finer deltas (precision
        // touchpads) scroll by pixels, 6 units per point (as the Mac viewer sends).
        if dy % 120 == 0 && dx % 120 == 0 {
            post(CGEvent(scrollWheelEvent2Source: source, units: .line, wheelCount: 2,
                         wheel1: Int32(dy / 40), wheel2: Int32(-dx / 40), wheel3: 0))
            return
        }
        scrollRemainder.dy += Double(dy) / 6
        scrollRemainder.dx += Double(dx) / 6
        let py = scrollRemainder.dy.rounded(.towardZero), px = scrollRemainder.dx.rounded(.towardZero)
        scrollRemainder.dy -= py
        scrollRemainder.dx -= px
        if py != 0 || px != 0 {
            post(CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2,
                         wheel1: Int32(py), wheel2: Int32(-px), wheel3: 0))
        }
    }
}

/// The Mac's current pointer image, for the viewer to draw as its own
/// cursor. Poll on the main thread; returns a shape only when it changed.
public final class CursorWatch {
    public struct Shape {
        public let width: Int
        public let height: Int
        public let hotX: Int
        public let hotY: Int
        public let rgba: [UInt8]
    }

    /// Pixels per point on the shared display (the stream is in pixels).
    private let scale: CGFloat
    private var last: (rgba: [UInt8], hot: NSPoint)?

    public init(display: HostDisplay) {
        scale = CGFloat(display.width) / max(display.bounds.width, 1)
    }

    public func poll() -> Shape? {
        let cursor = NSCursor.currentSystem ?? NSCursor.arrow
        let img = cursor.image
        var w = Int((img.size.width * scale).rounded())
        var h = Int((img.size.height * scale).rounded())
        var s = scale
        if w > 96 || h > 96 { // the protocol's limit; very large pointers shrink
            let k = 96 / CGFloat(max(w, h))
            s *= k
            w = Int((CGFloat(w) * k).rounded(.down))
            h = Int((CGFloat(h) * k).rounded(.down))
        }
        guard w > 0, h > 0,
              let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        var rect = NSRect(x: 0, y: 0, width: w, height: h)
        guard let cg = img.cgImage(forProposedRect: &rect, context: nil, hints: nil) else { return nil }
        ctx.interpolationQuality = .high
        ctx.draw(cg, in: CGRect(x: 0, y: 0, width: w, height: h))
        guard let data = ctx.data else { return nil }
        let p = data.assumingMemoryBound(to: UInt8.self)
        var rgba = [UInt8](repeating: 0, count: w * h * 4)
        // CGContext rows start at the top here (bitmap memory order); un-premultiply.
        for i in 0..<(w * h) {
            let a = Int(p[i * 4 + 3])
            rgba[i * 4 + 3] = UInt8(a)
            for c in 0..<3 {
                let v = Int(p[i * 4 + c])
                rgba[i * 4 + c] = a == 0 ? 0 : UInt8(min(255, v * 255 / a))
            }
        }
        // macOS hands out new cursor objects; compare the pixels.
        if let last, last.hot == cursor.hotSpot, last.rgba == rgba { return nil }
        last = (rgba, cursor.hotSpot)
        return Shape(width: w, height: h,
                     hotX: min(w - 1, Int((cursor.hotSpot.x * s).rounded())),
                     hotY: min(h - 1, Int((cursor.hotSpot.y * s).rounded())),
                     rgba: rgba)
    }
}

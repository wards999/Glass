import CGlassCore

/// Swift-facing wrapper over the Rust core's C ABI.
public enum GlassCore {
    public static var version: String { String(cString: glass_core_version()) }
    public static var protocolVersion: UInt8 { glass_protocol_version() }
}

// LATER(M1): client decode (VTDecompressionSession) -> Metal render from an HEVC file.
// LATER(M3b): host capture (ScreenCaptureKit) + VideoToolbox encode + CGEvent injection,
//             with Screen Recording / Accessibility permission checks.

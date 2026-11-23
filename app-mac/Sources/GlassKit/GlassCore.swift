import CGlassCore

/// Swift-facing wrapper over the Rust core's C ABI.
public enum GlassCore {
    public static var version: String { String(cString: glass_core_version()) }
    /// The test-build tag this was built from, or "local".
    public static var build: String { String(cString: glass_core_build()) }
    public static var protocolVersion: UInt8 { glass_protocol_version() }
}

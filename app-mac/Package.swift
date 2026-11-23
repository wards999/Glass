// swift-tools-version:5.9
// Glass for macOS: host and client roles in one app.
// Build core first: `cargo build -p glass-core --release` (from the repo root).
import Foundation
import PackageDescription

// libglass_core.a lives in the Cargo workspace target dir. Absolute path so the
// linker finds it regardless of SwiftPM's working directory.
let coreLibDir = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .appendingPathComponent("../target/release")
    .standardized.path

let package = Package(
    name: "GlassMac",
    platforms: [.macOS(.v14)],
    targets: [
        .systemLibrary(name: "CGlassCore", path: "Sources/CGlassCore"),
        // Private CGVirtualDisplay (M3c), looked up at run time.
        .target(
            name: "CVirtualDisplay",
            cSettings: [.unsafeFlags(["-fobjc-arc"])],
            linkerSettings: [.linkedFramework("CoreGraphics"), .linkedFramework("Foundation")]
        ),
        .target(
            name: "GlassKit",
            dependencies: ["CGlassCore", "CVirtualDisplay"],
            linkerSettings: [.unsafeFlags(["-L", coreLibDir]), .linkedFramework("CoreWLAN")]
        ),
        .executableTarget(name: "GlassMac", dependencies: ["GlassKit", "CGlassCore"]),
        .testTarget(
            name: "GlassKitTests",
            dependencies: ["GlassKit", "CGlassCore"],
            resources: [.copy("Fixtures")]
        ),
    ]
)

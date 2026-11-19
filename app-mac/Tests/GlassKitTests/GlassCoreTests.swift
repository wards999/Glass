import CoreVideo
import Metal
import XCTest
@testable import GlassKit

final class GlassCoreTests: XCTestCase {
    func testCoreLinksAndReportsVersions() {
        XCTAssertFalse(GlassCore.version.isEmpty)
        XCTAssertEqual(GlassCore.version.split(separator: ".").count, 3)
        XCTAssertEqual(GlassCore.protocolVersion, 0)
    }
}

/// Fixtures: 12 frames of FFmpeg testsrc2 at 320x192, encoded with hevc_nvenc
/// on Windows (same settings family as `glass record`) and packed with `glass pack`.
final class PipelineTests: XCTestCase {
    func fixture(_ name: String) throws -> Recording {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "glsr", subdirectory: "Fixtures"))
        return try Recording(contentsOf: url)
    }

    func testParsesRecording() throws {
        for (name, is444) in [("testsrc-420", false), ("testsrc-444", true)] {
            let rec = try fixture(name)
            XCTAssertEqual(rec.header.width, 320)
            XCTAssertEqual(rec.header.height, 192)
            XCTAssertEqual(rec.is444, is444)
            XCTAssertEqual(rec.frames.count, 12)
            XCTAssertTrue(rec.frames[0].isIDR)
            XCTAssertEqual(rec.frames.map(\.meta.frame_id), Array(0..<12))
        }
    }

    func testRejectsGarbage() {
        XCTAssertThrowsError(try Recording(data: Data("not a recording".utf8)))
    }

    func decodeAll(_ rec: Recording) throws -> (HEVCDecoder, [CVPixelBuffer]) {
        let dec = HEVCDecoder(chroma444: rec.is444)
        var out: [CVPixelBuffer] = []
        for f in rec.frames {
            if let pb = try rec.withFrameBytes(f, { try dec.decode($0) }) { out.append(pb) }
        }
        return (dec, out)
    }

    func testDecodes420() throws {
        let (dec, frames) = try decodeAll(try fixture("testsrc-420"))
        XCTAssertEqual(frames.count, 12)
        XCTAssertEqual(CVPixelBufferGetWidth(frames[0]), 320)
        XCTAssertEqual(CVPixelBufferGetPixelFormatType(frames[0]), kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        XCTAssertNotNil(CVPixelBufferGetIOSurface(frames[0]))
        XCTAssertFalse(Renderer.isBT709(frames[0]), "NVENC RGB input is tagged BT.601")
        print("4:2:0 decoder: \(dec.usingHardware ? "hardware" : "software")")
    }

    func testDecodes444() throws {
        let rec = try fixture("testsrc-444")
        let dec: HEVCDecoder, frames: [CVPixelBuffer]
        do {
            (dec, frames) = try decodeAll(rec)
        } catch {
            throw XCTSkip("HEVC 4:4:4 decode unavailable on this machine: \(error)")
        }
        XCTAssertEqual(frames.count, 12)
        XCTAssertEqual(CVPixelBufferGetPixelFormatType(frames[0]), kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange)
        XCTAssertEqual(CVPixelBufferGetWidthOfPlane(frames[0], 1), 320, "4:4:4 chroma plane is full width")
        print("4:4:4 decoder: \(dec.usingHardware ? "hardware" : "software")")
    }

    /// Renders the first 4:2:0 frame offscreen and checks the shader produced
    /// real colour (testsrc2 is mostly saturated colour bars, never all black).
    func testRendersOffscreen() throws {
        guard let device = MTLCreateSystemDefaultDevice() else { throw XCTSkip("no Metal device") }
        let (_, frames) = try decodeAll(try fixture("testsrc-420"))
        let renderer = try Renderer(device: device)
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: 320, height: 192, mipmapped: false)
        desc.usage = [.renderTarget, .shaderRead]
        desc.storageMode = .shared
        let target = try XCTUnwrap(device.makeTexture(descriptor: desc))
        let cb = try XCTUnwrap(renderer.queue.makeCommandBuffer())
        XCTAssertTrue(renderer.encode(frames[0], to: target, in: cb))
        cb.commit()
        cb.waitUntilCompleted()
        XCTAssertNil(cb.error)
        var px = [UInt8](repeating: 0, count: 320 * 192 * 4)
        target.getBytes(&px, bytesPerRow: 320 * 4, from: MTLRegionMake2D(0, 0, 320, 192), mipmapLevel: 0)
        let distinct = Set(stride(from: 0, to: px.count, by: 4 * 97).map { Int(px[$0]) << 16 | Int(px[$0 + 1]) << 8 | Int(px[$0 + 2]) })
        XCTAssertGreaterThan(distinct.count, 8, "expected a colourful test pattern")
    }
}

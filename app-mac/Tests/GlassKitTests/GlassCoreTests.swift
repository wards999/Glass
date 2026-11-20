import CoreVideo
import Metal
import XCTest
@testable import GlassKit

final class GlassCoreTests: XCTestCase {
    func testCoreLinksAndReportsVersions() {
        XCTAssertFalse(GlassCore.version.isEmpty)
        XCTAssertEqual(GlassCore.version.split(separator: ".").count, 3)
        XCTAssertEqual(GlassCore.protocolVersion, 1)
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
        var distinct = Set<UInt32>()
        for i in stride(from: 0, to: px.count, by: 4 * 97) {
            let b = UInt32(px[i]), g = UInt32(px[i + 1]), r = UInt32(px[i + 2])
            distinct.insert((r << 16) | (g << 8) | b)
        }
        XCTAssertGreaterThan(distinct.count, 8, "expected a colourful test pattern")
    }
}

/// Live path over loopback: core's replay host streams a fixture, GlassClient
/// receives it (same code the viewer uses), VideoToolbox decodes it.
final class LiveTests: XCTestCase {
    func testLoopbackStreamDecodes() throws {
        for name in ["testsrc-420", "testsrc-444"] {
            let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "glsr", subdirectory: "Fixtures"))
            let data = try Data(contentsOf: url)
            let rec = try Recording(data: data)
            let expected = Set(rec.frames.map { f in rec.withFrameBytes(f) { Data($0) } })
            let host = try ReplayHost(data: data)
            let client = try GlassClient(host: "127.0.0.1", port: host.port)
            XCTAssertEqual(client.info.width, 320)
            XCTAssertEqual(client.info.height, 192)
            XCTAssertEqual(client.is444, rec.is444)
            let dec = HEVCDecoder(chroma444: client.is444)
            var received = 0, decoded = 0, mismatched = 0
            let deadline = Date().addingTimeInterval(10)
            do {
                while decoded < 24, Date() < deadline {
                    let r = try client.waitFrame(timeoutMs: 200) { _, bytes in
                        received += 1
                        if !expected.contains(Data(bytes)) { mismatched += 1 }
                        if try dec.decode(bytes) != nil { decoded += 1 }
                    }
                    XCTAssertNotEqual(r, .disconnected)
                }
            } catch let error where rec.is444 {
                throw XCTSkip("HEVC 4:4:4 decode unavailable on this machine: \(error)")
            }
            XCTAssertGreaterThanOrEqual(decoded, 24, name)
            XCTAssertEqual(mismatched, 0, name)
            XCTAssertEqual(client.stats.rx.frames_lost, 0, name)
            XCTAssertNotNil(client.hostToLocal(GlassClock.nowUs()), "clock sync should have a sample by now")
            print("\(name): received \(received), decoded \(decoded), RTT \(client.stats.last_rtt_us) us")
            client.close()
            host.stop()
        }
    }

    func testConnectRefused() {
        XCTAssertThrowsError(try GlassClient(host: "127.0.0.1", port: 9)) { error in
            XCTAssertEqual((error as? ConnectFailure)?.code, 2)
        }
    }

    func testFitViewport() {
        // 16:9 into 16:10 -> bars top and bottom.
        let v = Renderer.fitViewport(src: CGSize(width: 2560, height: 1440), dst: CGSize(width: 2940, height: 1912))
        XCTAssertEqual(v.width, 2940)
        XCTAssertEqual(v.height, 1654)
        XCTAssertEqual(v.originX, 0)
        XCTAssertEqual(v.originY, 129)
        // Exact fit stays 1:1.
        let e = Renderer.fitViewport(src: CGSize(width: 2560, height: 1440), dst: CGSize(width: 2560, height: 1440))
        XCTAssertEqual(e.width, 2560)
        XCTAssertEqual(e.originY, 0)
    }
}

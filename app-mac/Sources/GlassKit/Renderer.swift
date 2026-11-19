import CoreVideo
import Foundation
import Metal
import simd

/// Draws a bi-planar YCbCr CVPixelBuffer (4:2:0 or 4:4:4, video range) into a
/// BGRA Metal texture. Planes are wrapped zero-copy via CVMetalTextureCache.
public final class Renderer {
    public let device: MTLDevice
    public let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private let cache: CVMetalTextureCache

    struct Csc {
        var m: simd_float3x3
        var offset: SIMD3<Float>
    }

    static let shader = """
    #include <metal_stdlib>
    using namespace metal;
    struct VOut { float4 pos [[position]]; float2 uv; };
    vertex VOut vmain(uint vid [[vertex_id]]) {
        float2 p = float2((vid << 1) & 2, vid & 2);
        VOut o;
        o.pos = float4(p * 2.0 - 1.0, 0.0, 1.0);
        o.uv = float2(p.x, 1.0 - p.y);
        return o;
    }
    struct Csc { float3x3 m; float3 offset; };
    fragment float4 fmain(VOut in [[stage_in]],
                          texture2d<float> y [[texture(0)]],
                          texture2d<float> c [[texture(1)]],
                          constant Csc& csc [[buffer(0)]]) {
        constexpr sampler s(filter::linear, address::clamp_to_edge);
        float3 yuv = float3(y.sample(s, in.uv).r, c.sample(s, in.uv).rg);
        return float4(saturate(csc.m * (yuv - csc.offset)), 1.0);
    }
    """

    public init(device: MTLDevice? = MTLCreateSystemDefaultDevice(), pixelFormat: MTLPixelFormat = .bgra8Unorm) throws {
        guard let device else { throw GlassError.metal("no Metal device") }
        guard let queue = device.makeCommandQueue() else { throw GlassError.metal("no command queue") }
        let lib = try device.makeLibrary(source: Self.shader, options: nil)
        let desc = MTLRenderPipelineDescriptor()
        desc.vertexFunction = lib.makeFunction(name: "vmain")
        desc.fragmentFunction = lib.makeFunction(name: "fmain")
        desc.colorAttachments[0].pixelFormat = pixelFormat
        pipeline = try device.makeRenderPipelineState(descriptor: desc)
        var cache: CVMetalTextureCache?
        let st = CVMetalTextureCacheCreate(kCFAllocatorDefault, nil, device, nil, &cache)
        guard st == kCVReturnSuccess, let cache else { throw GlassError.metal("CVMetalTextureCacheCreate \(st)") }
        self.device = device
        self.queue = queue
        self.pipeline = pipeline
        self.cache = cache
    }

    /// Colour matrix from the buffer's attachments. NVENC's RGB input path tags
    /// streams BT.601, so 601 is the default when nothing is attached.
    public static func isBT709(_ pb: CVPixelBuffer) -> Bool {
        guard let m = CVBufferCopyAttachment(pb, kCVImageBufferYCbCrMatrixKey, nil) else { return false }
        return CFEqual(m, kCVImageBufferYCbCrMatrix_ITU_R_709_2)
    }

    static func csc(bt709: Bool) -> Csc {
        let (cb, cr): (SIMD3<Float>, SIMD3<Float>) = bt709
            ? (SIMD3(0, -0.187324, 1.8556), SIMD3(1.5748, -0.468124, 0))
            : (SIMD3(0, -0.344136, 1.772), SIMD3(1.402, -0.714136, 0))
        // Video range: Y in [16,235], C in [16,240].
        let ys: Float = 255.0 / 219.0
        let cs: Float = 255.0 / 224.0
        return Csc(
            m: simd_float3x3(columns: (SIMD3<Float>(1, 1, 1) * ys, cb * cs, cr * cs)),
            offset: SIMD3(16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0))
    }

    private func texture(_ pb: CVPixelBuffer, plane: Int, format: MTLPixelFormat) -> CVMetalTexture? {
        var t: CVMetalTexture?
        CVMetalTextureCacheCreateTextureFromImage(
            kCFAllocatorDefault, cache, pb, nil, format,
            CVPixelBufferGetWidthOfPlane(pb, plane), CVPixelBufferGetHeightOfPlane(pb, plane), plane, &t)
        return t
    }

    /// Encodes the conversion draw into `cb`. Returns false if the planes
    /// couldn't be wrapped.
    @discardableResult
    public func encode(_ pb: CVPixelBuffer, to target: MTLTexture, in cb: MTLCommandBuffer) -> Bool {
        guard let yT = texture(pb, plane: 0, format: .r8Unorm),
              let cT = texture(pb, plane: 1, format: .rg8Unorm),
              let y = CVMetalTextureGetTexture(yT), let c = CVMetalTextureGetTexture(cT)
        else { return false }
        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = target
        pass.colorAttachments[0].loadAction = .dontCare
        pass.colorAttachments[0].storeAction = .store
        guard let enc = cb.makeRenderCommandEncoder(descriptor: pass) else { return false }
        var csc = Self.csc(bt709: Self.isBT709(pb))
        enc.setRenderPipelineState(pipeline)
        enc.setFragmentTexture(y, index: 0)
        enc.setFragmentTexture(c, index: 1)
        enc.setFragmentBytes(&csc, length: MemoryLayout<Csc>.stride, index: 0)
        enc.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        enc.endEncoding()
        // Keep the CVMetalTextures (and so the IOSurface) alive until the GPU is done.
        cb.addCompletedHandler { _ in _ = (yT, cT) }
        return true
    }
}

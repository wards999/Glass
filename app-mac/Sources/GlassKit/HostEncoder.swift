import CGlassCore
import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

/// HEVC encode with VideoToolbox for the Mac host: real-time, no frame
/// reordering, hardware only (software fallback reported). Pixel buffers
/// from ScreenCaptureKit are IOSurface-backed, so frames stay on the GPU.
/// Output is Annex-B with VPS/SPS/PPS before every keyframe.
public final class HEVCEncoder {
    public struct Config {
        public var width: Int
        public var height: Int
        public var fps: Int
        public var bitrate: Int
        public var chroma444: Bool
        public init(width: Int, height: Int, fps: Int, bitrate: Int, chroma444: Bool = false) {
            self.width = width
            self.height = height
            self.fps = fps
            self.bitrate = bitrate
            self.chroma444 = chroma444
        }
    }

    /// One encoded access unit (Annex-B), valid only inside the callback.
    public typealias Output = (_ annexB: UnsafeRawBufferPointer, _ keyframe: Bool, _ ptsUs: UInt64) -> Void

    public let config: Config
    public private(set) var usingHardware = false
    /// Created with "hardware only", so it is hardware even if the session
    /// doesn't report it (the low-latency encoder doesn't).
    private var requiredHardware = false
    public private(set) var lowLatencyRateControl = false
    /// Chroma format of the first keyframe, read back from the stream
    /// (1 = 4:2:0, 3 = 4:4:4).
    public private(set) var producedChroma: Int?
    private var session: VTCompressionSession?

    /// Changes the bitrate from the next frame (M5 rate control): the
    /// average, and the 1.5x-per-second cap with it. No keyframe needed.
    public func setBitrate(_ bps: Int) {
        guard let session else { return }
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AverageBitRate, value: NSNumber(value: bps))
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_DataRateLimits,
                             value: [NSNumber(value: bps * 3 / 16), NSNumber(value: 1)] as CFArray)
    }
    private var out: UnsafeMutableRawPointer
    private var outCap: Int
    private var sample: UnsafeMutableRawPointer
    private var sampleCap: Int

    public init(_ config: Config) throws {
        self.config = config
        outCap = 4 << 20
        out = .allocate(byteCount: outCap, alignment: 16)
        sampleCap = 4 << 20
        sample = .allocate(byteCount: sampleCap, alignment: 16)
        var lastStatus: OSStatus = 0
        // Low-latency rate control where the OS offers it for HEVC, then
        // plain real-time hardware, then anything (software, CI runners).
        let specs: [[CFString: Any]] = [
            [kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true,
             kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true],
            [kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true],
            [:],
        ]
        // The low-latency encoder only does 4:2:0 (m3c-test2: 4:4:4 input
        // still came out 4:2:0), so 4:4:4 starts at plain hardware.
        for (i, spec) in specs.enumerated() where !(config.chroma444 && i == 0) {
            var s: VTCompressionSession?
            let source: [CFString: Any] = [
                kCVPixelBufferPixelFormatTypeKey: config.chroma444
                    ? kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange
                    : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                kCVPixelBufferWidthKey: config.width,
                kCVPixelBufferHeightKey: config.height,
                kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any]() as CFDictionary,
            ]
            lastStatus = VTCompressionSessionCreate(
                allocator: kCFAllocatorDefault, width: Int32(config.width), height: Int32(config.height),
                codecType: kCMVideoCodecType_HEVC, encoderSpecification: spec as CFDictionary,
                imageBufferAttributes: source as CFDictionary, compressedDataAllocator: nil,
                outputCallback: nil, refcon: nil, compressionSessionOut: &s)
            if lastStatus == noErr, let s {
                session = s
                lowLatencyRateControl = i == 0
                requiredHardware = i < 2
                break
            }
        }
        // (deinit frees the buffers if this throws)
        guard let session else { throw GlassError.decoder("VTCompressionSessionCreate(HEVC)", lastStatus) }
        func set(_ key: CFString, _ value: Any) {
            VTSessionSetProperty(session, key: key, value: value as CFTypeRef)
        }
        set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue!)
        set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse!)
        set(kVTCompressionPropertyKey_ExpectedFrameRate, NSNumber(value: config.fps))
        set(kVTCompressionPropertyKey_MaxKeyFrameInterval, NSNumber(value: config.fps * 600)) // IDR on request
        set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, NSNumber(value: 600))
        set(kVTCompressionPropertyKey_AverageBitRate, NSNumber(value: config.bitrate))
        // Cap any one second at 1.5x the average so a burst can't flood Wi-Fi.
        set(kVTCompressionPropertyKey_DataRateLimits, [NSNumber(value: config.bitrate * 3 / 16), NSNumber(value: 1)] as CFArray)
        // 4:4:4: no profile at all. Setting Main (even then clearing it) keeps
        // VideoToolbox at 4:2:0 (m3c-test1); unset, it picks Range Extensions.
        if !config.chroma444 { set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main_AutoLevel) }
        set(kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2)
        set(kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2)
        set(kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2)
        set(kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue!)
        set(kVTCompressionPropertyKey_MaximizePowerEfficiency, kCFBooleanFalse!)
        VTCompressionSessionPrepareToEncodeFrames(session)

        var hw: CFBoolean?
        VTSessionCopyProperty(session, key: kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                              allocator: kCFAllocatorDefault, valueOut: &hw)
        usingHardware = requiredHardware || (hw.map { CFBooleanGetValue($0) } ?? false)
    }

    deinit {
        if let s = session {
            VTCompressionSessionCompleteFrames(s, untilPresentationTimeStamp: .invalid)
            VTCompressionSessionInvalidate(s)
        }
        out.deallocate()
        sample.deallocate()
    }

    /// Encodes one frame and waits for it: `output` runs (on VideoToolbox's
    /// thread) before this returns, once per access unit produced.
    public func encode(_ pb: CVPixelBuffer, ptsUs: UInt64, keyframe: Bool, output: Output) throws {
        guard let session else { return }
        let pts = CMTime(value: CMTimeValue(ptsUs), timescale: 1_000_000)
        let props: CFDictionary? = keyframe ? [kVTEncodeFrameOptionKey_ForceKeyFrame: kCFBooleanTrue!] as CFDictionary : nil
        var failure: OSStatus = noErr
        try withoutActuallyEscaping(output) { output in
            let st = VTCompressionSessionEncodeFrame(
                session, imageBuffer: pb, presentationTimeStamp: pts, duration: .invalid,
                frameProperties: props, infoFlagsOut: nil
            ) { [self] status, _, sb in
                guard status == noErr, let sb else {
                    failure = status
                    return
                }
                self.deliver(sb, output)
            }
            guard st == noErr else { throw GlassError.decoder("VTCompressionSessionEncodeFrame", st) }
            // No reordering: completing up to this frame returns as soon as it's out.
            VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: pts)
        }
        if failure != noErr { throw GlassError.decoder("encode", failure) }
    }

    private func deliver(_ sb: CMSampleBuffer, _ output: Output) {
        guard let block = CMSampleBufferGetDataBuffer(sb), let fmt = CMSampleBufferGetFormatDescription(sb) else { return }
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[String: Any]]
        let notSync = (attachments?.first?[kCMSampleAttachmentKey_NotSync as String] as? Bool) ?? false
        let keyframe = !notSync
        if keyframe, producedChroma == nil { producedChroma = Self.chromaFormat(of: sb) }

        // Sample bytes (copied only if the block buffer isn't contiguous).
        let len = CMBlockBufferGetDataLength(block)
        if len > sampleCap {
            sample.deallocate()
            sampleCap = len * 2
            sample = .allocate(byteCount: sampleCap, alignment: 16)
        }
        var dataPtr: UnsafeMutablePointer<CChar>?
        var contiguous = 0
        let src: UnsafeRawPointer
        if CMBlockBufferGetDataPointer(block, atOffset: 0, lengthAtOffsetOut: &contiguous, totalLengthOut: nil, dataPointerOut: &dataPtr) == noErr,
           contiguous == len, let dataPtr {
            src = UnsafeRawPointer(dataPtr)
        } else {
            guard CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: len, destination: sample) == noErr else { return }
            src = UnsafeRawPointer(sample)
        }

        // Parameter sets go in front of every keyframe.
        var prefix = [GLASS_Bytes](repeating: GLASS_Bytes(), count: 4)
        var count = 0
        if keyframe {
            var n = 0
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, parameterSetIndex: 0, parameterSetPointerOut: nil,
                                                               parameterSetSizeOut: nil, parameterSetCountOut: &n, nalUnitHeaderLengthOut: nil)
            for i in 0..<min(n, 4) {
                var p: UnsafePointer<UInt8>?
                var size = 0
                if CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, parameterSetIndex: i, parameterSetPointerOut: &p,
                                                                      parameterSetSizeOut: &size, parameterSetCountOut: nil,
                                                                      nalUnitHeaderLengthOut: nil) == noErr, let p {
                    prefix[count] = GLASS_Bytes(ptr: p, len: size)
                    count += 1
                }
            }
        }
        let need = len + prefix.prefix(count).reduce(0) { $0 + Int($1.len) + 4 } + len / 64 + 64
        if need > outCap {
            out.deallocate()
            outCap = need * 2
            out = .allocate(byteCount: outCap, alignment: 16)
        }
        let n = prefix.withUnsafeBufferPointer { pre in
            glass_hevc_from_length_prefixed(pre.baseAddress, count, src.assumingMemoryBound(to: UInt8.self), len,
                                            out.assumingMemoryBound(to: UInt8.self), outCap)
        }
        guard n > 0 else { return }
        let pts = CMSampleBufferGetPresentationTimeStamp(sb)
        let ptsUs = UInt64(max(0, pts.seconds * 1e6))
        output(UnsafeRawBufferPointer(start: out, count: n), keyframe, ptsUs)
    }

    /// Chroma format the encoder actually produced (from the HEVC config
    /// record): 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4; nil if unknown.
    public static func chromaFormat(of sb: CMSampleBuffer) -> Int? {
        guard let fmt = CMSampleBufferGetFormatDescription(sb),
              let atoms = CMFormatDescriptionGetExtension(fmt, extensionKey: kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms) as? [String: Any],
              let hvcC = atoms["hvcC"] as? Data, hvcC.count > 16 else { return nil }
        return Int(hvcC[hvcC.startIndex + 16] & 0x03)
    }
}

/// Whether this Mac can hardware-encode HEVC 4:4:4 (decides how M3c keeps
/// text sharp). Encodes one synthetic frame and reads the chroma format back.
public func checkHEVC444Encode(width: Int = 1920, height: Int = 1080) -> String {
    var lines: [String] = []
    for want444 in [false, true] {
        let label = want444 ? "4:4:4" : "4:2:0"
        var session: VTCompressionSession?
        let spec = [kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true] as CFDictionary
        var st = VTCompressionSessionCreate(allocator: kCFAllocatorDefault, width: Int32(width), height: Int32(height),
                                            codecType: kCMVideoCodecType_HEVC, encoderSpecification: spec,
                                            imageBufferAttributes: nil, compressedDataAllocator: nil,
                                            outputCallback: nil, refcon: nil, compressionSessionOut: &session)
        guard st == noErr, let session else {
            lines.append("HEVC \(label) hardware encode: NO (no hardware HEVC encoder, status \(st))")
            continue
        }
        defer { VTCompressionSessionInvalidate(session) }
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        if !want444 {
            VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ProfileLevel, value: kVTProfileLevel_HEVC_Main_AutoLevel)
        }
        let format = want444 ? kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        var pb: CVPixelBuffer?
        let attrs = [kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any]() as CFDictionary] as CFDictionary
        st = CVPixelBufferCreate(kCFAllocatorDefault, width, height, format, attrs, &pb)
        guard st == kCVReturnSuccess, let pb else {
            lines.append("HEVC \(label) hardware encode: NO (can't make a \(label) buffer)")
            continue
        }
        CVPixelBufferLockBaseAddress(pb, [])
        for plane in 0..<CVPixelBufferGetPlaneCount(pb) {
            if let base = CVPixelBufferGetBaseAddressOfPlane(pb, plane) {
                memset(base, plane == 0 ? 0x80 : 0x60, CVPixelBufferGetBytesPerRowOfPlane(pb, plane) * CVPixelBufferGetHeightOfPlane(pb, plane))
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, [])
        var chroma: Int?
        var encodeStatus: OSStatus = noErr
        st = VTCompressionSessionEncodeFrame(session, imageBuffer: pb, presentationTimeStamp: .zero, duration: .invalid,
                                             frameProperties: nil, infoFlagsOut: nil) { status, _, sb in
            encodeStatus = status
            if let sb { chroma = HEVCEncoder.chromaFormat(of: sb) }
        }
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        let names = [1: "4:2:0", 2: "4:2:2", 3: "4:4:4"]
        if st != noErr || encodeStatus != noErr {
            lines.append("HEVC \(label) hardware encode: NO (encode failed, status \(st)/\(encodeStatus))")
        } else if let chroma {
            let got = names[chroma] ?? "chroma \(chroma)"
            lines.append("HEVC \(label) hardware encode: \(got == label ? "YES" : "NO (produced \(got))")")
        } else {
            lines.append("HEVC \(label) hardware encode: unknown (no format description)")
        }
    }
    return lines.joined(separator: "\n")
}

//! HEVC 4:4:4 decode on NVIDIA GPUs (M3c). D3D11VA has no 4:4:4 profile
//! there, so this uses FFmpeg's HEVC decoder on NVDEC and copies the three
//! decoded planes into D3D11 textures on the GPU (CUDA interop): frames
//! still never touch the CPU. The presenter converts them with a shader.

use super::cuda::{self, Cuda, Memcpy2D, CU_MEMORYTYPE_ARRAY, CU_MEMORYTYPE_DEVICE};
use super::decode::Color;
use super::Res;
use ffmpeg_sys_next as ff;
use std::ffi::{c_void, CStr, CString};
use std::ptr;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11ShaderResourceView, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

const EAGAIN: i32 = -11;
const PADDING: usize = ff::AV_INPUT_BUFFER_PADDING_SIZE as usize;

/// A decoded 4:4:4 picture: Y, Cb, Cr as R8 textures (valid until the next decode).
pub struct Planar {
    pub planes: [ID3D11ShaderResourceView; 3],
    pub width: u32,
    pub height: u32,
    pub color: Color,
}

struct Planes {
    srv: [ID3D11ShaderResourceView; 3],
    _tex: [ID3D11Texture2D; 3],
    res: [*mut c_void; 3],
    size: (u32, u32),
}

pub struct CudaDecoder {
    cu: &'static Cuda,
    cuctx: *mut c_void,
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    input: Vec<u8>,
    device: ID3D11Device,
    planes: Option<Planes>,
    color: Color,
}

fn check(code: i32, what: &str) -> Res<i32> {
    if code >= 0 {
        return Ok(code);
    }
    let mut buf = [0 as std::ffi::c_char; 128];
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    Err(format!("{what}: {msg} ({code})").into())
}

unsafe extern "C" fn get_format(
    _ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    let mut p = fmts;
    while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
        if *p == ff::AVPixelFormat::AV_PIX_FMT_CUDA {
            return *p;
        }
        p = p.add(1);
    }
    *fmts
}

/// The CUDA device ordinal of `device`'s adapter, if it's an NVIDIA GPU
/// with a CUDA driver.
fn cuda_ordinal(cu: &Cuda, device: &ID3D11Device) -> Option<i32> {
    unsafe {
        let adapter = device.cast::<IDXGIDevice>().ok()?.GetAdapter().ok()?;
        let mut dev = -1;
        let r = (cu.cuD3D11GetDevice)(&mut dev, adapter.as_raw());
        (r == cuda::CUDA_SUCCESS && dev >= 0).then_some(dev)
    }
}

/// Whether this PC can take the 4:4:4 path (tells the host in `Hello`).
pub fn available(device: &ID3D11Device) -> bool {
    let has_hevc = unsafe { !ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_HEVC).is_null() };
    has_hevc
        && cuda::cuda()
            .and_then(|cu| cuda_ordinal(cu, device))
            .is_some()
}

impl CudaDecoder {
    pub fn new(device: &ID3D11Device) -> Res<Self> {
        let cu = cuda::cuda().ok_or("no CUDA driver (needs an NVIDIA GPU)")?;
        let ordinal = cuda_ordinal(cu, device).ok_or("this GPU isn't a CUDA device")?;
        unsafe {
            let mut dev_ref: *mut ff::AVBufferRef = ptr::null_mut();
            let name = CString::new(ordinal.to_string())?;
            check(
                ff::av_hwdevice_ctx_create(
                    &mut dev_ref,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                    name.as_ptr(),
                    ptr::null_mut(),
                    0,
                ),
                "CUDA device",
            )?;
            // AVCUDADeviceContext starts with the CUcontext.
            let devctx = (*dev_ref).data as *mut ff::AVHWDeviceContext;
            let cuctx = *((*devctx).hwctx as *const *mut c_void);
            // FFmpeg's own HEVC decoder on NVDEC, not `hevc_cuvid`: that one
            // leaves finding the end of a picture to NVIDIA's parser, which
            // only knows when the next picture starts, so every picture came
            // out one frame late and a still screen's last one (the sharp
            // still keyframe) never did (m5-test28, `decode444-test`).
            let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_HEVC);
            if codec.is_null() {
                ff::av_buffer_unref(&mut dev_ref);
                return Err("no HEVC decoder in this FFmpeg build".into());
            }
            let mut ctx = ff::avcodec_alloc_context3(codec);
            (*ctx).hw_device_ctx = dev_ref;
            (*ctx).get_format = Some(get_format);
            // Each picture out as soon as it's decoded: no reordering delay,
            // no frame threads (each holds a picture back).
            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).thread_count = 1;
            (*ctx).pkt_timebase = ff::AVRational {
                num: 1,
                den: 1_000_000,
            };
            let r = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            if let Err(e) = check(r, "avcodec_open2(hevc, NVDEC)") {
                ff::avcodec_free_context(&mut ctx);
                return Err(e);
            }
            Ok(Self {
                cu,
                cuctx,
                ctx,
                pkt: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                input: Vec::with_capacity(1 << 20),
                device: device.clone(),
                planes: None,
                color: Color {
                    bt709: true,
                    full_range: false,
                },
            })
        }
    }

    fn make_planes(&mut self, w: u32, h: u32) -> Res<()> {
        self.release_planes();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let mut tex = Vec::new();
        let mut srv = Vec::new();
        let mut res = [ptr::null_mut(); 3];
        unsafe {
            // CUDA calls need FFmpeg's context current on this thread.
            cuda::check((self.cu.cuCtxPushCurrent)(self.cuctx), "cuCtxPushCurrent")?;
            let mut old = ptr::null_mut();
            let pop = |old: &mut *mut c_void| {
                (self.cu.cuCtxPopCurrent)(old);
            };
            for r in res.iter_mut() {
                let mut t = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut t))?;
                let t: ID3D11Texture2D = t.ok_or("no texture")?;
                let mut v = None;
                self.device
                    .CreateShaderResourceView(&t, None, Some(&mut v))?;
                let reg = (self.cu.cuGraphicsD3D11RegisterResource)(r, t.as_raw(), 0);
                if let Err(e) = cuda::check(reg, "register texture") {
                    pop(&mut old);
                    return Err(e);
                }
                tex.push(t);
                srv.push(v.ok_or("no view")?);
            }
            pop(&mut old);
        }
        fn to3<T>(v: Vec<T>) -> Res<[T; 3]> {
            v.try_into().map_err(|_| "planes".into())
        }
        self.planes = Some(Planes {
            srv: to3(srv)?,
            _tex: to3(tex)?,
            res,
            size: (w, h),
        });
        Ok(())
    }

    fn release_planes(&mut self) {
        if let Some(p) = self.planes.take() {
            unsafe {
                (self.cu.cuCtxPushCurrent)(self.cuctx);
                for r in p.res {
                    if !r.is_null() {
                        (self.cu.cuGraphicsUnregisterResource)(r);
                    }
                }
                let mut old = ptr::null_mut();
                (self.cu.cuCtxPopCurrent)(&mut old);
            }
        }
    }

    /// Decodes one access unit; returns the newest picture, if any.
    pub fn decode(&mut self, au: &[u8]) -> Res<Option<Planar>> {
        self.input.clear();
        self.input.extend_from_slice(au);
        let len = self.input.len();
        self.input.resize(len + PADDING, 0);
        let mut got = false;
        unsafe {
            (*self.pkt).data = self.input.as_mut_ptr();
            (*self.pkt).size = len as i32;
            let r = ff::avcodec_send_packet(self.ctx, self.pkt);
            (*self.pkt).data = ptr::null_mut();
            (*self.pkt).size = 0;
            check(r, "decode")?;
            loop {
                let r = ff::avcodec_receive_frame(self.ctx, self.frame);
                if r == EAGAIN || r == ff::AVERROR_EOF {
                    break;
                }
                check(r, "decode")?;
                self.copy_out()?;
                ff::av_frame_unref(self.frame);
                got = true;
            }
        }
        if !got {
            return Ok(None);
        }
        let p = self.planes.as_ref().ok_or("no planes")?;
        Ok(Some(Planar {
            planes: p.srv.clone(),
            width: p.size.0,
            height: p.size.1,
            color: self.color,
        }))
    }

    /// Copies the frame's Y, Cb, Cr planes (CUDA memory) into our textures.
    unsafe fn copy_out(&mut self) -> Res<()> {
        let f = &*self.frame;
        if f.format != ff::AVPixelFormat::AV_PIX_FMT_CUDA as i32 {
            return Err("NVDEC returned frames in system memory".into());
        }
        let fc = (*f.hw_frames_ctx).data as *const ff::AVHWFramesContext;
        if (*fc).sw_format != ff::AVPixelFormat::AV_PIX_FMT_YUV444P {
            return Err(format!("NVDEC output {:?}, not 8-bit 4:4:4", (*fc).sw_format).into());
        }
        let (w, h) = (f.width as u32, f.height as u32);
        if self.planes.as_ref().is_none_or(|p| p.size != (w, h)) {
            self.make_planes(w, h)?;
        }
        self.color = Color {
            bt709: f.colorspace == ff::AVColorSpace::AVCOL_SPC_BT709,
            full_range: f.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG,
        };
        let cu = self.cu;
        let p = self.planes.as_mut().unwrap();
        cuda::check((cu.cuCtxPushCurrent)(self.cuctx), "cuCtxPushCurrent")?;
        let result = (|| -> Res<()> {
            cuda::check(
                (cu.cuGraphicsMapResources)(3, p.res.as_mut_ptr(), ptr::null_mut()),
                "map",
            )?;
            let mut copy = Ok(());
            for i in 0..3 {
                let mut array = ptr::null_mut();
                let r = (cu.cuGraphicsSubResourceGetMappedArray)(&mut array, p.res[i], 0, 0);
                if let Err(e) = cuda::check(r, "mapped array") {
                    copy = Err(e);
                    break;
                }
                let m = Memcpy2D {
                    src_memory_type: CU_MEMORYTYPE_DEVICE,
                    src_device: f.data[i] as u64,
                    src_pitch: f.linesize[i] as usize,
                    dst_memory_type: CU_MEMORYTYPE_ARRAY,
                    dst_array: array as usize,
                    width_in_bytes: w as usize,
                    height: h as usize,
                    ..Default::default()
                };
                if let Err(e) = cuda::check((cu.cuMemcpy2D)(&m), "copy plane") {
                    copy = Err(e);
                    break;
                }
            }
            cuda::check(
                (cu.cuGraphicsUnmapResources)(3, p.res.as_mut_ptr(), ptr::null_mut()),
                "unmap",
            )?;
            copy
        })();
        let mut old = ptr::null_mut();
        (cu.cuCtxPopCurrent)(&mut old);
        result
    }
}

impl Drop for CudaDecoder {
    fn drop(&mut self) {
        self.release_planes();
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

/// `glass-cli decode444-test FILE`: an HEVC stream (`nvenc-test --444
/// --dump`) through this decoder one access unit at a time, printing
/// whether each one's picture came out at once (a still picture's last
/// frame must, or it's never shown).
pub fn self_test(args: &[String]) -> Res<()> {
    let file = args.first().ok_or("usage: decode444-test FILE")?;
    let data = std::fs::read(file)?;
    // NAL starts (after 00 00 01).
    let mut nals = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            nals.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    // Access units: a new one at a parameter set / AUD after a picture, or
    // at a picture's first slice.
    let mut aus: Vec<usize> = Vec::new();
    let mut seen_vcl = false;
    for &n in &nals {
        let t = (data[n] >> 1) & 0x3f;
        let vcl = t < 32;
        let first_slice = vcl && data.get(n + 2).is_some_and(|b| b & 0x80 != 0);
        if aus.is_empty() || (seen_vcl && (!vcl || first_slice)) {
            // Back up over the start code (and a 4-byte one's extra zero).
            let mut s = n - 3;
            if s > 0 && data[s - 1] == 0 {
                s -= 1;
            }
            aus.push(s);
            seen_vcl = false;
        }
        seen_vcl |= vcl;
    }
    aus.push(data.len());
    super::d3d::init_process();
    let gpu = super::d3d::Gpu::new_nvidia()?;
    let mut dec = CudaDecoder::new(&gpu.device)?;
    let mut shown = 0;
    for (k, w) in aus.windows(2).enumerate() {
        let got = dec.decode(&data[w[0]..w[1]])?.is_some();
        shown += got as usize;
        println!(
            "access unit {k}: {:>8} bytes -> {}",
            w[1] - w[0],
            if got { "picture out" } else { "nothing yet" }
        );
    }
    println!("{} access units, {shown} pictures out", aus.len() - 1);
    Ok(())
}

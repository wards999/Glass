//! HEVC decode with FFmpeg on D3D11VA. Annex-B access units go in; decoded
//! pictures come out as a slice of FFmpeg's D3D11 texture array, so frames
//! never touch the CPU. The picture stays valid until the next `decode`.

use super::Res;
use ffmpeg_sys_next as ff;
use std::ffi::{c_void, CStr};
use std::ptr;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

const EAGAIN: i32 = -11; // AVERROR(EAGAIN) on Windows
const PADDING: usize = ff::AV_INPUT_BUFFER_PADDING_SIZE as usize;

/// How the picture's YCbCr values are to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub bt709: bool,
    pub full_range: bool,
}

/// A decoded picture (borrowed from the decoder until the next `decode`).
pub struct Picture {
    pub texture: ID3D11Texture2D,
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub color: Color,
}

pub struct Decoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    next: *mut ff::AVFrame,
    input: Vec<u8>,
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

/// Prefer D3D11 hardware frames; FFmpeg offers them only if the GPU can
/// decode this profile.
unsafe extern "C" fn get_format(
    _ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    let mut p = fmts;
    while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
        if *p == ff::AVPixelFormat::AV_PIX_FMT_D3D11 {
            return *p;
        }
        p = p.add(1);
    }
    *fmts
}

impl Decoder {
    pub fn new(device: &ID3D11Device) -> Res<Self> {
        unsafe {
            let mut dev_ref =
                ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if dev_ref.is_null() {
                return Err("av_hwdevice_ctx_alloc failed".into());
            }
            let devctx = (*dev_ref).data as *mut ff::AVHWDeviceContext;
            let d3dctx = (*devctx).hwctx as *mut ff::AVD3D11VADeviceContext;
            // FFmpeg releases this reference when the device context is freed.
            (*d3dctx).device = device.clone().into_raw() as *mut _;
            if let Err(e) = check(ff::av_hwdevice_ctx_init(dev_ref), "av_hwdevice_ctx_init") {
                ff::av_buffer_unref(&mut dev_ref);
                return Err(e);
            }
            let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_HEVC);
            if codec.is_null() {
                ff::av_buffer_unref(&mut dev_ref);
                return Err("no HEVC decoder in this FFmpeg build".into());
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            (*ctx).hw_device_ctx = dev_ref; // takes our reference
            (*ctx).get_format = Some(get_format);
            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).thread_count = 1; // frame threads would add a frame of delay each
            (*ctx).extra_hw_frames = 4; // we hold one picture while the next decodes
            let mut ctx_ptr = ctx;
            if let Err(e) = check(
                ff::avcodec_open2(ctx, codec, ptr::null_mut()),
                "avcodec_open2(hevc)",
            ) {
                ff::avcodec_free_context(&mut ctx_ptr);
                return Err(e);
            }
            Ok(Self {
                ctx,
                pkt: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                next: ff::av_frame_alloc(),
                input: Vec::with_capacity(1 << 20),
            })
        }
    }

    /// Decodes one access unit. Returns the newest picture it produced, or
    /// `None` (e.g. before the first IDR).
    pub fn decode(&mut self, au: &[u8]) -> Res<Option<Picture>> {
        // FFmpeg reads past the end; the buffer only grows past the largest frame.
        self.input.clear();
        self.input.extend_from_slice(au);
        self.input.resize(au.len() + PADDING, 0);
        let mut got = false;
        unsafe {
            (*self.pkt).data = self.input.as_mut_ptr();
            (*self.pkt).size = au.len() as i32;
            let r = ff::avcodec_send_packet(self.ctx, self.pkt);
            (*self.pkt).data = ptr::null_mut();
            (*self.pkt).size = 0;
            check(r, "decode")?;
            loop {
                let r = ff::avcodec_receive_frame(self.ctx, self.next);
                if r == EAGAIN || r == ff::AVERROR_EOF {
                    break;
                }
                check(r, "decode")?;
                std::mem::swap(&mut self.frame, &mut self.next);
                ff::av_frame_unref(self.next);
                got = true;
            }
            if !got {
                return Ok(None);
            }
            let f = &*self.frame;
            if f.format != ff::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 {
                return Err(
                    "this GPU can't hardware-decode the stream (software decode isn't supported yet)"
                        .into(),
                );
            }
            let raw = f.data[0] as *mut c_void;
            let texture = ID3D11Texture2D::from_raw_borrowed(&raw)
                .ok_or("decoder returned no texture")?
                .clone();
            Ok(Some(Picture {
                texture,
                index: f.data[1] as usize as u32,
                width: f.width as u32,
                height: f.height as u32,
                color: Color {
                    bt709: f.colorspace == ff::AVColorSpace::AVCOL_SPC_BT709,
                    full_range: f.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG,
                },
            }))
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.next);
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

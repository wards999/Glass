//! HEVC encode with FFmpeg `hevc_nvenc` on D3D11 hardware frames. The capture
//! texture is copied GPU->GPU into an FFmpeg pool texture; NVENC reads BGRA and
//! converts to YUV itself, so frames never touch the CPU.

use super::Res;
use ffmpeg_sys_next as ff;
use std::ffi::{c_void, CStr, CString};
use std::ptr;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};

pub struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: i64,
    pub chroma444: bool,
    /// Keep a GPU copy of the last frame so an IDR can be sent on request
    /// even when the screen is idle and capture delivers nothing new.
    pub keep_last: bool,
}

pub struct Encoder {
    ctx: *mut ff::AVCodecContext,
    frames: *mut ff::AVBufferRef,
    frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
    d3d: ID3D11DeviceContext,
    device: ID3D11Device,
    keep_last: bool,
    last: Option<ID3D11Texture2D>,
}

const EAGAIN: i32 = -11; // AVERROR(EAGAIN) on Windows

fn av_err(code: i32, what: &str) -> Box<dyn std::error::Error> {
    let mut buf = [0 as std::ffi::c_char; 128];
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    format!("{what}: {msg} ({code})").into()
}

fn check(code: i32, what: &str) -> Res<i32> {
    if code < 0 {
        Err(av_err(code, what))
    } else {
        Ok(code)
    }
}

impl Encoder {
    pub fn new(device: &ID3D11Device, cfg: &Config) -> Res<Self> {
        unsafe {
            // Hardware device context wrapping our own D3D11 device.
            let mut dev_ref =
                ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if dev_ref.is_null() {
                return Err("av_hwdevice_ctx_alloc failed".into());
            }
            let devctx = (*dev_ref).data as *mut ff::AVHWDeviceContext;
            let d3dctx = (*devctx).hwctx as *mut ff::AVD3D11VADeviceContext;
            // FFmpeg releases this reference when the device context is freed.
            (*d3dctx).device = device.clone().into_raw() as *mut _;
            check(ff::av_hwdevice_ctx_init(dev_ref), "av_hwdevice_ctx_init")?;

            let frames = ff::av_hwframe_ctx_alloc(dev_ref);
            ff::av_buffer_unref(&mut dev_ref);
            if frames.is_null() {
                return Err("av_hwframe_ctx_alloc failed".into());
            }
            let fc = (*frames).data as *mut ff::AVHWFramesContext;
            (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*fc).sw_format = ff::AVPixelFormat::AV_PIX_FMT_BGRA;
            (*fc).width = cfg.width as i32;
            (*fc).height = cfg.height as i32;
            (*fc).initial_pool_size = 8;
            check(ff::av_hwframe_ctx_init(frames), "av_hwframe_ctx_init")?;

            let codec = ff::avcodec_find_encoder_by_name(c"hevc_nvenc".as_ptr());
            if codec.is_null() {
                return Err("hevc_nvenc not available in this FFmpeg build".into());
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            (*ctx).width = cfg.width as i32;
            (*ctx).height = cfg.height as i32;
            (*ctx).time_base = ff::AVRational {
                num: 1,
                den: 1_000_000,
            }; // pts in µs
            (*ctx).framerate = ff::AVRational {
                num: cfg.fps as i32,
                den: 1,
            };
            (*ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*ctx).sw_pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_BGRA;
            (*ctx).hw_frames_ctx = ff::av_buffer_ref(frames);
            (*ctx).max_b_frames = 0;
            // IDRs only at the start and on request (a loss the viewer
            // couldn't recover, a new viewer): no periodic one, which was a
            // burst every 10 s for nothing (M4 step 3, ADR 0022). i32::MAX
            // frames is NVENC's GOP for "never" in practice.
            (*ctx).gop_size = i32::MAX;
            (*ctx).bit_rate = cfg.bitrate;
            (*ctx).rc_max_rate = cfg.bitrate;
            (*ctx).rc_buffer_size = (cfg.bitrate / cfg.fps as i64) as i32; // ~1-frame VBV
                                                                           // NVENC's internal RGB->YUV conversion is BT.601 limited range and
                                                                           // FFmpeg writes that into the VUI regardless; decoders must read it.
            (*ctx).color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;

            let opts: [(&str, &str); 9] = [
                ("preset", "p1"),
                ("tune", "ull"),
                ("rc", "cbr"),
                ("zerolatency", "1"),
                ("delay", "0"),
                ("rc-lookahead", "0"),
                ("forced-idr", "1"),
                ("rgb_mode", if cfg.chroma444 { "yuv444" } else { "yuv420" }),
                ("profile", if cfg.chroma444 { "rext" } else { "main" }),
            ];
            let mut dict: *mut ff::AVDictionary = ptr::null_mut();
            for (k, v) in opts {
                let (k, v) = (CString::new(k)?, CString::new(v)?);
                ff::av_dict_set(&mut dict, k.as_ptr(), v.as_ptr(), 0);
            }
            let r = ff::avcodec_open2(ctx, codec, &mut dict);
            let mut e = ptr::null();
            loop {
                e = ff::av_dict_iterate(dict, e);
                if e.is_null() {
                    break;
                }
                eprintln!(
                    "warning: nvenc ignored option {}",
                    CStr::from_ptr((*e).key).to_string_lossy()
                );
            }
            ff::av_dict_free(&mut dict);
            check(r, "avcodec_open2(hevc_nvenc)")?;

            Ok(Self {
                ctx,
                frames,
                frame: ff::av_frame_alloc(),
                pkt: ff::av_packet_alloc(),
                d3d: device.GetImmediateContext()?,
                device: device.clone(),
                keep_last: cfg.keep_last,
                last: None,
            })
        }
    }

    /// Encodes one captured texture. `on_packet(data, is_keyframe, pts_us)` is
    /// called for every packet the encoder emits (with delay=0: this frame's).
    pub fn encode(
        &mut self,
        src: &ID3D11Texture2D,
        pts_us: i64,
        force_idr: bool,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        if self.keep_last {
            if self.last.is_none() {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { src.GetDesc(&mut desc) };
                desc.Usage = D3D11_USAGE_DEFAULT;
                desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
                desc.CPUAccessFlags = 0;
                desc.MiscFlags = 0;
                let mut t = None;
                unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t))? };
                self.last = t;
            }
            if let Some(last) = &self.last {
                unsafe { self.d3d.CopyResource(last, src) };
            }
        }
        self.encode_texture(src, pts_us, force_idr, on_packet)
    }

    /// Keeps `src` as the last frame without encoding it (a GPU copy): the
    /// frame-rate cap holds the newest frame until it's time to send one.
    pub fn keep(&mut self, src: &ID3D11Texture2D) {
        if !self.keep_last {
            return;
        }
        if self.last.is_none() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { src.GetDesc(&mut desc) };
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
            desc.CPUAccessFlags = 0;
            desc.MiscFlags = 0;
            let mut t = None;
            if unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut t)) }.is_err() {
                return;
            }
            self.last = t;
        }
        if let Some(last) = &self.last {
            unsafe { self.d3d.CopyResource(last, src) };
        }
    }

    /// Encodes the kept frame (not an IDR unless `force_idr`).
    pub fn encode_last(
        &mut self,
        pts_us: i64,
        force_idr: bool,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        let Some(last) = self.last.clone() else {
            return Ok(());
        };
        self.encode_texture(&last, pts_us, force_idr, on_packet)
    }

    /// True once a frame has been kept for [`Self::encode_last_idr`].
    pub fn has_last(&self) -> bool {
        self.last.is_some()
    }

    /// Re-encodes the last captured frame as an IDR (idle screen + keyframe request).
    pub fn encode_last_idr(
        &mut self,
        pts_us: i64,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        let Some(last) = self.last.clone() else {
            return Ok(());
        };
        self.encode_texture(&last, pts_us, true, on_packet)
    }

    fn encode_texture(
        &mut self,
        src: &ID3D11Texture2D,
        pts_us: i64,
        force_idr: bool,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        unsafe {
            // Pooled texture: no D3D allocation after warm-up.
            check(
                ff::av_hwframe_get_buffer(self.frames, self.frame, 0),
                "av_hwframe_get_buffer",
            )?;
            let raw = (*self.frame).data[0] as *mut c_void;
            let index = (*self.frame).data[1] as usize as u32;
            let dst = ID3D11Texture2D::from_raw_borrowed(&raw).ok_or("null hw frame texture")?;
            self.d3d
                .CopySubresourceRegion(dst, index, 0, 0, 0, src, 0, None);
            (*self.frame).pts = pts_us;
            (*self.frame).pict_type = if force_idr {
                ff::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ff::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ff::avcodec_send_frame(self.ctx, self.frame);
            ff::av_frame_unref(self.frame);
            check(r, "avcodec_send_frame")?;
        }
        self.drain(on_packet)
    }

    pub fn flush(&mut self, on_packet: impl FnMut(&[u8], bool, i64)) -> Res<()> {
        unsafe { check(ff::avcodec_send_frame(self.ctx, ptr::null()), "flush")? };
        self.drain(on_packet)
    }

    fn drain(&mut self, mut on_packet: impl FnMut(&[u8], bool, i64)) -> Res<()> {
        loop {
            let r = unsafe { ff::avcodec_receive_packet(self.ctx, self.pkt) };
            if r == EAGAIN || r == ff::AVERROR_EOF {
                return Ok(());
            }
            check(r, "avcodec_receive_packet")?;
            unsafe {
                let p = &*self.pkt;
                let data = std::slice::from_raw_parts(p.data, p.size as usize);
                on_packet(data, p.flags & ff::AV_PKT_FLAG_KEY != 0, p.pts);
                ff::av_packet_unref(self.pkt);
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_packet_free(&mut self.pkt);
            ff::av_frame_free(&mut self.frame);
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_buffer_unref(&mut self.frames);
        }
    }
}

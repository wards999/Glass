//! HEVC encode straight through NVIDIA's NVENC API (M8, brought forward),
//! instead of FFmpeg's `hevc_nvenc`. FFmpeg restarts the encoder with a
//! keyframe at every rate-control change and has no per-frame quality, so
//! a still photo could never be sent sharp (m5-test22..24) and every
//! bitrate step cost a keyframe (m5-test10). Here:
//! - bitrate changes go through `nvEncReconfigureEncoder` without a reset
//!   or a keyframe;
//! - a still picture is sent once at a fixed high quality (constant QP for
//!   that one keyframe), then rate control carries on where it was.
//!
//! Same input and output as the FFmpeg path: the captured BGRA texture is
//! copied GPU->GPU into a registered texture, NVENC converts to YUV itself
//! (BT.601, limited range, written into the VUI as FFmpeg did), Annex B
//! access units come out, SPS/PPS repeated on keyframes.
//!
//! The bindings (`nvenc_sys.rs`) are generated from NVIDIA's
//! `nvEncodeAPI.h` (nv-codec-headers n12.1.14.1, MIT) by
//! `scripts/nvenc-bindings.ps1`; the driver's `nvEncodeAPI64.dll` is loaded
//! at run time (API 12.1: drivers 531 and newer).

#![allow(non_snake_case)]

use super::nvenc_sys as nv;
use super::Res;
use std::ffi::c_void;
use std::ptr;
use windows::core::{s, w, Interface};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

pub use super::encode::Config;

const API_VERSION: u32 = 12 | (1 << 24);
const fn struct_version(v: u32) -> u32 {
    API_VERSION | (v << 16) | (0x7 << 28)
}
const OPEN_SESSION_VER: u32 = struct_version(1);
const INIT_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
const CONFIG_VER: u32 = struct_version(8) | (1 << 31);
const PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
const RECONFIGURE_VER: u32 = struct_version(1) | (1 << 31);
const PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
const LOCK_BITSTREAM_VER: u32 = struct_version(1) | (1 << 31);
const MAP_INPUT_VER: u32 = struct_version(4);
const REGISTER_VER: u32 = struct_version(4);
const BITSTREAM_VER: u32 = struct_version(1);
const RC_PARAMS_VER: u32 = struct_version(1);
const FUNCTION_LIST_VER: u32 = struct_version(2);
const INFINITE_GOP: u32 = 0xffff_ffff;

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> nv::GUID {
    nv::GUID {
        Data1: d1,
        Data2: d2,
        Data3: d3,
        Data4: d4,
    }
}
const CODEC_HEVC: nv::GUID = guid(
    0x790cdc88,
    0x4522,
    0x4d7b,
    [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
);
const PRESET_P1: nv::GUID = guid(
    0xfc0a8d3e,
    0x45f8,
    0x4cf8,
    [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
);
const PROFILE_MAIN: nv::GUID = guid(
    0xb514c39a,
    0xb55b,
    0x40fa,
    [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec],
);
const PROFILE_FREXT: nv::GUID = guid(
    0x51ec32b5,
    0x1b4c,
    0x453c,
    [0x9c, 0xbd, 0xb6, 0x16, 0xbd, 0x62, 0x13, 0x41],
);

/// The QP of a still picture's keyframe for a link of `limit_bps` (0: no
/// limit): near photo quality where there's room (QUIETPC22, a grainy
/// 2560x1440 test picture in 4:4:4: 26.2 dB at the encoder's own rate
/// control, 31.4 dB at QP 20, 2.2 MB once), coarser on a squeezed link so
/// the one frame doesn't take long to arrive.
pub fn still_qp(limit_bps: i64) -> u32 {
    match limit_bps {
        0 => 20,
        l if l >= 60_000_000 => 20,
        l if l >= 20_000_000 => 24,
        _ => 28,
    }
}

type CreateInstance = unsafe extern "C" fn(*mut nv::NV_ENCODE_API_FUNCTION_LIST) -> nv::NVENCSTATUS;
type MaxVersion = unsafe extern "C" fn(*mut u32) -> nv::NVENCSTATUS;

/// An NVENC entry point from the function list (all are filled in by
/// `NvEncodeAPICreateInstance`).
macro_rules! nvf {
    ($o:expr) => {
        $o.ok_or("NVENC function missing")?
    };
}

fn ok(r: nv::NVENCSTATUS, what: &str) -> Res<()> {
    if r == nv::NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(format!("NVENC {what}: error {r}").into())
    }
}

pub struct Nvenc {
    api: nv::NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    init: Box<nv::NV_ENC_INITIALIZE_PARAMS>,
    config: Box<nv::NV_ENC_CONFIG>,
    device: ID3D11Device,
    d3d: ID3D11DeviceContext,
    input: ID3D11Texture2D,
    registered: nv::NV_ENC_REGISTERED_PTR,
    bitstream: nv::NV_ENC_OUTPUT_PTR,
    width: u32,
    height: u32,
    keep_last: bool,
    last: Option<ID3D11Texture2D>,
    frame_idx: u32,
    /// Bitrate rate control runs at (bits/s).
    bps: i64,
    /// `GLASS_NVENC_DUMP=<file>`: the first frames as raw HEVC, to check
    /// the stream decodes (testing only).
    dump: Option<(std::fs::File, u32)>,
}

// SAFETY: the encoder session and its buffers are used from one thread at
// a time (the session's encode loop owns the `Nvenc`).
unsafe impl Send for Nvenc {}

impl Nvenc {
    pub fn new(device: &ID3D11Device, cfg: &Config) -> Res<Self> {
        unsafe {
            let lib = LoadLibraryW(w!("nvEncodeAPI64.dll")).map_err(|_| {
                "NVENC isn't available (no nvEncodeAPI64.dll: an NVIDIA driver is needed)"
            })?;
            let max: MaxVersion = std::mem::transmute(
                GetProcAddress(lib, s!("NvEncodeAPIGetMaxSupportedVersion"))
                    .ok_or("NvEncodeAPIGetMaxSupportedVersion missing")?,
            );
            let create: CreateInstance = std::mem::transmute(
                GetProcAddress(lib, s!("NvEncodeAPICreateInstance"))
                    .ok_or("NvEncodeAPICreateInstance missing")?,
            );
            let mut have = 0u32;
            ok(max(&mut have), "version")?;
            // `have` is major << 4 | minor.
            if have < (12 << 4 | 1) {
                return Err(format!(
                    "the NVIDIA driver supports NVENC API {}.{}; 12.1 is needed (update the driver)",
                    have >> 4,
                    have & 0xf
                )
                .into());
            }
            let mut api: nv::NV_ENCODE_API_FUNCTION_LIST = std::mem::zeroed();
            api.version = FUNCTION_LIST_VER;
            ok(create(&mut api), "function list")?;

            let mut open: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
            open.version = OPEN_SESSION_VER;
            open.deviceType = nv::NV_ENC_DEVICE_TYPE_DIRECTX;
            open.device = device.as_raw();
            open.apiVersion = API_VERSION;
            let mut enc = ptr::null_mut();
            ok(
                nvf!(api.nvEncOpenEncodeSessionEx)(&mut open, &mut enc),
                "open session",
            )?;
            let mut me = Self {
                api,
                enc,
                init: Box::new(std::mem::zeroed()),
                config: Box::new(std::mem::zeroed()),
                device: device.clone(),
                d3d: device.GetImmediateContext()?,
                input: make_texture(device, cfg.width, cfg.height)?,
                registered: ptr::null_mut(),
                bitstream: ptr::null_mut(),
                width: cfg.width,
                height: cfg.height,
                keep_last: cfg.keep_last,
                last: None,
                frame_idx: 0,
                bps: cfg.bitrate,
                dump: std::env::var_os("GLASS_NVENC_DUMP")
                    .and_then(|p| std::fs::File::create(p).ok())
                    .map(|f| (f, 0)),
            };
            me.configure(cfg)?;
            ok(
                nvf!(me.api.nvEncInitializeEncoder)(me.enc, &mut *me.init),
                "initialize",
            )?;

            let mut reg: nv::NV_ENC_REGISTER_RESOURCE = std::mem::zeroed();
            reg.version = REGISTER_VER;
            reg.resourceType = nv::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            reg.width = cfg.width;
            reg.height = cfg.height;
            reg.resourceToRegister = me.input.as_raw();
            reg.bufferFormat = nv::NV_ENC_BUFFER_FORMAT_ARGB;
            reg.bufferUsage = nv::NV_ENC_INPUT_IMAGE;
            ok(
                nvf!(me.api.nvEncRegisterResource)(me.enc, &mut reg),
                "register input",
            )?;
            me.registered = reg.registeredResource;

            let mut bs: nv::NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
            bs.version = BITSTREAM_VER;
            ok(
                nvf!(me.api.nvEncCreateBitstreamBuffer)(me.enc, &mut bs),
                "bitstream buffer",
            )?;
            me.bitstream = bs.bitstreamBuffer;
            Ok(me)
        }
    }

    /// The preset (P1, ultra-low latency) and Glass's settings on top: CBR
    /// at `bitrate` with a tenth of a second of buffer, no B-frames, no
    /// periodic keyframes, SPS/PPS on every keyframe, 4:4:4 (Rext) or
    /// 4:2:0 (Main), BT.601 limited range in the VUI.
    unsafe fn configure(&mut self, cfg: &Config) -> Res<()> {
        let mut preset: nv::NV_ENC_PRESET_CONFIG = std::mem::zeroed();
        preset.version = PRESET_CONFIG_VER;
        preset.presetCfg.version = CONFIG_VER;
        ok(
            nvf!(self.api.nvEncGetEncodePresetConfigEx)(
                self.enc,
                CODEC_HEVC,
                PRESET_P1,
                nv::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut preset,
            ),
            "preset",
        )?;
        let c = &mut *self.config;
        *c = preset.presetCfg;
        c.version = CONFIG_VER;
        c.profileGUID = if cfg.chroma444 {
            PROFILE_FREXT
        } else {
            PROFILE_MAIN
        };
        c.gopLength = INFINITE_GOP;
        c.frameIntervalP = 1;
        let rc = &mut c.rcParams;
        rc.version = RC_PARAMS_VER;
        rc.rateControlMode = nv::NV_ENC_PARAMS_RC_CBR;
        rc.averageBitRate = cfg.bitrate.clamp(1, u32::MAX as i64) as u32;
        rc.maxBitRate = rc.averageBitRate;
        rc.vbvBufferSize = (cfg.bitrate / 10).clamp(1, u32::MAX as i64) as u32;
        rc.vbvInitialDelay = rc.vbvBufferSize;
        // One pass, as FFmpeg's default: the preset's two-pass mode made
        // encodes ~70 % slower here (6.0 ms against 3.5 ms p50).
        rc.multiPass = nv::NV_ENC_MULTI_PASS_DISABLED;
        rc.set_zeroReorderDelay(1);
        rc.set_enableLookahead(0);
        let h = &mut c.encodeCodecConfig.hevcConfig;
        h.idrPeriod = INFINITE_GOP;
        h.set_repeatSPSPPS(1);
        h.set_chromaFormatIDC(if cfg.chroma444 { 3 } else { 1 });
        let v = &mut h.hevcVUIParameters;
        v.videoSignalTypePresentFlag = 1;
        v.videoFormat = 5; // unspecified
        v.videoFullRangeFlag = 0;
        v.colourDescriptionPresentFlag = 1;
        v.colourMatrix = 5; // BT.470BG = BT.601, as NVENC converts
        v.colourPrimaries = 2;
        v.transferCharacteristics = 2;

        let i = &mut *self.init;
        i.version = INIT_PARAMS_VER;
        i.encodeGUID = CODEC_HEVC;
        i.presetGUID = PRESET_P1;
        i.tuningInfo = nv::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        i.encodeWidth = cfg.width;
        i.encodeHeight = cfg.height;
        i.darWidth = cfg.width;
        i.darHeight = cfg.height;
        i.maxEncodeWidth = cfg.width;
        i.maxEncodeHeight = cfg.height;
        i.frameRateNum = cfg.fps.max(1);
        i.frameRateDen = 1;
        i.enablePTD = 1;
        i.encodeConfig = &mut *self.config;
        Ok(())
    }

    /// Applies the current rate control without a reset or a keyframe.
    fn reconfigure(&mut self) -> Res<()> {
        unsafe {
            let mut p: nv::NV_ENC_RECONFIGURE_PARAMS = std::mem::zeroed();
            p.version = RECONFIGURE_VER;
            p.reInitEncodeParams = *self.init;
            p.reInitEncodeParams.encodeConfig = &mut *self.config;
            p.set_resetEncoder(0);
            p.set_forceIDR(0);
            ok(
                nvf!(self.api.nvEncReconfigureEncoder)(self.enc, &mut p),
                "reconfigure",
            )
        }
    }

    /// Changes the bitrate from the next frame, without a keyframe (FFmpeg's
    /// NVENC made one every time: m5-test10).
    pub fn set_bitrate(&mut self, bps: i64) {
        self.bps = bps;
        let rc = &mut self.config.rcParams;
        rc.averageBitRate = bps.clamp(1, u32::MAX as i64) as u32;
        rc.maxBitRate = rc.averageBitRate;
        rc.vbvBufferSize = (bps / 10).clamp(1, u32::MAX as i64) as u32;
        rc.vbvInitialDelay = rc.vbvBufferSize;
        if let Err(e) = self.reconfigure() {
            eprintln!("{e}");
        }
    }

    pub fn encode(
        &mut self,
        src: &ID3D11Texture2D,
        pts_us: i64,
        force_idr: bool,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        self.keep(src);
        unsafe { self.d3d.CopyResource(&self.input, src) };
        self.encode_input(pts_us, force_idr, on_packet)
    }

    /// Keeps `src` as the last frame without encoding it (the frame-rate
    /// cap holds the newest frame until it's time to send one).
    pub fn keep(&mut self, src: &ID3D11Texture2D) {
        if !self.keep_last {
            return;
        }
        if self.last.is_none() {
            self.last = make_texture(&self.device, self.width, self.height).ok();
        }
        if let Some(last) = &self.last {
            unsafe { self.d3d.CopyResource(last, src) };
        }
    }

    pub fn has_last(&self) -> bool {
        self.last.is_some()
    }

    pub fn encode_last(
        &mut self,
        pts_us: i64,
        force_idr: bool,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        let Some(last) = self.last.clone() else {
            return Ok(());
        };
        unsafe { self.d3d.CopyResource(&self.input, &last) };
        self.encode_input(pts_us, force_idr, on_packet)
    }

    pub fn encode_last_idr(
        &mut self,
        pts_us: i64,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        self.encode_last(pts_us, true, on_packet)
    }

    /// The still picture as one keyframe at a fixed high quality (constant
    /// QP), then rate control as it was: neither change resets the encoder
    /// or forces another keyframe (FFmpeg did both: m5-test23's blur cycle).
    pub fn encode_last_quality(
        &mut self,
        pts_us: i64,
        qp: u32,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        let saved = self.config.rcParams;
        {
            let rc = &mut self.config.rcParams;
            rc.rateControlMode = nv::NV_ENC_PARAMS_RC_CONSTQP;
            rc.constQP = nv::NV_ENC_QP {
                qpInterP: qp,
                qpInterB: qp,
                qpIntra: qp,
            };
        }
        let r = self
            .reconfigure()
            .and_then(|()| self.encode_last(pts_us, true, on_packet));
        self.config.rcParams = saved;
        let back = self.reconfigure();
        r.and(back)
    }

    fn encode_input(
        &mut self,
        pts_us: i64,
        force_idr: bool,
        mut on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        unsafe {
            let mut map: nv::NV_ENC_MAP_INPUT_RESOURCE = std::mem::zeroed();
            map.version = MAP_INPUT_VER;
            map.registeredResource = self.registered;
            ok(
                nvf!(self.api.nvEncMapInputResource)(self.enc, &mut map),
                "map",
            )?;
            let mut pic: nv::NV_ENC_PIC_PARAMS = std::mem::zeroed();
            pic.version = PIC_PARAMS_VER;
            pic.inputWidth = self.width;
            pic.inputHeight = self.height;
            pic.inputPitch = 0;
            pic.inputBuffer = map.mappedResource;
            pic.bufferFmt = map.mappedBufferFmt;
            pic.outputBitstream = self.bitstream;
            pic.pictureStruct = nv::NV_ENC_PIC_STRUCT_FRAME;
            pic.inputTimeStamp = pts_us as u64;
            pic.frameIdx = self.frame_idx;
            self.frame_idx = self.frame_idx.wrapping_add(1);
            if force_idr {
                pic.encodePicFlags =
                    (nv::NV_ENC_PIC_FLAG_FORCEIDR | nv::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS) as u32;
            }
            let r = nvf!(self.api.nvEncEncodePicture)(self.enc, &mut pic);
            let out = if r == nv::NV_ENC_SUCCESS {
                let mut lock: nv::NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
                lock.version = LOCK_BITSTREAM_VER;
                lock.outputBitstream = self.bitstream;
                ok(
                    nvf!(self.api.nvEncLockBitstream)(self.enc, &mut lock),
                    "lock",
                )?;
                let data = std::slice::from_raw_parts(
                    lock.bitstreamBufferPtr as *const u8,
                    lock.bitstreamSizeInBytes as usize,
                );
                let key = lock.pictureType == nv::NV_ENC_PIC_TYPE_IDR
                    || lock.pictureType == nv::NV_ENC_PIC_TYPE_I;
                if let Some((file, n)) = &mut self.dump {
                    if *n < 300 {
                        use std::io::Write;
                        let _ = file.write_all(data);
                        *n += 1;
                    }
                }
                if !data.is_empty() {
                    on_packet(data, key, lock.outputTimeStamp as i64);
                }
                nvf!(self.api.nvEncUnlockBitstream)(self.enc, self.bitstream);
                Ok(())
            } else {
                ok(r, "encode")
            };
            nvf!(self.api.nvEncUnmapInputResource)(self.enc, map.mappedResource);
            out
        }
    }
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        unsafe {
            if let Some(d) = self.api.nvEncDestroyBitstreamBuffer {
                if !self.bitstream.is_null() {
                    d(self.enc, self.bitstream);
                }
            }
            if let Some(u) = self.api.nvEncUnregisterResource {
                if !self.registered.is_null() {
                    u(self.enc, self.registered);
                }
            }
            if let Some(d) = self.api.nvEncDestroyEncoder {
                d(self.enc);
            }
        }
    }
}

/// A BGRA texture NVENC can read (and Glass copies captures into).
fn make_texture(device: &ID3D11Device, w: u32, h: u32) -> Res<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut t))? };
    t.ok_or_else(|| "no texture".into())
}

/// The session's encoder: NVENC directly, or FFmpeg's `hevc_nvenc` if the
/// direct path can't start (or `GLASS_ENCODER=ffmpeg`).
pub enum Video {
    Direct(Box<Nvenc>),
    Ffmpeg(super::encode::Encoder),
}

macro_rules! both {
    ($self:ident, $e:ident => $body:expr) => {
        match $self {
            Video::Direct($e) => $body,
            Video::Ffmpeg($e) => $body,
        }
    };
}

impl Video {
    pub fn new(device: &ID3D11Device, cfg: &Config) -> Res<Self> {
        if std::env::var("GLASS_ENCODER").is_ok_and(|v| v.eq_ignore_ascii_case("ffmpeg")) {
            return Ok(Self::Ffmpeg(super::encode::Encoder::new(device, cfg)?));
        }
        match Nvenc::new(device, cfg) {
            Ok(n) => Ok(Self::Direct(Box::new(n))),
            Err(e) => {
                eprintln!("direct NVENC unavailable ({e}); using FFmpeg's");
                Ok(Self::Ffmpeg(super::encode::Encoder::new(device, cfg)?))
            }
        }
    }

    /// For the results.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Direct(_) => "NVENC (direct)",
            Self::Ffmpeg(_) => "NVENC (through FFmpeg)",
        }
    }

    pub fn encode(
        &mut self,
        src: &ID3D11Texture2D,
        pts_us: i64,
        force_idr: bool,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        both!(self, e => e.encode(src, pts_us, force_idr, on_packet))
    }

    pub fn keep(&mut self, src: &ID3D11Texture2D) {
        both!(self, e => e.keep(src))
    }

    pub fn has_last(&self) -> bool {
        both!(self, e => e.has_last())
    }

    pub fn encode_last(
        &mut self,
        pts_us: i64,
        force_idr: bool,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        both!(self, e => e.encode_last(pts_us, force_idr, on_packet))
    }

    pub fn encode_last_idr(
        &mut self,
        pts_us: i64,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        both!(self, e => e.encode_last_idr(pts_us, on_packet))
    }

    /// The still picture as a keyframe: at `qp` on the direct path; the
    /// FFmpeg path can only send a plain keyframe.
    pub fn encode_last_quality(
        &mut self,
        pts_us: i64,
        qp: u32,
        on_packet: impl FnMut(&[u8], bool, i64),
    ) -> Res<()> {
        match self {
            Video::Direct(e) => e.encode_last_quality(pts_us, qp, on_packet),
            Video::Ffmpeg(e) => e.encode_last_quality(pts_us, on_packet),
        }
    }

    pub fn set_bitrate(&mut self, bps: i64) {
        both!(self, e => e.set_bitrate(bps))
    }
}

/// `glass-cli nvenc-test [--dump FILE] [--444]`: one captured frame through
/// the direct encoder: a keyframe, two repeats, a bitrate change (must not
/// make a keyframe), the still picture's quality keyframe, and one more
/// frame after it (must not be a keyframe either). Sizes and types printed.
pub fn self_test(args: &[String]) -> Res<()> {
    let dump = args
        .iter()
        .position(|a| a == "--dump")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let chroma444 = args.iter().any(|a| a == "--444");
    super::d3d::init_process();
    let gpu = super::d3d::Gpu::new_nvidia()?;
    let mon = super::capture::monitors()?
        .into_iter()
        .next()
        .ok_or("no monitor")?;
    // `--image FILE.bgra W H`: a raw BGRA picture instead of the screen
    // (a detailed photo, to measure quality).
    let image = args.iter().position(|a| a == "--image").map(|i| {
        let f = args.get(i + 1).cloned().unwrap_or_default();
        let w: u32 = args.get(i + 2).and_then(|v| v.parse().ok()).unwrap_or(0);
        let h: u32 = args.get(i + 3).and_then(|v| v.parse().ok()).unwrap_or(0);
        (f, w, h)
    });
    let (texture, w, h) = if let Some((file, w, h)) = image {
        let bytes = std::fs::read(&file)?;
        if bytes.len() != (w * h * 4) as usize {
            return Err(format!("{file} isn't {w}x{h} BGRA").into());
        }
        let t = make_texture(&gpu.device, w, h)?;
        unsafe {
            gpu.device.GetImmediateContext()?.UpdateSubresource(
                &t,
                0,
                None,
                bytes.as_ptr() as *const c_void,
                w * 4,
                0,
            );
        }
        (t, w, h)
    } else {
        let mut cap = super::capture::Capture::start(&gpu, &mon, false)?;
        let mut frame = None;
        for _ in 0..100 {
            cap.wait(20);
            if let Some(f) = cap.next_frame()? {
                frame = Some(f);
                break;
            }
        }
        let frame = frame.ok_or("no frame captured")?;
        let (w, h) = frame.size;
        let t = make_texture(&gpu.device, w, h)?;
        unsafe {
            gpu.device
                .GetImmediateContext()?
                .CopyResource(&t, &frame.texture)
        };
        (t, w, h)
    };
    let mut enc = Nvenc::new(
        &gpu.device,
        &Config {
            width: w,
            height: h,
            fps: mon.refresh_hz.max(1),
            bitrate: 80_000_000,
            chroma444,
            keep_last: true,
        },
    )?;
    let mut out = Vec::new();
    let step = |label: &str, r: Res<()>, got: &mut Vec<(usize, bool)>| -> Res<()> {
        r?;
        let (n, key) = got
            .drain(..)
            .fold((0, false), |(n, k), (l, key)| (n + l, k || key));
        println!(
            "  {label:<34} {:>8.1} KB  {}",
            n as f64 / 1024.0,
            if key { "keyframe" } else { "" }
        );
        Ok(())
    };
    let mut got: Vec<(usize, bool)> = Vec::new();
    println!(
        "{w}x{h} @ {} Hz, {}",
        mon.refresh_hz,
        if chroma444 { "4:4:4" } else { "4:2:0" }
    );
    let r = enc.encode(&texture, 0, true, |d, k, _| {
        got.push((d.len(), k));
        out.extend_from_slice(d);
    });
    step("first (keyframe)", r, &mut got)?;
    for i in 1..=2 {
        let r = enc.encode_last(i, false, |d, k, _| {
            got.push((d.len(), k));
            out.extend_from_slice(d);
        });
        step("same picture again", r, &mut got)?;
    }
    enc.set_bitrate(20_000_000);
    let r = enc.encode_last(3, false, |d, k, _| {
        got.push((d.len(), k));
        out.extend_from_slice(d);
    });
    step("after a bitrate change (no keyframe)", r, &mut got)?;
    let r = enc.encode_last_quality(4, still_qp(0), |d, k, _| {
        got.push((d.len(), k));
        out.extend_from_slice(d);
    });
    step("still picture, quality keyframe", r, &mut got)?;
    let r = enc.encode_last(5, false, |d, k, _| {
        got.push((d.len(), k));
        out.extend_from_slice(d);
    });
    step("next frame (no keyframe)", r, &mut got)?;
    if let Some(p) = dump {
        std::fs::write(&p, &out)?;
        println!("written to {p}");
    }
    Ok(())
}

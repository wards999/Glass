//! Shows decoded pictures: a flip-model swap chain (waitable, one frame of
//! latency) and the GPU's video processor, which converts YCbCr to RGB and
//! scales into the letterboxed picture area in one pass.

use super::decode::{Color, Picture};
use super::decode444::Planar;
use super::Res;
use windows::core::Interface;
use windows::Win32::Foundation::{HANDLE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC,
    D3D11_COMPARISON_NEVER, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FILTER_MIN_MAG_MIP_POINT,
    D3D11_SAMPLER_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain2, DXGI_FRAME_STATISTICS, DXGI_MWA_NO_ALT_ENTER,
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Threading::WaitForSingleObjectEx;

/// The largest rectangle with the source's aspect ratio, centred in the
/// window (black bars elsewhere). Also used to map the pointer.
pub fn fit(win: (u32, u32), src: (u32, u32)) -> RECT {
    let (ww, wh) = (win.0.max(1) as u64, win.1.max(1) as u64);
    let (sw, sh) = (src.0.max(1) as u64, src.1.max(1) as u64);
    let (w, h) = if ww * sh <= wh * sw {
        (ww, (ww * sh / sw).max(1))
    } else {
        ((wh * sw / sh).max(1), wh)
    };
    let left = ((ww - w) / 2) as i32;
    let top = ((wh - h) / 2) as i32;
    RECT {
        left,
        top,
        right: left + w as i32,
        bottom: top + h as i32,
    }
}

struct Processor {
    enumerator: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    key: (u32, u32, u32, u32),
    color: Option<Color>,
}

/// YCbCr 4:4:4 planes -> RGB (shader path for NVDEC pictures).
struct Shader {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    point: ID3D11SamplerState,
    linear: ID3D11SamplerState,
    cb: ID3D11Buffer,
    color: Option<Color>,
}

const SHADER: &str = r#"
Texture2D<float> Y : register(t0);
Texture2D<float> U : register(t1);
Texture2D<float> V : register(t2);
SamplerState S : register(s0);
cbuffer C : register(b0) { float4 m0; float4 m1; float4 m2; float4 off; };
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut vs(uint id : SV_VertexID) {
    VSOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}
float4 ps(VSOut i) : SV_Target {
    float3 yuv = float3(Y.Sample(S, i.uv), U.Sample(S, i.uv), V.Sample(S, i.uv)) - off.xyz;
    return float4(saturate(float3(dot(m0.xyz, yuv), dot(m1.xyz, yuv), dot(m2.xyz, yuv))), 1);
}
"#;

/// Rows of the YCbCr -> RGB matrix and the offsets, as the shader takes them.
fn matrix(c: Color) -> [f32; 16] {
    let (kr, kb) = if c.bt709 {
        (0.2126f32, 0.0722f32)
    } else {
        (0.299, 0.114)
    };
    let kg = 1.0 - kr - kb;
    let (ys, cs, yoff) = if c.full_range {
        (1.0f32, 1.0f32, 0.0f32)
    } else {
        (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0)
    };
    let rv = 2.0 * (1.0 - kr);
    let bu = 2.0 * (1.0 - kb);
    let gu = -bu * kb / kg;
    let gv = -rv * kr / kg;
    let coff = 128.0 / 255.0;
    [
        ys,
        0.0,
        rv * cs,
        0.0, //
        ys,
        gu * cs,
        gv * cs,
        0.0, //
        ys,
        bu * cs,
        0.0,
        0.0, //
        yoff,
        coff,
        coff,
        0.0,
    ]
}

unsafe fn compile(entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Res<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    let r = D3DCompile(
        SHADER.as_ptr() as *const _,
        SHADER.len(),
        windows::core::s!("glass"),
        None,
        None,
        entry,
        target,
        0,
        0,
        &mut code,
        Some(&mut errors),
    );
    if let Err(e) = r {
        let msg = errors
            .map(|b: ID3DBlob| {
                let s = std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                );
                String::from_utf8_lossy(s).into_owned()
            })
            .unwrap_or_default();
        return Err(format!("shader: {e} {msg}").into());
    }
    Ok(code.ok_or("no shader code")?)
}

fn blob(b: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

pub struct Presenter {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    shader: Option<Shader>,
    rtv: Option<ID3D11RenderTargetView>,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    swap: IDXGISwapChain2,
    waitable: HANDLE,
    size: (u32, u32),
    proc: Option<Processor>,
    out_view: Option<ID3D11VideoProcessorOutputView>,
    /// Input views for FFmpeg's texture array slices: (texture, slice, view).
    in_views: Vec<(usize, u32, ID3D11VideoProcessorInputView)>,
}

const FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;

fn color_space(c: Color) -> DXGI_COLOR_SPACE_TYPE {
    match (c.bt709, c.full_range) {
        (true, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
        (true, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
        (false, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
        (false, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
    }
}

impl Presenter {
    pub fn new(device: &ID3D11Device, hwnd: HWND, size: (u32, u32)) -> Res<Self> {
        unsafe {
            let factory: IDXGIFactory2 = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0.max(1),
                Height: size.1.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: FLAGS.0 as u32,
                ..Default::default()
            };
            let swap = factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None)?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            let swap: IDXGISwapChain2 = swap.cast()?;
            // One frame queued at most: Present never piles up latency.
            swap.SetMaximumFrameLatency(1)?;
            let waitable = swap.GetFrameLatencyWaitableObject();
            Ok(Self {
                device: device.clone(),
                ctx: device.GetImmediateContext()?,
                shader: None,
                rtv: None,
                vdev: device.cast()?,
                vctx: device.GetImmediateContext()?.cast()?,
                swap,
                waitable,
                size: (desc.Width, desc.Height),
                proc: None,
                out_view: None,
                in_views: Vec::new(),
            })
        }
    }

    pub fn resize(&mut self, size: (u32, u32)) -> Res<()> {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.size {
            return Ok(());
        }
        self.out_view = None;
        self.rtv = None;
        unsafe {
            self.swap
                .ResizeBuffers(0, size.0, size.1, DXGI_FORMAT_UNKNOWN, FLAGS)?
        };
        self.size = size;
        Ok(())
    }

    /// Waits (up to `ms`) until the swap chain can take another frame
    /// without blocking. Returns how long it waited, µs.
    pub fn wait_ready(&self, ms: u32) -> u64 {
        let t0 = glass_core::clock::now_us();
        unsafe { WaitForSingleObjectEx(self.waitable, ms, true) };
        glass_core::clock::now_us() - t0
    }

    fn processor(&mut self, src: (u32, u32), color: Color) -> Res<()> {
        let key = (src.0, src.1, self.size.0, self.size.1);
        if self.proc.as_ref().is_none_or(|p| p.key != key) {
            let rate = DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: src.0,
                InputHeight: src.1,
                OutputFrameRate: rate,
                OutputWidth: self.size.0,
                OutputHeight: self.size.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            unsafe {
                let enumerator = self.vdev.CreateVideoProcessorEnumerator(&desc)?;
                let vp = self.vdev.CreateVideoProcessor(&enumerator, 0)?;
                // Turn off anything that would touch the picture beyond scaling.
                self.vctx
                    .VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
                self.vctx.VideoProcessorSetStreamFrameFormat(
                    &vp,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
                self.proc = Some(Processor {
                    enumerator,
                    vp,
                    key,
                    color: None,
                });
            }
            self.in_views.clear();
            self.out_view = None;
        }
        let p = self.proc.as_mut().unwrap();
        if p.color != Some(color) {
            unsafe {
                if let Ok(v1) = self.vctx.cast::<ID3D11VideoContext1>() {
                    v1.VideoProcessorSetStreamColorSpace1(&p.vp, 0, color_space(color));
                    v1.VideoProcessorSetOutputColorSpace1(
                        &p.vp,
                        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    );
                } else {
                    // Older interface: 0 = BT.601, 1 = BT.709 in bit 1; studio range.
                    let cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                        _bitfield: (color.bt709 as u32) << 1,
                    };
                    self.vctx.VideoProcessorSetStreamColorSpace(&p.vp, 0, &cs);
                }
            }
            p.color = Some(color);
        }
        Ok(())
    }

    fn input_view(
        &mut self,
        tex: &ID3D11Texture2D,
        slice: u32,
    ) -> Res<ID3D11VideoProcessorInputView> {
        let id = tex.as_raw() as usize;
        if let Some((_, _, v)) = self
            .in_views
            .iter()
            .find(|(t, s, _)| *t == id && *s == slice)
        {
            return Ok(v.clone());
        }
        let p = self.proc.as_ref().ok_or("no video processor")?;
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: slice,
                },
            },
        };
        let mut view = None;
        unsafe {
            self.vdev
                .CreateVideoProcessorInputView(tex, &p.enumerator, &desc, Some(&mut view))?
        };
        let view = view.ok_or("no input view")?;
        // FFmpeg's pool is one texture array; a new texture means a new pool.
        if self.in_views.iter().any(|(t, _, _)| *t != id) {
            self.in_views.clear();
        }
        self.in_views.push((id, slice, view.clone()));
        Ok(view)
    }

    /// Draws `pic` letterboxed into the back buffer.
    pub fn draw(&mut self, pic: &Picture) -> Res<()> {
        self.processor((pic.width, pic.height), pic.color)?;
        let input = self.input_view(&pic.texture, pic.index)?;
        let p = self.proc.as_ref().unwrap();
        unsafe {
            if self.out_view.is_none() {
                // Flip model: buffer 0 is always the current back buffer.
                let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
                let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut v = None;
                self.vdev.CreateVideoProcessorOutputView(
                    &back,
                    &p.enumerator,
                    &desc,
                    Some(&mut v),
                )?;
                self.out_view = v;
            }
            let out = self.out_view.as_ref().ok_or("no output view")?;
            let src = RECT {
                left: 0,
                top: 0,
                right: pic.width as i32,
                bottom: pic.height as i32,
            };
            let dst = fit(self.size, (pic.width, pic.height));
            let full = RECT {
                left: 0,
                top: 0,
                right: self.size.0 as i32,
                bottom: self.size.1 as i32,
            };
            self.vctx
                .VideoProcessorSetStreamSourceRect(&p.vp, 0, true, Some(&src));
            self.vctx
                .VideoProcessorSetStreamDestRect(&p.vp, 0, true, Some(&dst));
            self.vctx
                .VideoProcessorSetOutputTargetRect(&p.vp, true, Some(&full));
            let black = windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_COLOR {
                Anonymous: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_COLOR_0 {
                    RGBA: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            self.vctx
                .VideoProcessorSetOutputBackgroundColor(&p.vp, false, &black);
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input)),
                ..Default::default()
            };
            let streams = [stream];
            let r = self.vctx.VideoProcessorBlt(&p.vp, out, 0, &streams);
            let [s] = streams;
            drop(std::mem::ManuallyDrop::into_inner(s.pInputSurface));
            r?;
        }
        Ok(())
    }

    fn shader(&mut self) -> Res<&mut Shader> {
        if self.shader.is_none() {
            unsafe {
                let vsb = compile(windows::core::s!("vs"), windows::core::s!("vs_4_0"))?;
                let psb = compile(windows::core::s!("ps"), windows::core::s!("ps_4_0"))?;
                let mut vs = None;
                self.device
                    .CreateVertexShader(blob(&vsb), None, Some(&mut vs))?;
                let mut ps = None;
                self.device
                    .CreatePixelShader(blob(&psb), None, Some(&mut ps))?;
                let sampler = |filter| -> Res<ID3D11SamplerState> {
                    let d = D3D11_SAMPLER_DESC {
                        Filter: filter,
                        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                        ComparisonFunc: D3D11_COMPARISON_NEVER,
                        MaxLOD: f32::MAX,
                        ..Default::default()
                    };
                    let mut s = None;
                    self.device.CreateSamplerState(&d, Some(&mut s))?;
                    Ok(s.ok_or("no sampler")?)
                };
                let point = sampler(D3D11_FILTER_MIN_MAG_MIP_POINT)?;
                let linear = sampler(D3D11_FILTER_MIN_MAG_MIP_LINEAR)?;
                let bd = D3D11_BUFFER_DESC {
                    ByteWidth: 64,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                };
                let mut cb = None;
                self.device.CreateBuffer(&bd, None, Some(&mut cb))?;
                self.shader = Some(Shader {
                    vs: vs.ok_or("no vertex shader")?,
                    ps: ps.ok_or("no pixel shader")?,
                    point,
                    linear,
                    cb: cb.ok_or("no constant buffer")?,
                    color: None,
                });
            }
        }
        Ok(self.shader.as_mut().unwrap())
    }

    /// Draws a 4:4:4 picture (three planes) letterboxed into the back buffer.
    /// Pixel-exact when the picture fits 1:1, bilinear when scaled.
    pub fn draw_planar(&mut self, p: &Planar) -> Res<()> {
        let size = self.size;
        let ctx = self.ctx.clone();
        unsafe {
            if self.rtv.is_none() {
                let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
                let mut v = None;
                self.device
                    .CreateRenderTargetView(&back, None, Some(&mut v))?;
                self.rtv = v;
            }
            let rtv = self.rtv.clone().ok_or("no render target")?;
            let dst = fit(size, (p.width, p.height));
            let one_to_one = (dst.right - dst.left) as u32 == p.width
                && (dst.bottom - dst.top) as u32 == p.height;
            let sh = self.shader()?;
            if sh.color != Some(p.color) {
                let m = matrix(p.color);
                ctx.UpdateSubresource(&sh.cb, 0, None, m.as_ptr() as *const _, 0, 0);
                sh.color = Some(p.color);
            }
            ctx.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            ctx.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            let vp = D3D11_VIEWPORT {
                TopLeftX: dst.left as f32,
                TopLeftY: dst.top as f32,
                Width: (dst.right - dst.left) as f32,
                Height: (dst.bottom - dst.top) as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&sh.vs, None);
            ctx.PSSetShader(&sh.ps, None);
            let [a, b, c] = p.planes.clone();
            ctx.PSSetShaderResources(0, Some(&[Some(a), Some(b), Some(c)]));
            let s = if one_to_one { &sh.point } else { &sh.linear };
            ctx.PSSetSamplers(0, Some(&[Some(s.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(sh.cb.clone())]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&[None, None, None]));
            ctx.OMSetRenderTargets(None, None);
        }
        Ok(())
    }

    /// Queues the back buffer for the next refresh. Returns its present id.
    pub fn present(&mut self) -> Res<u32> {
        unsafe {
            self.swap.Present(1, DXGI_PRESENT(0)).ok()?;
            Ok(self.swap.GetLastPresentCount()?)
        }
    }

    /// The latest present that reached the screen and when (local µs), if
    /// the system reports it.
    pub fn displayed(&self) -> Option<(u32, u64)> {
        let mut s = DXGI_FRAME_STATISTICS::default();
        unsafe { self.swap.GetFrameStatistics(&mut s) }.ok()?;
        if s.SyncQPCTime == 0 {
            return None;
        }
        Some((s.PresentCount, qpc_to_us(s.SyncQPCTime)))
    }
}

/// QueryPerformanceCounter ticks -> µs (same base as `clock::now_us`).
fn qpc_to_us(t: i64) -> u64 {
    let mut f = 0i64;
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut f);
    }
    (t as i128 * 1_000_000 / f.max(1) as i128) as u64
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn letterbox() {
        // 16:10 Mac picture on a 16:9 monitor: bars at the sides.
        let r = fit((2560, 1440), (2940, 1912));
        assert_eq!((r.top, r.bottom), (0, 1440));
        assert_eq!(r.right - r.left, 2214);
        assert_eq!(r.left, (2560 - 2214) / 2);
        // Same shape: fills.
        let r = fit((1920, 1080), (2560, 1440));
        assert_eq!((r.left, r.top, r.right, r.bottom), (0, 0, 1920, 1080));
        // Wider source: bars top and bottom.
        let r = fit((1000, 1000), (2000, 1000));
        assert_eq!((r.left, r.right, r.bottom - r.top), (0, 1000, 500));
    }
}

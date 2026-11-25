//! The scorecard's test scene as a capture source (ADR 0033): frames from
//! `glass_core::scorecard` instead of the screen. The card goes to the GPU
//! once (twice over, so any offset is one copy); each frame is a GPU copy
//! from its offset plus small updates for the marker and the barcode.

use super::capture::Frame;
use super::Res;
use glass_core::clock;
use glass_core::scorecard::{self, Phase, Scene, BAR_BITS, BAR_CELL, MARKER};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

pub struct SceneSource {
    ctx: ID3D11DeviceContext,
    tall: ID3D11Texture2D,
    out: ID3D11Texture2D,
    scene: Scene,
    fps: u32,
    start_us: u64,
    /// What `out` shows: (offset, marker, barcode).
    drawn: Option<(u32, Option<bool>, Option<u16>)>,
    seen_seq: u32,
    marker: bool,
    bgra: Vec<u8>,
}

fn texture(device: &ID3D11Device, w: u32, h: u32, init: Option<&[u8]>) -> Res<ID3D11Texture2D> {
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
    let data = init.map(|d| D3D11_SUBRESOURCE_DATA {
        pSysMem: d.as_ptr().cast(),
        SysMemPitch: w * 4,
        SysMemSlicePitch: 0,
    });
    let mut t = None;
    unsafe { device.CreateTexture2D(&desc, data.as_ref().map(|d| d as *const _), Some(&mut t))? };
    t.ok_or_else(|| "no texture".into())
}

impl SceneSource {
    pub fn new(device: &ID3D11Device, w: u32, h: u32, fps: u32) -> Res<Self> {
        let scene = Scene::new(w, h);
        let (w, h) = (scene.width, scene.height);
        let tall = texture(device, w, h * 2, Some(&scene.tall_bgra()))?;
        let out = texture(device, w, h, None)?;
        let ctx = unsafe { device.GetImmediateContext()? };
        Ok(Self {
            ctx,
            tall,
            out,
            scene,
            fps: fps.max(1),
            start_us: clock::now_us(),
            drawn: None,
            seen_seq: 0,
            marker: false,
            bgra: Vec::with_capacity((BAR_BITS * BAR_CELL * BAR_CELL * 4) as usize),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.scene.width, self.scene.height)
    }

    /// The viewer's latest input packet (the marker answers each one).
    pub fn note_input(&mut self, seq: u32) {
        if seq != self.seen_seq {
            self.seen_seq = seq;
            let t = clock::now_us().saturating_sub(self.start_us);
            if scorecard::phase_at(t, self.fps) == Phase::Latency {
                self.marker = !self.marker;
            }
        }
    }

    /// Sleeps until the scene next changes, at most `timeout_ms` (1 ms while
    /// it waits for input).
    pub fn wait(&mut self, timeout_ms: u32) {
        let t = clock::now_us().saturating_sub(self.start_us);
        let until = match scorecard::phase_at(t, self.fps) {
            Phase::Latency => 1_000,
            _ => scorecard::next_change_us(t, self.fps)
                .map_or(timeout_ms as u64 * 1000, |n| n.saturating_sub(t)),
        };
        let us = until.min(timeout_ms as u64 * 1000);
        if us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(us));
        }
    }

    pub fn next_frame(&mut self) -> Res<Option<Frame>> {
        let now = clock::now_us();
        let t = now.saturating_sub(self.start_us);
        let Some(want) = self.scene.content(t, self.fps, self.marker) else {
            return Ok(None);
        };
        if self.drawn == Some(want) {
            return Ok(None);
        }
        let (w, h) = (self.scene.width, self.scene.height);
        unsafe {
            let from = D3D11_BOX {
                left: 0,
                top: want.0,
                front: 0,
                right: w,
                bottom: want.0 + h,
                back: 1,
            };
            self.ctx
                .CopySubresourceRegion(&self.out, 0, 0, 0, 0, &self.tall, 0, Some(&from));
            if let Some(odd) = want.1 {
                let v = Scene::marker_gray(odd);
                self.bgra.clear();
                for _ in 0..MARKER * MARKER {
                    self.bgra.extend_from_slice(&[v, v, v, 255]);
                }
                self.put(MARKER, MARKER);
            }
            if let Some(age) = want.2 {
                self.bgra.clear();
                for v in Scene::barcode_gray(age) {
                    self.bgra.extend_from_slice(&[v, v, v, 255]);
                }
                self.put(BAR_BITS * BAR_CELL, BAR_CELL);
            }
        }
        self.drawn = Some(want);
        Ok(Some(Frame::from_texture(self.out.clone(), now, (w, h))))
    }

    /// `bgra` into the top-left `w` x `h` of the frame.
    unsafe fn put(&self, w: u32, h: u32) {
        let to = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: w.min(self.scene.width),
            bottom: h.min(self.scene.height),
            back: 1,
        };
        self.ctx
            .UpdateSubresource(&self.out, 0, Some(&to), self.bgra.as_ptr().cast(), w * 4, 0);
    }
}

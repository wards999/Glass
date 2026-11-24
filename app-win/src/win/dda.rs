//! Monitor capture via DXGI Desktop Duplication (M3f), for the agent the
//! Glass service runs. Unlike Windows.Graphics.Capture it can show the lock
//! screen, the sign-in screen and UAC prompts: run as SYSTEM, it follows the
//! input desktop (`desktop::follow`) and starts again when Windows switches
//! (ACCESS_LOST). Each frame is copied into one texture of ours on the GPU
//! (no CPU copies, no allocation per frame) so the duplication frame can be
//! released at once.

use super::{d3d::Gpu, desktop, Res};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::HMONITOR;

pub struct Dda {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    hmon: HMONITOR,
    dup: Option<IDXGIOutputDuplication>,
    copy: Option<ID3D11Texture2D>,
    size: (u32, u32),
    /// Timeout for the next acquire (set by `wait`, then 0 for the rest).
    next_timeout_ms: u32,
    /// Times Windows switched desktops (lock screen, UAC) during the session.
    pub switches: u64,
}

/// The outputs (screens) of `device`'s GPU as they are now. Not through the
/// device's own adapter object: DXGI keeps that adapter's screens as they
/// were when its factory was made, so a screen added since (the virtual
/// display, switched on after the service's helper started) is missing
/// there (QUIETPC22: the virtual display was on the RTX 4070, but a
/// helper-age device said it wasn't). A new factory, the same GPU by LUID.
fn outputs_now(device: &ID3D11Device) -> Res<Vec<IDXGIOutput>> {
    unsafe {
        let luid = device
            .cast::<IDXGIDevice>()?
            .GetAdapter()?
            .GetDesc()?
            .AdapterLuid;
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0;
        while let Ok(a) = factory.EnumAdapters1(i) {
            i += 1;
            let l = a.GetDesc1()?.AdapterLuid;
            if l.LowPart == luid.LowPart && l.HighPart == luid.HighPart {
                let mut outs = Vec::new();
                let mut j = 0;
                while let Ok(o) = a.EnumOutputs(j) {
                    j += 1;
                    outs.push(o);
                }
                return Ok(outs);
            }
        }
        Err("the encoding GPU isn't listed any more".into())
    }
}

/// The screen called `name` (`\.\DISPLAYn`) is an output of `device`'s
/// GPU: Desktop Duplication can capture it there.
pub fn on_gpu(device: &ID3D11Device, name: &str) -> bool {
    outputs_now(device).is_ok_and(|outs| {
        outs.iter().any(|o| unsafe {
            o.GetDesc().is_ok_and(|d| {
                let len = d.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
                String::from_utf16_lossy(&d.DeviceName[..len]).eq_ignore_ascii_case(name)
            })
        })
    })
}

fn duplicate(device: &ID3D11Device, hmon: HMONITOR) -> Res<IDXGIOutputDuplication> {
    unsafe {
        for out in outputs_now(device)? {
            if out.GetDesc()?.Monitor == hmon {
                return Ok(out.cast::<IDXGIOutput1>()?.DuplicateOutput(device)?);
            }
        }
        Err("this monitor isn't on the encoding GPU (Desktop Duplication needs that)".into())
    }
}

impl Dda {
    pub fn start(gpu: &Gpu, hmon: HMONITOR, size: (u32, u32)) -> Res<Self> {
        desktop::follow();
        let device = gpu.device.clone();
        let context = unsafe { device.GetImmediateContext()? };
        let dup = duplicate(&device, hmon)?;
        Ok(Self {
            device,
            context,
            hmon,
            dup: Some(dup),
            copy: None,
            size,
            next_timeout_ms: 0,
            switches: 0,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn wait(&mut self, timeout_ms: u32) {
        self.next_timeout_ms = timeout_ms;
    }

    /// A fresh duplication: its first frame is the whole screen.
    pub fn refresh(&mut self) {
        self.dup = None;
        self.dup = duplicate(&self.device, self.hmon).ok();
    }

    /// Starts duplicating again on the current input desktop (after Windows
    /// switched to or from the lock screen / a UAC prompt).
    fn restart(&mut self) {
        self.dup = None;
        if desktop::follow() {
            self.switches += 1;
            desktop::bump();
        }
        self.dup = duplicate(&self.device, self.hmon).ok();
    }

    /// The next changed frame, if any (pointer-only updates are skipped:
    /// the pointer travels separately).
    pub fn next_frame(&mut self) -> Res<Option<(ID3D11Texture2D, (u32, u32))>> {
        let timeout = std::mem::take(&mut self.next_timeout_ms);
        if self.dup.is_none() {
            self.restart();
            if self.dup.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(timeout.max(5) as u64));
                return Ok(None);
            }
        }
        let dup = self.dup.as_ref().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(timeout, &mut info, &mut res) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.restart();
                return Ok(None);
            }
            Err(_) => {
                self.restart();
                return Ok(None);
            }
        }
        let out = (|| -> Res<Option<(ID3D11Texture2D, (u32, u32))>> {
            if info.LastPresentTime == 0 {
                return Ok(None); // only the pointer moved
            }
            let Some(res) = res else { return Ok(None) };
            let tex: ID3D11Texture2D = res.cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut desc) };
            let size = (desc.Width, desc.Height);
            let fits = self.copy.as_ref().is_some_and(|c| {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                unsafe { c.GetDesc(&mut d) };
                (d.Width, d.Height, d.Format) == (desc.Width, desc.Height, desc.Format)
            });
            if !fits {
                // First frame, or the size changed: a texture to copy into.
                let mine = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                    MipLevels: 1,
                    ArraySize: 1,
                    ..desc
                };
                let mut t = None;
                unsafe { self.device.CreateTexture2D(&mine, None, Some(&mut t))? };
                self.copy = t;
            }
            let copy = self.copy.clone().ok_or("no capture texture")?;
            unsafe { self.context.CopyResource(&copy, &tex) };
            Ok(Some((copy, size)))
        })();
        unsafe {
            let _ = dup.ReleaseFrame();
        }
        out
    }
}

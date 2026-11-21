//! Monitor capture. Normally Windows.Graphics.Capture; the agent the Glass
//! service runs uses Desktop Duplication (`dda.rs`) instead, which can show
//! the lock screen and UAC prompts (M3f). Frames stay on the GPU as BGRA
//! D3D11 textures; only changed frames are delivered.

use super::{d3d::Gpu, Res};
use windows::core::Interface;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HANDLE, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW, ENUM_CURRENT_SETTINGS,
    HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows_core::{BOOL, PCWSTR};

pub struct Monitor {
    pub name: String,
    /// Top-left in virtual-desktop pixels.
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub primary: bool,
    pub(super) hmon: HMONITOR,
}

pub fn monitors() -> Res<Vec<Monitor>> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let v = &mut *(data.0 as *mut Vec<HMONITOR>);
        v.push(h);
        true.into()
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(cb),
            LPARAM(&mut handles as *mut _ as isize),
        );
    }
    let mut out = Vec::new();
    for hmon in handles {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        unsafe {
            if !GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
                continue;
            }
        }
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let refresh_hz = unsafe {
            if EnumDisplaySettingsW(
                PCWSTR(info.szDevice.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut dm,
            )
            .as_bool()
            {
                dm.dmDisplayFrequency
            } else {
                60
            }
        };
        let r = info.monitorInfo.rcMonitor;
        let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(32);
        out.push(Monitor {
            name: String::from_utf16_lossy(&info.szDevice[..len]),
            left: r.left,
            top: r.top,
            width: (r.right - r.left) as u32,
            height: (r.bottom - r.top) as u32,
            refresh_hz,
            primary: info.monitorInfo.dwFlags & 1 != 0,
            hmon,
        });
    }
    // Primary first so `--monitor 0` is the main display.
    out.sort_by_key(|m| !m.primary);
    Ok(out)
}

pub struct Frame {
    pub texture: ID3D11Texture2D,
    /// Compositor timestamp (SystemRelativeTime), µs on the QPC clock.
    pub ts_us: u64,
    pub size: (u32, u32),
    _frame: Option<windows::Graphics::Capture::Direct3D11CaptureFrame>,
}

/// Either capture method, behind one interface.
pub enum Capture {
    Wgc(Wgc),
    Dda(super::dda::Dda),
}

impl Capture {
    /// `desktops`: Desktop Duplication, following the lock screen and UAC
    /// prompts (the service's agent); otherwise Windows.Graphics.Capture.
    pub fn start(gpu: &Gpu, mon: &Monitor, desktops: bool) -> Res<Self> {
        Ok(if desktops {
            Capture::Dda(super::dda::Dda::start(
                gpu,
                mon.hmon,
                (mon.width, mon.height),
            )?)
        } else {
            Capture::Wgc(Wgc::start(gpu, mon)?)
        })
    }

    pub fn size(&self) -> (u32, u32) {
        match self {
            Capture::Wgc(c) => c.size(),
            Capture::Dda(c) => c.size(),
        }
    }

    pub fn wait(&mut self, timeout_ms: u32) {
        match self {
            Capture::Wgc(c) => c.wait(timeout_ms),
            Capture::Dda(c) => c.wait(timeout_ms),
        }
    }

    pub fn next_frame(&mut self) -> Res<Option<Frame>> {
        match self {
            Capture::Wgc(c) => c.next_frame(),
            Capture::Dda(c) => Ok(c.next_frame()?.map(|(texture, size)| Frame {
                texture,
                ts_us: glass_core::clock::now_us(),
                size,
                _frame: None,
            })),
        }
    }

    /// Desktop switches seen (lock screen, UAC), for the report.
    pub fn desktop_switches(&self) -> u64 {
        match self {
            Capture::Wgc(_) => 0,
            Capture::Dda(c) => c.switches,
        }
    }
}

pub struct Wgc {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    event: HANDLE,
    size: (u32, u32),
}

impl Wgc {
    pub fn start(gpu: &Gpu, mon: &Monitor) -> Res<Self> {
        let item: GraphicsCaptureItem = unsafe {
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?
                .CreateForMonitor(mon.hmon)?
        };
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        let device: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let sz = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            sz,
        )?;
        let event = unsafe { CreateEventW(None, false, false, None)? };
        let raw = event.0 as usize; // HANDLE isn't Send; the event outlives the pool.
        pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            unsafe {
                let _ = SetEvent(HANDLE(raw as *mut _));
            }
            Ok(())
        }))?;
        let session = pool.CreateCaptureSession(&item)?;
        // LATER(M3): cursor is drawn locally on the client from separate shape/position data.
        session.SetIsCursorCaptureEnabled(false)?;
        // Best effort: not available on every Windows build.
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000 }); // 1 ms
        session.StartCapture()?;
        Ok(Self {
            pool,
            session,
            event,
            size: (sz.Width as u32, sz.Height as u32),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn wait(&self, timeout_ms: u32) {
        unsafe {
            WaitForSingleObject(self.event, timeout_ms);
        }
    }

    pub fn next_frame(&mut self) -> Res<Option<Frame>> {
        let Ok(frame) = self.pool.TryGetNextFrame() else {
            return Ok(None);
        };
        let ts_us = (frame.SystemRelativeTime()?.Duration / 10) as u64;
        let cs = frame.ContentSize()?;
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
        Ok(Some(Frame {
            texture,
            ts_us,
            size: (cs.Width as u32, cs.Height as u32),
            _frame: Some(frame),
        }))
    }
}

impl Drop for Wgc {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

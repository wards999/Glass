//! D3D11 device on the NVIDIA adapter (NVENC needs it), shared by capture and encode.

use super::Res;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

pub const VENDOR_NVIDIA: u32 = 0x10de;

/// Per-monitor DPI awareness (physical pixels) and a multithreaded WinRT apartment.
pub fn init_process() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

pub struct AdapterInfo {
    pub name: String,
    pub vendor_id: u32,
    adapter: IDXGIAdapter1,
}

pub fn adapters() -> Res<Vec<AdapterInfo>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut out = Vec::new();
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        let desc = unsafe { adapter.GetDesc1()? };
        let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
        out.push(AdapterInfo {
            name: String::from_utf16_lossy(&desc.Description[..len]),
            vendor_id: desc.VendorId,
            adapter,
        });
    }
    Ok(out)
}

pub struct Gpu {
    pub name: String,
    pub device: ID3D11Device,
}

impl Gpu {
    pub fn new_nvidia() -> Res<Self> {
        let a = adapters()?
            .into_iter()
            .find(|a| a.vendor_id == VENDOR_NVIDIA)
            .ok_or("no NVIDIA GPU found (NVENC required for M1)")?;
        // LATER(M8): AMF/QSV adapters.
        let mut device = None;
        unsafe {
            D3D11CreateDevice(
                &a.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
        }
        let device: ID3D11Device = device.ok_or("D3D11CreateDevice returned no device")?;
        // Windows.Graphics.Capture touches the device from its own thread.
        unsafe {
            let _ = device
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);
        }
        Ok(Self {
            name: a.name,
            device,
        })
    }
}

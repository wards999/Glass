//! The few CUDA driver calls the 4:4:4 decode path needs (nvcuda.dll,
//! loaded at run time, so PCs without an NVIDIA driver still run Glass):
//! find the CUDA device behind our D3D11 adapter and copy decoded planes
//! into D3D11 textures on the GPU.

use std::ffi::c_void;
use std::sync::OnceLock;
use windows::core::{s, w};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

pub type CuResult = i32;
pub const CUDA_SUCCESS: CuResult = 0;
pub const CU_MEMORYTYPE_DEVICE: u32 = 2;
pub const CU_MEMORYTYPE_ARRAY: u32 = 3;

/// `CUDA_MEMCPY2D` (v2).
#[repr(C)]
#[derive(Default)]
pub struct Memcpy2D {
    pub src_x_in_bytes: usize,
    pub src_y: usize,
    pub src_memory_type: u32,
    pub src_host: usize,
    pub src_device: u64,
    pub src_array: usize,
    pub src_pitch: usize,
    pub dst_x_in_bytes: usize,
    pub dst_y: usize,
    pub dst_memory_type: u32,
    pub dst_host: usize,
    pub dst_device: u64,
    pub dst_array: usize,
    pub dst_pitch: usize,
    pub width_in_bytes: usize,
    pub height: usize,
}

#[allow(non_snake_case)]
pub struct Cuda {
    pub cuInit: unsafe extern "system" fn(u32) -> CuResult,
    pub cuD3D11GetDevice: unsafe extern "system" fn(*mut i32, *mut c_void) -> CuResult,
    pub cuCtxPushCurrent: unsafe extern "system" fn(*mut c_void) -> CuResult,
    pub cuCtxPopCurrent: unsafe extern "system" fn(*mut *mut c_void) -> CuResult,
    pub cuGraphicsD3D11RegisterResource:
        unsafe extern "system" fn(*mut *mut c_void, *mut c_void, u32) -> CuResult,
    pub cuGraphicsUnregisterResource: unsafe extern "system" fn(*mut c_void) -> CuResult,
    pub cuGraphicsMapResources:
        unsafe extern "system" fn(u32, *mut *mut c_void, *mut c_void) -> CuResult,
    pub cuGraphicsUnmapResources:
        unsafe extern "system" fn(u32, *mut *mut c_void, *mut c_void) -> CuResult,
    pub cuGraphicsSubResourceGetMappedArray:
        unsafe extern "system" fn(*mut *mut c_void, *mut c_void, u32, u32) -> CuResult,
    pub cuMemcpy2D: unsafe extern "system" fn(*const Memcpy2D) -> CuResult,
}

unsafe impl Send for Cuda {}
unsafe impl Sync for Cuda {}

/// The driver's CUDA entry points, or None (no NVIDIA driver).
#[allow(clippy::missing_transmute_annotations)] // each target type is the field's
pub fn cuda() -> Option<&'static Cuda> {
    static CUDA: OnceLock<Option<Cuda>> = OnceLock::new();
    CUDA.get_or_init(|| unsafe {
        let lib = LoadLibraryW(w!("nvcuda.dll")).ok()?;
        macro_rules! f {
            ($name:literal) => {
                std::mem::transmute(GetProcAddress(lib, s!($name))?)
            };
        }
        let c = Cuda {
            cuInit: f!("cuInit"),
            cuD3D11GetDevice: f!("cuD3D11GetDevice"),
            cuCtxPushCurrent: f!("cuCtxPushCurrent_v2"),
            cuCtxPopCurrent: f!("cuCtxPopCurrent_v2"),
            cuGraphicsD3D11RegisterResource: f!("cuGraphicsD3D11RegisterResource"),
            cuGraphicsUnregisterResource: f!("cuGraphicsUnregisterResource"),
            cuGraphicsMapResources: f!("cuGraphicsMapResources"),
            cuGraphicsUnmapResources: f!("cuGraphicsUnmapResources"),
            cuGraphicsSubResourceGetMappedArray: f!("cuGraphicsSubResourceGetMappedArray"),
            cuMemcpy2D: f!("cuMemcpy2D_v2"),
        };
        ((c.cuInit)(0) == CUDA_SUCCESS).then_some(c)
    })
    .as_ref()
}

pub fn check(r: CuResult, what: &str) -> Result<(), Box<dyn std::error::Error>> {
    if r == CUDA_SUCCESS {
        Ok(())
    } else {
        Err(format!("{what}: CUDA error {r}").into())
    }
}

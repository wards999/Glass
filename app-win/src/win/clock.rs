//! QPC-based clock in microseconds, the same time base as
//! `Direct3D11CaptureFrame::SystemRelativeTime` (100 ns QPC units).

use std::sync::OnceLock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

fn freq() -> i64 {
    static F: OnceLock<i64> = OnceLock::new();
    *F.get_or_init(|| {
        let mut f = 0;
        unsafe { QueryPerformanceFrequency(&mut f).expect("QPF") };
        f
    })
}

pub fn now_us() -> u64 {
    let mut c = 0;
    unsafe { QueryPerformanceCounter(&mut c).expect("QPC") };
    (c as i128 * 1_000_000 / freq() as i128) as u64
}

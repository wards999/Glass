//! C ABI for the Swift app. Header: `core/include/glass_core.h` (cbindgen).
//! Everything here is allocation-free except `glass_stats_new`/`_free`.

use crate::annexb;
use crate::recording::{self, FrameMeta, RecordingHeader};
use crate::stats::{StatRing, Summary};
use std::ffi::c_char;

static VERSION_C: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// NUL-terminated core version string, e.g. "0.0.1". Static; do not free.
#[no_mangle]
pub extern "C" fn glass_core_version() -> *const c_char {
    VERSION_C.as_ptr().cast()
}

/// Wire protocol version (see docs/protocol.md).
#[no_mangle]
pub extern "C" fn glass_protocol_version() -> u8 {
    crate::protocol::PROTOCOL_VERSION
}

/// A borrowed byte range. `len == 0` means absent/empty.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Bytes {
    pub ptr: *const u8,
    pub len: usize,
}

impl Bytes {
    const EMPTY: Self = Self {
        ptr: std::ptr::null(),
        len: 0,
    };

    fn from(s: Option<&[u8]>) -> Self {
        s.map_or(Self::EMPTY, |s| Self {
            ptr: s.as_ptr(),
            len: s.len(),
        })
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HevcParamSets {
    pub vps: Bytes,
    pub sps: Bytes,
    pub pps: Bytes,
}

unsafe fn slice<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(ptr, len)
    }
}

/// Parses the `.glsr` file header. Returns 0 on success or a `RecordingError` code.
///
/// # Safety
/// `buf` must point to `len` readable bytes; `out` must be valid for writes.
#[no_mangle]
pub unsafe extern "C" fn glass_recording_header(
    buf: *const u8,
    len: usize,
    out: *mut RecordingHeader,
) -> i32 {
    match RecordingHeader::decode(slice(buf, len)) {
        Ok(h) => {
            *out = h;
            0
        }
        Err(e) => e as i32,
    }
}

/// Reads the frame at `*offset` (start with `*offset = 0`), advances `*offset`,
/// and points `data` into `buf`. Returns 0 on success, -1 at end of file, or a
/// `RecordingError` code.
///
/// # Safety
/// `buf` must point to `len` readable bytes that outlive `data`; the out
/// pointers must be valid for reads/writes.
#[no_mangle]
pub unsafe extern "C" fn glass_recording_next_frame(
    buf: *const u8,
    len: usize,
    offset: *mut usize,
    meta: *mut FrameMeta,
    data: *mut Bytes,
) -> i32 {
    let all = slice(buf, len);
    let off = (*offset).max(RecordingHeader::SIZE);
    if off > all.len() {
        return recording::RecordingError::Truncated as i32;
    }
    let mut rest = &all[off..];
    match recording::next_frame(&mut rest) {
        None => -1,
        Some(Err(e)) => e as i32,
        Some(Ok((m, d))) => {
            *meta = m;
            *data = Bytes::from(Some(d));
            *offset = all.len() - rest.len();
            0
        }
    }
}

/// Finds VPS/SPS/PPS in an Annex-B access unit. Returns true if all three exist.
///
/// # Safety
/// `buf` must point to `len` readable bytes; `out` must be valid for writes.
#[no_mangle]
pub unsafe extern "C" fn glass_hevc_param_sets(
    buf: *const u8,
    len: usize,
    out: *mut HevcParamSets,
) -> bool {
    let ps = annexb::param_sets(slice(buf, len));
    *out = HevcParamSets {
        vps: Bytes::from(ps.vps),
        sps: Bytes::from(ps.sps),
        pps: Bytes::from(ps.pps),
    };
    ps.complete()
}

/// Output size needed by `glass_hevc_to_length_prefixed`.
///
/// # Safety
/// `buf` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_hevc_length_prefixed_len(buf: *const u8, len: usize) -> usize {
    annexb::length_prefixed_len(slice(buf, len))
}

/// Converts an Annex-B access unit to 4-byte length-prefixed NALs (parameter
/// sets and AUDs dropped). Returns bytes written, or -1 if `out_cap` is too small.
///
/// # Safety
/// `buf` must point to `len` readable bytes and `out` to `out_cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_hevc_to_length_prefixed(
    buf: *const u8,
    len: usize,
    out: *mut u8,
    out_cap: usize,
) -> isize {
    let out = if out.is_null() || out_cap == 0 {
        &mut [][..]
    } else {
        std::slice::from_raw_parts_mut(out, out_cap)
    };
    annexb::to_length_prefixed(slice(buf, len), out).map_or(-1, |n| n as isize)
}

/// Allocates a stats ring. Free with `glass_stats_free`.
#[no_mangle]
pub extern "C" fn glass_stats_new() -> *mut StatRing {
    Box::into_raw(Box::default())
}

/// # Safety
/// `ring` must come from `glass_stats_new` and not be used concurrently.
#[no_mangle]
pub unsafe extern "C" fn glass_stats_push(ring: *mut StatRing, us: u32) {
    (*ring).push(us);
}

/// # Safety
/// `ring` must come from `glass_stats_new`.
#[no_mangle]
pub unsafe extern "C" fn glass_stats_summary(ring: *const StatRing) -> Summary {
    (*ring).summary()
}

/// # Safety
/// `ring` must come from `glass_stats_new` (or be null) and not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glass_stats_free(ring: *mut StatRing) {
    if !ring.is_null() {
        drop(Box::from_raw(ring));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{RecordingWriter, CHROMA_420, CODEC_HEVC};
    use std::ffi::CStr;

    #[test]
    fn version_over_ffi_matches_crate() {
        let s = unsafe { CStr::from_ptr(glass_core_version()) };
        assert_eq!(s.to_str().unwrap(), crate::VERSION);
        assert_eq!(glass_protocol_version(), crate::protocol::PROTOCOL_VERSION);
    }

    #[test]
    fn recording_over_ffi() {
        let h = RecordingHeader {
            codec: CODEC_HEVC,
            chroma: CHROMA_420,
            width: 64,
            height: 32,
            fps: 60,
        };
        let mut w = RecordingWriter::new(Vec::new(), &h).unwrap();
        for id in 0..2u32 {
            let data = [0, 0, 1, 0x02, id as u8];
            w.write_frame(
                &FrameMeta {
                    frame_id: id,
                    len: 5,
                    ..Default::default()
                },
                &data,
            )
            .unwrap();
        }
        let buf = w.into_inner();
        unsafe {
            let mut out = std::mem::zeroed();
            assert_eq!(glass_recording_header(buf.as_ptr(), buf.len(), &mut out), 0);
            assert_eq!(out, h);
            let (mut off, mut meta, mut data) = (0usize, FrameMeta::default(), Bytes::EMPTY);
            for id in 0..2u32 {
                assert_eq!(
                    glass_recording_next_frame(
                        buf.as_ptr(),
                        buf.len(),
                        &mut off,
                        &mut meta,
                        &mut data
                    ),
                    0
                );
                assert_eq!(meta.frame_id, id);
                assert_eq!(*data.ptr.add(4), id as u8);
            }
            assert_eq!(
                glass_recording_next_frame(buf.as_ptr(), buf.len(), &mut off, &mut meta, &mut data),
                -1
            );
        }
    }

    #[test]
    fn stats_over_ffi() {
        unsafe {
            let r = glass_stats_new();
            for v in [10, 20, 30] {
                glass_stats_push(r, v);
            }
            let s = glass_stats_summary(r);
            assert_eq!((s.count, s.p50_us, s.max_us), (3, 20, 30));
            glass_stats_free(r);
        }
    }
}

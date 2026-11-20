//! C ABI for the Swift app. Header: `core/include/glass_core.h` (cbindgen).
//! Everything here is allocation-free except the `_new`/`_connect`/`_start`
//! constructors and their `_free`/`_close`/`_stop` counterparts.

use crate::annexb;
use crate::keymap::{self, ModifierRule};
use crate::net::{Client, ClientStats, ConnectError, CursorInfo, LiveFrameMeta, Wait};
use crate::protocol::StreamInfo;
use crate::recording::{self, FrameMeta, RecordingHeader};
use crate::replay::Replay;
use crate::stats::{StatRing, Summary};
use std::ffi::{c_char, CStr};
use std::time::Duration;

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

/// # Safety
/// `ring` must come from `glass_stats_new` and not be used concurrently.
#[no_mangle]
pub unsafe extern "C" fn glass_stats_reset(ring: *mut StatRing) {
    (*ring).reset();
}

/// Media clock in µs. On macOS this equals `CACurrentMediaTime() * 1e6`.
#[no_mangle]
pub extern "C" fn glass_clock_now_us() -> u64 {
    crate::clock::now_us()
}

// --- Live viewer ------------------------------------------------------------

/// Connects to a host. Returns null on failure and sets `*err` to a
/// `GLASS_ConnectError` value. Free with `glass_client_close`.
///
/// # Safety
/// `host` must be a NUL-terminated string; `err` must be valid for writes or null.
#[no_mangle]
pub unsafe extern "C" fn glass_client_connect(
    host: *const c_char,
    port: u16,
    err: *mut i32,
) -> *mut Client {
    let set = |e: ConnectError| {
        if !err.is_null() {
            *err = e as i32;
        }
    };
    if host.is_null() {
        set(ConnectError::Resolve);
        return std::ptr::null_mut();
    }
    let Ok(host) = CStr::from_ptr(host).to_str() else {
        set(ConnectError::Resolve);
        return std::ptr::null_mut();
    };
    match Client::connect(host, port) {
        Ok(c) => {
            if !err.is_null() {
                *err = 0;
            }
            Box::into_raw(Box::new(c))
        }
        Err(e) => {
            set(e);
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// `c` must come from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_info(c: *const Client) -> StreamInfo {
    (*c).info()
}

/// Waits up to `timeout_ms` for the next frame. Returns 1 with `meta`/`data`
/// filled (data valid until `glass_client_release_frame`), 0 on timeout, or
/// -1 once the host has gone and no frames remain.
///
/// # Safety
/// `c` must come from `glass_client_connect`; only one thread may wait/release.
/// `meta` and `data` must be valid for writes.
#[no_mangle]
pub unsafe extern "C" fn glass_client_wait_frame(
    c: *const Client,
    timeout_ms: u32,
    meta: *mut LiveFrameMeta,
    data: *mut Bytes,
) -> i32 {
    match (*c).wait_frame(Duration::from_millis(timeout_ms as u64)) {
        Wait::Frame(m, d) => {
            *meta = m;
            *data = Bytes::from(Some(d));
            1
        }
        Wait::Timeout => 0,
        Wait::Disconnected => -1,
    }
}

/// # Safety
/// `c` must come from `glass_client_connect`; call from the waiting thread.
#[no_mangle]
pub unsafe extern "C" fn glass_client_release_frame(c: *const Client) {
    (*c).release_frame();
}

/// Asks the host for an IDR (e.g. after a decode error).
///
/// # Safety
/// `c` must come from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_request_keyframe(c: *const Client) {
    (*c).request_keyframe();
}

/// # Safety
/// `c` must come from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_stats(c: *const Client) -> ClientStats {
    (*c).stats()
}

/// Host timestamp -> local clock, or 0 until clock sync has a sample.
///
/// # Safety
/// `c` must come from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_host_to_local_us(c: *const Client, host_us: u64) -> u64 {
    (*c).host_to_local(host_us).unwrap_or(0)
}

// --- Input (any thread) -------------------------------------------------------

/// Mac `kVK_*` key code -> HID usage (0 = not forwarded). `pc_style`: ⌘ -> Win
/// and ⌃ -> Ctrl instead of the default ⌘ -> Ctrl, ⌃ -> Win.
#[no_mangle]
pub extern "C" fn glass_keymap_mac_to_hid(kvk: u16, iso: bool, pc_style: bool) -> u8 {
    let rule = if pc_style {
        ModifierRule::PcStyle
    } else {
        ModifierRule::MacStyle
    };
    keymap::mac_to_hid(kvk, iso, rule)
}

/// # Safety
/// `c` must come from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_input_key(c: *const Client, hid: u8, down: bool) {
    (*c).input_key(hid, down);
}

/// Button 0 left, 1 right, 2 middle, 3 back, 4 forward; position 0..=65535
/// across the video picture.
///
/// # Safety
/// `c` must come from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_input_button(
    c: *const Client,
    button: u8,
    down: bool,
    x: u16,
    y: u16,
) {
    (*c).input_button(button, down, x, y);
}

/// # Safety
/// `c` must come from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_input_move(c: *const Client, x: u16, y: u16) {
    (*c).input_move(x, y);
}

/// Wheel units (120 = one notch); `dy > 0` scrolls up, `dx > 0` right.
///
/// # Safety
/// `c` must come from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_input_scroll(c: *const Client, dy: i16, dx: i16) {
    (*c).input_scroll(dy, dx);
}

/// # Safety
/// `c` must come from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_input_release_all(c: *const Client) {
    (*c).input_release_all();
}

/// Local time (µs) input packet `seq` was sent, or 0 if unknown. Pair with
/// `GLASS_LiveFrameMeta.input_seq` for input -> screen latency.
///
/// # Safety
/// `c` must come from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_input_sent_us(c: *const Client, seq: u32) -> u64 {
    (*c).input_sent_at(seq).unwrap_or(0)
}

/// The host's pointer. If its generation differs from `seen` and `rgba` has
/// room (`cap >= width*height*4`), copies the straight-alpha RGBA image there.
/// Always returns the current info.
///
/// # Safety
/// `c` must come from `glass_client_connect`; `rgba` must point to `cap`
/// writable bytes (or be null with `cap == 0`).
#[no_mangle]
pub unsafe extern "C" fn glass_client_cursor(
    c: *const Client,
    seen: u32,
    rgba: *mut u8,
    cap: usize,
) -> CursorInfo {
    let mut img = Vec::new();
    let info = (*c).cursor(seen, &mut img);
    if info.generation != seen && !rgba.is_null() && img.len() <= cap {
        std::ptr::copy_nonoverlapping(img.as_ptr(), rgba, img.len());
    }
    info
}

/// Says goodbye, stops the threads and frees the client.
///
/// # Safety
/// `c` must come from `glass_client_connect` (or be null) and not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glass_client_close(c: *mut Client) {
    if !c.is_null() {
        drop(Box::from_raw(c));
    }
}

// --- Replay host (tests) ----------------------------------------------------

/// Serves a `.glsr` file (copied) as a live stream on `port` (0 = any free
/// port; the chosen one is written to `*out_port`). Returns null on error.
///
/// # Safety
/// `buf` must point to `len` readable bytes; `out_port` valid for writes or null.
#[no_mangle]
pub unsafe extern "C" fn glass_replay_start(
    buf: *const u8,
    len: usize,
    port: u16,
    out_port: *mut u16,
) -> *mut Replay {
    match Replay::start(slice(buf, len).to_vec(), port) {
        Ok(r) => {
            if !out_port.is_null() {
                *out_port = r.port();
            }
            Box::into_raw(Box::new(r))
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// Input actions the replay host has received (all sessions, capped).
///
/// # Safety
/// `r` must come from `glass_replay_start`.
#[no_mangle]
pub unsafe extern "C" fn glass_replay_input_actions(r: *const Replay) -> u64 {
    (*r).input_log().len() as u64
}

/// # Safety
/// `r` must come from `glass_replay_start` (or be null) and not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glass_replay_stop(r: *mut Replay) {
    if !r.is_null() {
        drop(Box::from_raw(r));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{RecordingWriter, CHROMA_420, CODEC_HEVC};

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
            glass_stats_reset(r);
            assert_eq!(glass_stats_summary(r).count, 0);
            glass_stats_free(r);
        }
    }

    #[test]
    fn client_connect_error_over_ffi() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut err = 0;
        let c = unsafe { glass_client_connect(c"127.0.0.1".as_ptr(), port, &mut err) };
        assert!(c.is_null());
        assert_eq!(err, ConnectError::Connect as i32);
        unsafe { glass_client_close(c) }; // null is fine
        assert!(glass_clock_now_us() > 0);
    }
}

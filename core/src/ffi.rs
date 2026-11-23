//! C ABI for the Swift app. Header: `core/include/glass_core.h` (cbindgen).
//! Everything here is allocation-free except the `_new`/`_connect`/`_start`
//! constructors and their `_free`/`_close`/`_stop` counterparts.

use crate::annexb;
use crate::clipboard::ClipProgress;
use crate::identity::{default_dir, Device};
use crate::input::InputAction;
use crate::keymap::{self, ModifierRule};
use crate::net::{
    pair_begin, Client, ClientStats, ConnectError, CursorInfo, HostCounters, HostEvent,
    HostListener, HostOptions, HostSession, LiveFrameMeta, Pairing, ViewerHello, Wait,
};
use crate::packet::OutFrame;
use crate::protocol::{CursorShape, StreamInfo};
use crate::recording::{self, FrameMeta, RecordingHeader};
use crate::replay::Replay;
use crate::stats::{StatRing, Summary};
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// This machine's identity and paired devices (opaque).
pub struct DeviceHandle(Arc<Device>);

/// Copies `s` into `buf` as a NUL-terminated string (truncated to fit).
/// Returns the full length in bytes (without the NUL).
unsafe fn copy_str(s: &str, buf: *mut c_char, len: usize) -> usize {
    if !buf.is_null() && len > 0 {
        let mut n = s.len().min(len - 1);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        std::ptr::copy_nonoverlapping(s.as_ptr(), buf.cast(), n);
        *buf.add(n) = 0;
    }
    s.len()
}

unsafe fn str_arg<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        CStr::from_ptr(p).to_str().ok()
    }
}

unsafe fn set_err(err: *mut i32, v: i32) {
    if !err.is_null() {
        *err = v;
    }
}

static VERSION_C: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// NUL-terminated build name (`BUILD`: the test-build tag, or "local").
/// Static; do not free.
#[no_mangle]
pub extern "C" fn glass_core_build() -> *const c_char {
    static B: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();
    B.get_or_init(|| std::ffi::CString::new(crate::BUILD).unwrap_or_default())
        .as_ptr()
}

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

// --- Device identity and paired devices -----------------------------------

/// Opens (creating on first run) this machine's identity in `dir`, or the
/// default folder (`GLASS_HOME`, else ~/Library/Application Support/Glass)
/// when `dir` is null. `name` is how other devices will see this one.
/// Returns null on failure. Free with `glass_device_free`.
///
/// # Safety
/// `dir` (or null) and `name` must be NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn glass_device_open(
    dir: *const c_char,
    name: *const c_char,
) -> *mut DeviceHandle {
    let dir = match str_arg(dir) {
        Some(d) => std::path::PathBuf::from(d),
        None => match default_dir() {
            Some(d) => d,
            None => return std::ptr::null_mut(),
        },
    };
    let name = str_arg(name).unwrap_or("Mac");
    match Device::open(&dir, name, None) {
        Ok(d) => Box::into_raw(Box::new(DeviceHandle(d))),
        Err(_) => std::ptr::null_mut(),
    }
}

/// # Safety
/// `d` must come from `glass_device_open` (or be null) and not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glass_device_free(d: *mut DeviceHandle) {
    if !d.is_null() {
        drop(Box::from_raw(d));
    }
}

/// Writes this device's 32-byte certificate fingerprint to `out`.
///
/// # Safety
/// `d` from `glass_device_open`; `out` valid for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_device_fingerprint(d: *const DeviceHandle, out: *mut u8) {
    std::ptr::copy_nonoverlapping((*d).0.fingerprint().as_ptr(), out, 32);
}

/// Number of paired devices to show (re-read from disk; not this device
/// itself).
///
/// # Safety
/// `d` must come from `glass_device_open`.
#[no_mangle]
pub unsafe extern "C" fn glass_device_peer_count(d: *const DeviceHandle) -> u32 {
    (*d).0.listed_peers().len() as u32
}

/// Paired device `index`: writes its fingerprint (32 bytes) to `fp_out` and
/// its name (NUL-terminated, truncated to `name_len`) to `name`. Returns its
/// roles (`GLASS_ROLE_*` bits), or 0 if `index` is out of range.
///
/// # Safety
/// `d` from `glass_device_open`; `fp_out` valid for 32 bytes; `name` valid
/// for `name_len` bytes (or null).
#[no_mangle]
pub unsafe extern "C" fn glass_device_peer(
    d: *const DeviceHandle,
    index: u32,
    fp_out: *mut u8,
    name: *mut c_char,
    name_len: usize,
) -> u8 {
    let peers = (*d).0.listed_peers();
    let Some(p) = peers.get(index as usize) else {
        return 0;
    };
    if !fp_out.is_null() {
        std::ptr::copy_nonoverlapping(p.fingerprint.as_ptr(), fp_out, 32);
    }
    copy_str(&p.name, name, name_len);
    p.roles
}

/// Unpairs the device with this fingerprint. False if it wasn't paired.
///
/// # Safety
/// `d` from `glass_device_open`; `fp` valid for 32 bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_device_forget(d: *const DeviceHandle, fp: *const u8) -> bool {
    let mut f = [0u8; 32];
    std::ptr::copy_nonoverlapping(fp, f.as_mut_ptr(), 32);
    (*d).0.forget(&f).unwrap_or(false)
}

// --- Pairing ------------------------------------------------------------------

/// Asks `host` to pair; on success the host is showing a PIN. Returns null
/// and sets `*err` (`GLASS_ConnectError`) on failure. Finish with
/// `glass_pair_finish` or cancel with `glass_pairing_cancel`.
///
/// # Safety
/// `d` from `glass_device_open`; `host` NUL-terminated; `err` valid or null.
#[no_mangle]
pub unsafe extern "C" fn glass_pair_begin(
    d: *const DeviceHandle,
    host: *const c_char,
    port: u16,
    err: *mut i32,
) -> *mut Pairing {
    let Some(host) = str_arg(host) else {
        set_err(err, ConnectError::Resolve as i32);
        return std::ptr::null_mut();
    };
    match pair_begin(&(*d).0, host, port) {
        Ok(p) => {
            set_err(err, 0);
            Box::into_raw(Box::new(p))
        }
        Err(e) => {
            set_err(err, e as i32);
            std::ptr::null_mut()
        }
    }
}

/// The host's name (NUL-terminated into `buf`); returns its full length.
///
/// # Safety
/// `p` from `glass_pair_begin`; `buf` valid for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_pairing_host_name(
    p: *const Pairing,
    buf: *mut c_char,
    len: usize,
) -> usize {
    copy_str((*p).host_name(), buf, len)
}

/// Runs the PIN exchange and frees `p`. True = paired (both sides stored
/// each other); false sets `*err` (e.g. `GLASS_ConnectError_WrongPin`).
///
/// # Safety
/// `p` from `glass_pair_begin`, not used afterwards; `pin` NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn glass_pair_finish(
    p: *mut Pairing,
    pin: *const c_char,
    err: *mut i32,
) -> bool {
    let p = Box::from_raw(p);
    let pin = str_arg(pin).unwrap_or("");
    match p.finish(pin) {
        Ok(_) => {
            set_err(err, 0);
            true
        }
        Err(e) => {
            set_err(err, e as i32);
            false
        }
    }
}

/// Cancels a pairing (the host stops waiting) and frees `p`.
///
/// # Safety
/// `p` from `glass_pair_begin` (or null), not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glass_pairing_cancel(p: *mut Pairing) {
    if !p.is_null() {
        drop(Box::from_raw(p));
    }
}

// --- Live viewer ------------------------------------------------------------

/// Connects to a paired host. Returns null on failure and sets `*err` to a
/// `GLASS_ConnectError` value (`NotPaired`: pair first). Free with
/// `glass_client_close`.
///
/// # Safety
/// `d` from `glass_device_open`; `host` must be a NUL-terminated string;
/// `err` must be valid for writes or null.
#[no_mangle]
pub unsafe extern "C" fn glass_client_connect(
    d: *const DeviceHandle,
    host: *const c_char,
    port: u16,
    hello: ViewerHello,
    err: *mut i32,
) -> *mut Client {
    let Some(host) = str_arg(host) else {
        set_err(err, ConnectError::Resolve as i32);
        return std::ptr::null_mut();
    };
    match Client::connect_with(&(*d).0, host, port, hello) {
        Ok(c) => {
            set_err(err, 0);
            Box::into_raw(Box::new(c))
        }
        Err(e) => {
            set_err(err, e as i32);
            std::ptr::null_mut()
        }
    }
}

/// What the host is (`HOST_OS_*`).
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_host_os(c: *const Client) -> u8 {
    (*c).host_os()
}

/// The host ended the session on purpose (said Bye), as opposed to the
/// connection dropping (the viewer may reconnect then).
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_host_said_bye(c: *const Client) -> bool {
    (*c).host_said_bye()
}

/// What the host can do (`HOST_CAP_*`).
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_host_caps(c: *const Client) -> u8 {
    (*c).host_caps()
}

/// Counts the host's `StreamChange`s; when it moves, re-read
/// `glass_client_info` (new size, chroma or mode from the next IDR).
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_info_changes(c: *const Client) -> u32 {
    (*c).info_changes()
}

/// Asks the host for view mode `mode` (`MODE_*`); `allow_444` false asks
/// for 4:2:0.
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_set_mode(c: *const Client, mode: u8, allow_444: bool) {
    (*c).set_mode(mode, allow_444)
}

/// The connected host's paired name (NUL-terminated into `buf`).
///
/// # Safety
/// `c` from `glass_client_connect`; `buf` valid for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_client_host_name(
    c: *const Client,
    buf: *mut c_char,
    len: usize,
) -> usize {
    copy_str(&(*c).peer().name, buf, len)
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

// --- Host (Mac host, M3b) ---------------------------------------------------

/// A caller-supplied context pointer handed back to callbacks on core's
/// threads. The caller guarantees it stays valid and is thread-safe.
#[derive(Clone, Copy)]
struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}

/// `HostEventC::kind`.
pub const HOST_EVENT_PAIRING_PIN: u8 = 1;
pub const HOST_EVENT_PAIRED: u8 = 2;
pub const HOST_EVENT_WRONG_PIN: u8 = 3;
pub const HOST_EVENT_PAIRING_LOCKED: u8 = 4;
pub const HOST_EVENT_REFUSED: u8 = 5;
pub const HOST_EVENT_DENIED: u8 = 6;
pub const HOST_EVENT_LOOP: u8 = 7;

/// Something the host's user should see (`net::HostEvent`). Strings are
/// NUL-terminated UTF-8.
#[repr(C)]
pub struct HostEventC {
    pub kind: u8,
    /// PairingPin: true the first time this PIN is shown.
    pub new_pin: bool,
    /// WrongPin: tries left with the current PIN.
    pub left: u32,
    /// Device name (PairingPin, Paired, WrongPin, Denied) or short
    /// fingerprint (Refused).
    pub device: [c_char; 72],
    /// PairingPin: the 6-digit PIN.
    pub pin: [c_char; 8],
    /// PairingPin, Refused: the device's IP address.
    pub addr: [c_char; 48],
}

pub type HostEventFn = unsafe extern "C" fn(ctx: *mut c_void, event: *const HostEventC);

/// `InputActionC::kind`.
pub const INPUT_MOVE: u8 = 1;
pub const INPUT_BUTTON: u8 = 2;
pub const INPUT_KEY: u8 = 3;
pub const INPUT_SCROLL: u8 = 4;

/// One input action to inject (`input::InputAction`), in order.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InputActionC {
    pub kind: u8,
    /// Button: `input::BUTTON_*`.
    pub button: u8,
    /// Key: HID usage (`glass_keymap_hid_to_mac` gives the Mac key code).
    pub hid: u8,
    pub down: bool,
    /// Move: normalised position on the picture, 0..=65535.
    pub x: u16,
    pub y: u16,
    /// Scroll: wheel units (120 = one notch); `dy > 0` up, `dx > 0` right.
    pub dy: i16,
    pub dx: i16,
}

impl From<InputAction> for InputActionC {
    fn from(a: InputAction) -> Self {
        let mut c = InputActionC::default();
        match a {
            InputAction::MoveTo { x, y } => {
                c.kind = INPUT_MOVE;
                c.x = x;
                c.y = y;
            }
            InputAction::Button { button, down } => {
                c.kind = INPUT_BUTTON;
                c.button = button;
                c.down = down;
            }
            InputAction::Key { hid, down } => {
                c.kind = INPUT_KEY;
                c.hid = hid;
                c.down = down;
            }
            InputAction::Scroll { dy, dx } => {
                c.kind = INPUT_SCROLL;
                c.dy = dy;
                c.dx = dx;
            }
        }
        c
    }
}

pub type InputFn = unsafe extern "C" fn(ctx: *mut c_void, action: *const InputActionC);

fn event_c(e: HostEvent) -> HostEventC {
    let mut c = HostEventC {
        kind: 0,
        new_pin: false,
        left: 0,
        device: [0; 72],
        pin: [0; 8],
        addr: [0; 48],
    };
    let (kind, device, pin, addr) = match e {
        HostEvent::PairingPin {
            device,
            pin,
            new,
            addr,
        } => {
            c.new_pin = new;
            (HOST_EVENT_PAIRING_PIN, device, pin, addr.ip().to_string())
        }
        HostEvent::Paired { device } => (HOST_EVENT_PAIRED, device, "".into(), "".into()),
        HostEvent::WrongPin { device, left } => {
            c.left = left;
            (HOST_EVENT_WRONG_PIN, device, "".into(), "".into())
        }
        HostEvent::PairingLocked => (HOST_EVENT_PAIRING_LOCKED, "".into(), "".into(), "".into()),
        HostEvent::Refused { fingerprint, addr } => (
            HOST_EVENT_REFUSED,
            fingerprint,
            "".into(),
            addr.ip().to_string(),
        ),
        HostEvent::Denied { device } => (HOST_EVENT_DENIED, device, "".into(), "".into()),
        HostEvent::Loop { device } => (HOST_EVENT_LOOP, device, "".into(), "".into()),
    };
    c.kind = kind;
    unsafe {
        copy_str(&device, c.device.as_mut_ptr(), c.device.len());
        copy_str(&pin, c.pin.as_mut_ptr(), c.pin.len());
        copy_str(&addr, c.addr.as_mut_ptr(), c.addr.len());
    }
    c
}

/// A listening host (opaque): waits for paired viewers and pairing requests.
pub struct HostHandle {
    listener: Mutex<HostListener>,
    stop: AtomicBool,
    port: u16,
}

/// Starts listening on `port` (TCP and UDP; 0 = any free port) as `d`.
/// `fixed_pin` (null = a random PIN per request) is for tests. `events`
/// (nullable) is called on the accepting thread. Returns null on error
/// (`*err`: 1 = port in use or no network, 10 = identity).
///
/// # Safety
/// `d` from `glass_device_open`; `fixed_pin` null or NUL-terminated; `ctx`
/// valid for as long as the host runs; `err` null or writable.
#[no_mangle]
pub unsafe extern "C" fn glass_host_listen(
    d: *const DeviceHandle,
    port: u16,
    fixed_pin: *const c_char,
    host_os: u8,
    host_caps: u8,
    events: HostEventFn,
    ctx: *mut c_void,
    err: *mut i32,
) -> *mut HostHandle {
    let ctx = Ctx(ctx);
    let opts = HostOptions {
        fixed_pin: str_arg(fixed_pin).map(str::to_string),
        ask: None, // LATER: "ask before each connection" on the Mac.
        host_os,
        host_caps,
        events: Box::new(move |e| {
            let f = events;
            let c = event_c(e);
            let ctx = ctx;
            f(ctx.0, &c);
        }),
    };
    match HostListener::bind(port, (*d).0.clone(), opts) {
        Ok(l) => {
            let port = l.port();
            Box::into_raw(Box::new(HostHandle {
                listener: Mutex::new(l),
                stop: AtomicBool::new(false),
                port,
            }))
        }
        Err(e) => {
            let code = if e.kind() == std::io::ErrorKind::AddrInUse {
                1
            } else if e.kind() == std::io::ErrorKind::InvalidData {
                10
            } else {
                1
            };
            set_err(err, code);
            std::ptr::null_mut()
        }
    }
}

/// The port the host listens on.
///
/// # Safety
/// `h` from `glass_host_listen`.
#[no_mangle]
pub unsafe extern "C" fn glass_host_port(h: *const HostHandle) -> u16 {
    (*h).port
}

/// Waits for one connection and handles it. Returns a session for a paired
/// viewer (`*status` = 1). A pairing request or a refused device is handled
/// here, reported through the events callback, and gives null with
/// `*status` = 0; a failed attempt gives -1; after `glass_host_wake`, -2.
/// `input` is called on core's UDP thread for every action, in order.
///
/// # Safety
/// `h` from `glass_host_listen`; `ctx` valid until the session is closed;
/// `status` writable or null.
#[no_mangle]
pub unsafe extern "C" fn glass_host_accept(
    h: *const HostHandle,
    info: StreamInfo,
    input: InputFn,
    ctx: *mut c_void,
    status: *mut i32,
) -> *mut HostSession {
    let h = &*h;
    if h.stop.load(Ordering::Relaxed) {
        set_err(status, -2);
        return std::ptr::null_mut();
    }
    let ctx = Ctx(ctx);
    let sink = Box::new(move |a: InputAction| {
        let c = InputActionC::from(a);
        let ctx = ctx;
        input(ctx.0, &c);
    });
    let r = h
        .listener
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .accept(info, sink);
    if h.stop.load(Ordering::Relaxed) {
        set_err(status, -2);
        return std::ptr::null_mut(); // a session, if any, is dropped (Bye)
    }
    match r {
        Ok(Some(s)) => {
            set_err(status, 1);
            Box::into_raw(Box::new(s))
        }
        Ok(None) => {
            set_err(status, 0);
            std::ptr::null_mut()
        }
        Err(_) => {
            set_err(status, -1);
            std::ptr::null_mut()
        }
    }
}

/// Makes a blocked `glass_host_accept` return (-2), and every later one.
///
/// # Safety
/// `h` from `glass_host_listen`.
#[no_mangle]
pub unsafe extern "C" fn glass_host_wake(h: *const HostHandle) {
    (*h).stop.store(true, Ordering::Relaxed);
    let _ = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], (*h).port)),
        Duration::from_millis(500),
    );
}

/// Stops listening. Call `glass_host_wake` and let `glass_host_accept`
/// return first.
///
/// # Safety
/// `h` from `glass_host_listen` (or null), not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glass_host_free(h: *mut HostHandle) {
    if !h.is_null() {
        drop(Box::from_raw(h));
    }
}

// A session's calls must not overlap: make them from one thread (or queue).

/// The connected viewer's name. Returns its full length in bytes.
///
/// # Safety
/// `s` from `glass_host_accept`; `buf` writable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_session_peer_name(
    s: *const HostSession,
    buf: *mut c_char,
    len: usize,
) -> usize {
    copy_str(&(*s).peer_name, buf, len)
}

/// The viewer's IP address.
///
/// # Safety
/// As `glass_session_peer_name`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_peer_addr(
    s: *const HostSession,
    buf: *mut c_char,
    len: usize,
) -> usize {
    copy_str(&(*s).peer_tcp.ip().to_string(), buf, len)
}

/// False once the viewer has gone.
///
/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_connected(s: *const HostSession) -> bool {
    (*s).connected()
}

/// True once frames are being delivered (the viewer's UDP address is known).
///
/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_streaming(s: *const HostSession) -> bool {
    (*s).streaming()
}

/// Takes a pending keyframe request: encode the next frame as an IDR.
///
/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_take_keyframe_request(s: *const HostSession) -> bool {
    (*s).take_keyframe_request()
}

/// The viewer's screen, decode capabilities and wanted view mode (`Hello`).
///
/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_viewer(s: *const HostSession) -> ViewerHello {
    (*s).viewer
}

/// Takes a view-mode change the viewer asked for. False if none.
///
/// # Safety
/// `s` from `glass_host_accept`; `mode` and `allow_444` writable.
#[no_mangle]
pub unsafe extern "C" fn glass_session_take_mode_request(
    s: *const HostSession,
    mode: *mut u8,
    allow_444: *mut bool,
) -> bool {
    match (*s).take_mode_request() {
        Some((m, a)) => {
            *mode = m;
            *allow_444 = a;
            true
        }
        None => false,
    }
}

/// Tells the viewer the stream changes from the next IDR on.
///
/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_send_stream_change(
    s: *mut HostSession,
    info: StreamInfo,
) -> bool {
    (*s).send_stream_change(info).is_ok()
}

/// Latest input packet applied; stamp it on the next captured frame.
///
/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_input_seq(s: *const HostSession) -> u32 {
    (*s).input_seq()
}

/// Seals and sends one encoded frame (Annex-B). Returns 1 if sent, 0 if the
/// viewer's address isn't known yet (nothing sent), -1 on error.
///
/// # Safety
/// `s` from `glass_host_accept`; `f` readable; `data` readable for `len`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_send_frame(
    s: *mut HostSession,
    f: *const OutFrame,
    data: *const u8,
    len: usize,
) -> i32 {
    match (*s).send_frame(&*f, slice(data, len)) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(_) => -1,
    }
}

/// Sends a new pointer image: straight-alpha RGBA, top row first,
/// `width * height * 4` bytes (at most 96 x 96). Returns false on error.
///
/// # Safety
/// `s` from `glass_host_accept`; `rgba` readable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_session_send_cursor(
    s: *mut HostSession,
    serial: u32,
    width: u16,
    height: u16,
    hot_x: u16,
    hot_y: u16,
    rgba: *const u8,
    len: usize,
) -> bool {
    let shape = CursorShape {
        serial,
        width,
        height,
        hot_x,
        hot_y,
        rgba: slice(rgba, len),
    };
    (*s).send_cursor(&shape).is_ok()
}

/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_send_cursor_visible(
    s: *mut HostSession,
    visible: bool,
) -> bool {
    (*s).send_cursor_visible(visible).is_ok()
}

/// Tells the viewer about a condition (`NOTICE_*`; 0 clears it).
///
/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_send_notice(s: *mut HostSession, code: u8) -> bool {
    (*s).send_notice(code).is_ok()
}

/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_counters(s: *const HostSession) -> HostCounters {
    (*s).counters()
}

/// Ends the session (releasing any keys the viewer held) and frees it.
///
/// # Safety
/// `s` from `glass_host_accept` (or null), not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glass_session_close(s: *mut HostSession) {
    if !s.is_null() {
        drop(Box::from_raw(s));
    }
}

// ---- Clipboard (M5c) ------------------------------------------------------

/// Clipboard data from the other side (opaque): text, an offer, or what a
/// fetch got. Read it with `glass_clip_kind` / `_data` / `_len` / `_id` /
/// `_size`, then `glass_clip_free`. Files: `data` is their paths,
/// NUL-separated.
pub struct ClipHandle {
    item: crate::clipboard::ClipItem,
    id: u32,
    size: u64,
}

unsafe fn clip_from(kind: u8, data: *const u8, len: usize) -> Option<crate::clipboard::ClipItem> {
    if data.is_null() || len == 0 || len > crate::clipboard::CLIP_MAX {
        return None;
    }
    let data = std::slice::from_raw_parts(data, len).to_vec();
    Some(crate::clipboard::ClipItem { kind, data })
}

fn handle(item: crate::clipboard::ClipItem, id: u32, size: u64) -> *mut ClipHandle {
    Box::into_raw(Box::new(ClipHandle { item, id, size }))
}

fn joined(paths: &[std::path::PathBuf]) -> Vec<u8> {
    paths
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\0")
        .into_bytes()
}

fn text_out(item: Option<crate::clipboard::ClipItem>) -> *mut ClipHandle {
    item.map_or(std::ptr::null_mut(), |i| {
        let n = i.data.len() as u64;
        handle(i, 0, n)
    })
}

fn offer_out(o: Option<crate::clipboard::Offer>) -> *mut ClipHandle {
    o.map_or(std::ptr::null_mut(), |o| {
        let item = crate::clipboard::ClipItem {
            kind: o.kind,
            data: joined(&o.files),
        };
        handle(item, o.id, o.size)
    })
}

fn fetch_out(id: u32, st: crate::clipboard::FetchState, state: *mut u8) -> *mut ClipHandle {
    use crate::clipboard::{FetchState, Fetched, CLIP_FILES, CLIP_PNG};
    let (code, out) = match st {
        FetchState::Pending => (0, std::ptr::null_mut()),
        FetchState::Failed => (2, std::ptr::null_mut()),
        FetchState::Done(Fetched::Files(f)) => {
            let item = crate::clipboard::ClipItem {
                kind: CLIP_FILES,
                data: joined(&f),
            };
            (1, handle(item, id, 0))
        }
        FetchState::Done(Fetched::Png(data)) => {
            let n = data.len() as u64;
            (
                1,
                handle(
                    crate::clipboard::ClipItem {
                        kind: CLIP_PNG,
                        data,
                    },
                    id,
                    n,
                ),
            )
        }
    };
    if !state.is_null() {
        unsafe { *state = code };
    }
    out
}

/// Paths, NUL-separated UTF-8 (`a\0b`), as the file list of a copy.
unsafe fn paths_from(data: *const u8, len: usize) -> Vec<std::path::PathBuf> {
    if data.is_null() || len == 0 {
        return Vec::new();
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(data, len))
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .collect()
}

unsafe fn dir_arg(dir: *const c_char) -> Option<std::path::PathBuf> {
    str_arg(dir)
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
}

/// This Mac copied text (`CLIP_TEXT`, sent now) or a picture (`CLIP_PNG`,
/// offered: sent only if pasted). The bytes are copied. False if empty or
/// too big.
///
/// # Safety
/// `c` from `glass_client_connect`; `data` points to `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_client_send_clipboard(
    c: *const Client,
    kind: u8,
    data: *const u8,
    len: usize,
) -> bool {
    match clip_from(kind, data, len) {
        Some(item) => {
            (*c).send_clipboard(item);
            true
        }
        None => false,
    }
}

/// This Mac copied files and folders (`paths`: NUL-separated UTF-8):
/// offered, sent only if pasted.
///
/// # Safety
/// `c` from `glass_client_connect`; `paths` points to `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_client_send_files(c: *const Client, paths: *const u8, len: usize) {
    let p = paths_from(paths, len);
    if !p.is_empty() {
        (*c).send_files(p);
    }
}

/// Text from the host, once; null if none.
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_take_clipboard(c: *const Client) -> *mut ClipHandle {
    text_out((*c).take_clipboard())
}

/// A new offer from the host, once; null if none. `kind` `CLIP_PNG` or
/// `CLIP_FILES` (data: where the top-level files will be), `id` for
/// `glass_client_fetch`, `size` in bytes.
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_take_offer(c: *const Client) -> *mut ClipHandle {
    offer_out((*c).take_offer())
}

/// Something was pasted: fetches offer `id`. `state`: 0 still coming (call
/// again), 1 done (the handle has the picture or the file paths), 2 failed.
///
/// # Safety
/// `c` from `glass_client_connect`; `state` writable or null.
#[no_mangle]
pub unsafe extern "C" fn glass_client_fetch(
    c: *const Client,
    id: u32,
    state: *mut u8,
) -> *mut ClipHandle {
    fetch_out(id, (*c).fetch(id), state)
}

/// Accept files from the host into `dir` (null or empty: refuse them).
///
/// # Safety
/// `c` from `glass_client_connect`; `dir` a C string or null.
#[no_mangle]
pub unsafe extern "C" fn glass_client_set_file_dir(c: *const Client, dir: *const c_char) {
    (*c).set_file_dir(dir_arg(dir));
}

/// A session's Wi-Fi link tracker (M4 step 5): feed it a sample about
/// once a second, read its results line at the end.
#[no_mangle]
pub extern "C" fn glass_link_track_new() -> *mut crate::spikes::LinkTrack {
    Box::into_raw(Box::default())
}

/// # Safety
/// `t` from `glass_link_track_new`; `s` valid.
#[no_mangle]
pub unsafe extern "C" fn glass_link_track_feed(
    t: *mut crate::spikes::LinkTrack,
    s: *const crate::spikes::LinkSample,
) {
    (*t).feed(&*s);
}

/// The results line, NUL-terminated into `buf`; returns its full length.
///
/// # Safety
/// `t` from `glass_link_track_new`; `buf` valid for `len` bytes (or null).
#[no_mangle]
pub unsafe extern "C" fn glass_link_track_text(
    t: *const crate::spikes::LinkTrack,
    buf: *mut c_char,
    len: usize,
) -> usize {
    copy_str(&(*t).describe(), buf, len)
}

/// # Safety
/// `t` from `glass_link_track_new`, not used after.
#[no_mangle]
pub unsafe extern "C" fn glass_link_track_free(t: *mut crate::spikes::LinkTrack) {
    if !t.is_null() {
        drop(Box::from_raw(t));
    }
}

/// The sound ring the output device reads (M4b), or null until the host
/// sends sound. Release it with `glass_audio_ring_free` (it stays valid
/// after the client closes).
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_audio_ring(c: *const Client) -> *const crate::audio::PcmRing {
    (*c).audio_ring().map_or(std::ptr::null(), Arc::into_raw)
}

/// Fills `out` (`n` interleaved stereo samples at 48 kHz) from the ring at
/// `gain`, silence for what isn't there. For the audio device's callback:
/// never blocks or allocates. Returns the samples that were sound.
///
/// # Safety
/// `ring` from `glass_client_audio_ring`; `out` valid for `n` floats.
#[no_mangle]
pub unsafe extern "C" fn glass_audio_ring_read(
    ring: *const crate::audio::PcmRing,
    out: *mut f32,
    n: usize,
    gain: f32,
) -> usize {
    (*ring).read(std::slice::from_raw_parts_mut(out, n), gain)
}

/// # Safety
/// `ring` from `glass_client_audio_ring`, not used after.
#[no_mangle]
pub unsafe extern "C" fn glass_audio_ring_free(ring: *const crate::audio::PcmRing) {
    if !ring.is_null() {
        drop(Arc::from_raw(ring));
    }
}

/// The results line for `s`, NUL-terminated into `buf`; returns its full
/// length.
///
/// # Safety
/// `s` valid; `buf` valid for `len` bytes (or null).
#[no_mangle]
pub unsafe extern "C" fn glass_audio_stats_text(
    s: *const crate::audio::AudioStats,
    buf: *mut c_char,
    len: usize,
) -> usize {
    copy_str(&crate::audio::describe(&*s), buf, len)
}

/// Hold at least `us` of sound (20–80 ms), so it isn't ahead of a slower
/// picture (M4b).
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_set_audio_min_target(c: *const Client, us: u64) {
    (*c).set_audio_min_target(us);
}

/// What the sound did so far (M4b).
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_audio_stats(c: *const Client) -> crate::audio::AudioStats {
    (*c).audio_stats()
}

/// Wi-Fi delay spikes so far (M4 step 5).
///
/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_spikes(c: *const Client) -> crate::spikes::SpikeSummary {
    (*c).spikes()
}

/// The results line for `s` (as on Windows), NUL-terminated into `buf`;
/// returns its full length.
///
/// # Safety
/// `s` valid; `buf` valid for `len` bytes (or null).
#[no_mangle]
pub unsafe extern "C" fn glass_spike_text(
    s: *const crate::spikes::SpikeSummary,
    buf: *mut c_char,
    len: usize,
) -> usize {
    copy_str(&crate::spikes::describe(&*s), buf, len)
}

/// # Safety
/// `c` from `glass_client_connect`.
#[no_mangle]
pub unsafe extern "C" fn glass_client_clip_progress(c: *const Client) -> ClipProgress {
    (*c).clip_progress()
}

/// See `glass_client_send_clipboard`.
///
/// # Safety
/// `s` from `glass_host_accept`; `data` points to `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_session_send_clipboard(
    s: *const HostSession,
    kind: u8,
    data: *const u8,
    len: usize,
) -> bool {
    match clip_from(kind, data, len) {
        Some(item) => {
            (*s).send_clipboard(item);
            true
        }
        None => false,
    }
}

/// See `glass_client_send_files`.
///
/// # Safety
/// `s` from `glass_host_accept`; `paths` points to `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn glass_session_send_files(
    s: *const HostSession,
    paths: *const u8,
    len: usize,
) {
    let p = paths_from(paths, len);
    if !p.is_empty() {
        (*s).send_files(p);
    }
}

/// Text from the viewer, once; null if none.
///
/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_take_clipboard(s: *const HostSession) -> *mut ClipHandle {
    text_out((*s).take_clipboard())
}

/// See `glass_client_take_offer`.
///
/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_take_offer(s: *const HostSession) -> *mut ClipHandle {
    offer_out((*s).take_offer())
}

/// See `glass_client_fetch`.
///
/// # Safety
/// `s` from `glass_host_accept`; `state` writable or null.
#[no_mangle]
pub unsafe extern "C" fn glass_session_fetch(
    s: *const HostSession,
    id: u32,
    state: *mut u8,
) -> *mut ClipHandle {
    fetch_out(id, (*s).fetch(id), state)
}

/// Accept files from the viewer into `dir` (null or empty: refuse them).
///
/// # Safety
/// `s` from `glass_host_accept`; `dir` a C string or null.
#[no_mangle]
pub unsafe extern "C" fn glass_session_set_file_dir(s: *const HostSession, dir: *const c_char) {
    (*s).set_file_dir(dir_arg(dir));
}

/// # Safety
/// `s` from `glass_host_accept`.
#[no_mangle]
pub unsafe extern "C" fn glass_session_clip_progress(s: *const HostSession) -> ClipProgress {
    (*s).clip_progress()
}

/// # Safety
/// `h` from a clipboard call, not freed.
#[no_mangle]
pub unsafe extern "C" fn glass_clip_kind(h: *const ClipHandle) -> u8 {
    (*h).item.kind
}

/// # Safety
/// `h` from a clipboard call, not freed.
#[no_mangle]
pub unsafe extern "C" fn glass_clip_len(h: *const ClipHandle) -> usize {
    (*h).item.data.len()
}

/// The bytes; valid until `glass_clip_free`.
///
/// # Safety
/// `h` from a clipboard call, not freed.
#[no_mangle]
pub unsafe extern "C" fn glass_clip_data(h: *const ClipHandle) -> *const u8 {
    (*h).item.data.as_ptr()
}

/// An offer's id (for `_fetch`).
///
/// # Safety
/// `h` from a clipboard call, not freed.
#[no_mangle]
pub unsafe extern "C" fn glass_clip_id(h: *const ClipHandle) -> u32 {
    (*h).id
}

/// An offer's size in bytes.
///
/// # Safety
/// `h` from a clipboard call, not freed.
#[no_mangle]
pub unsafe extern "C" fn glass_clip_size(h: *const ClipHandle) -> u64 {
    (*h).size
}

/// # Safety
/// `h` from a clipboard call (or null), not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glass_clip_free(h: *mut ClipHandle) {
    if !h.is_null() {
        drop(Box::from_raw(h));
    }
}

/// HID usage -> Mac key code for the host (`keymap::hid_to_mac`); 0xFFFF if
/// the Mac has no such key.
#[no_mangle]
pub extern "C" fn glass_keymap_hid_to_mac(hid: u8, iso: bool) -> u16 {
    keymap::hid_to_mac(hid, iso).unwrap_or(0xFFFF)
}

/// Builds an Annex-B access unit from VideoToolbox output: the `prefix_count`
/// (at most 4) raw NALs in `prefix` (VPS/SPS/PPS on keyframes), then the
/// 4-byte length-prefixed NALs in `sample`. Returns bytes written, or -1 if
/// the sample is malformed or `out_cap` is too small.
///
/// # Safety
/// `prefix` readable for `prefix_count` entries (each a valid `Bytes`);
/// `sample` readable for `len`; `out` writable for `out_cap`.
#[no_mangle]
pub unsafe extern "C" fn glass_hevc_from_length_prefixed(
    prefix: *const Bytes,
    prefix_count: usize,
    sample: *const u8,
    len: usize,
    out: *mut u8,
    out_cap: usize,
) -> isize {
    let mut nals: [&[u8]; 4] = [&[]; 4];
    if prefix_count > nals.len() {
        return -1;
    }
    for (i, n) in nals.iter_mut().enumerate().take(prefix_count) {
        let b = *prefix.add(i);
        *n = slice(b.ptr, b.len);
    }
    let out = if out.is_null() || out_cap == 0 {
        &mut [][..]
    } else {
        std::slice::from_raw_parts_mut(out, out_cap)
    };
    annexb::from_length_prefixed(&nals[..prefix_count], slice(sample, len), out)
        .map_or(-1, |n| n as isize)
}

// --- Replay host (tests) ----------------------------------------------------

/// Serves a `.glsr` file (copied) as a live stream on `port` (0 = any free
/// port; the chosen one is written to `*out_port`), as a new in-memory host
/// identity "Glass test host" that pairs with the fixed PIN `pin`. Returns
/// null on error.
///
/// # Safety
/// `buf` must point to `len` readable bytes; `pin` NUL-terminated;
/// `out_port` valid for writes or null.
#[no_mangle]
pub unsafe extern "C" fn glass_replay_start(
    buf: *const u8,
    len: usize,
    port: u16,
    pin: *const c_char,
    out_port: *mut u16,
) -> *mut Replay {
    let Ok(dev) = Device::ephemeral("Glass test host") else {
        return std::ptr::null_mut();
    };
    let opts = HostOptions {
        fixed_pin: str_arg(pin).map(str::to_string),
        ..Default::default()
    };
    match Replay::start(slice(buf, len).to_vec(), port, dev, opts) {
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
        let dir = std::env::temp_dir().join(format!("glass-ffi-{}", std::process::id()));
        let dir_c = std::ffi::CString::new(dir.to_str().unwrap()).unwrap();
        unsafe {
            let d = glass_device_open(dir_c.as_ptr(), c"FFI test".as_ptr());
            assert!(!d.is_null());
            let c = glass_client_connect(
                d,
                c"127.0.0.1".as_ptr(),
                port,
                ViewerHello::default(),
                &mut err,
            );
            assert!(c.is_null());
            assert_eq!(err, ConnectError::Connect as i32);
            glass_client_close(c); // null is fine
            glass_device_free(d);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(glass_clock_now_us() > 0);
    }

    #[test]
    fn pair_and_connect_over_ffi() {
        // Smallest valid recording: one frame.
        let h = RecordingHeader {
            codec: CODEC_HEVC,
            chroma: CHROMA_420,
            width: 64,
            height: 32,
            fps: 60,
        };
        let mut w = RecordingWriter::new(Vec::new(), &h).unwrap();
        w.write_frame(
            &FrameMeta {
                frame_id: 0,
                flags: crate::protocol::FLAG_IDR,
                len: 5,
                ..Default::default()
            },
            &[0, 0, 1, 0x26, 1],
        )
        .unwrap();
        let buf = w.into_inner();
        let dir = std::env::temp_dir().join(format!("glass-ffi-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dir_c = std::ffi::CString::new(dir.to_str().unwrap()).unwrap();
        unsafe {
            let mut port = 0u16;
            let r = glass_replay_start(buf.as_ptr(), buf.len(), 0, c"135790".as_ptr(), &mut port);
            assert!(!r.is_null());
            let d = glass_device_open(dir_c.as_ptr(), c"FFI viewer".as_ptr());
            let mut err = 0;
            // Not paired yet.
            let c = glass_client_connect(
                d,
                c"127.0.0.1".as_ptr(),
                port,
                ViewerHello::default(),
                &mut err,
            );
            assert!(c.is_null());
            assert_eq!(err, ConnectError::NotPaired as i32);
            // Wrong PIN, then the right one.
            let p = glass_pair_begin(d, c"127.0.0.1".as_ptr(), port, &mut err);
            assert!(!p.is_null());
            assert!(!glass_pair_finish(p, c"000000".as_ptr(), &mut err));
            assert_eq!(err, ConnectError::WrongPin as i32);
            let p = glass_pair_begin(d, c"127.0.0.1".as_ptr(), port, &mut err);
            let mut name = [0 as c_char; 64];
            glass_pairing_host_name(p, name.as_mut_ptr(), name.len());
            assert_eq!(
                CStr::from_ptr(name.as_ptr()).to_str().unwrap(),
                "Glass test host"
            );
            assert!(
                glass_pair_finish(p, c"135790".as_ptr(), &mut err),
                "err {err}"
            );
            assert_eq!(glass_device_peer_count(d), 1);
            let mut fp = [0u8; 32];
            let roles = glass_device_peer(d, 0, fp.as_mut_ptr(), name.as_mut_ptr(), name.len());
            assert_eq!(roles, crate::identity::ROLE_HOST);
            assert_eq!(
                glass_device_peer(d, 1, fp.as_mut_ptr(), name.as_mut_ptr(), 64),
                0
            );
            // Connect works now and knows the host's name.
            let c = glass_client_connect(
                d,
                c"127.0.0.1".as_ptr(),
                port,
                ViewerHello::default(),
                &mut err,
            );
            assert!(!c.is_null(), "err {err}");
            glass_client_host_name(c, name.as_mut_ptr(), name.len());
            assert_eq!(
                CStr::from_ptr(name.as_ptr()).to_str().unwrap(),
                "Glass test host"
            );
            glass_client_close(c);
            // Forget: back to not paired.
            assert!(glass_device_forget(d, fp.as_ptr()));
            assert_eq!(glass_device_peer_count(d), 0);
            glass_device_free(d);
            glass_replay_stop(r);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_over_ffi() {
        use std::sync::atomic::AtomicU64;
        static INPUTS: AtomicU64 = AtomicU64::new(0);
        static PINS: AtomicU64 = AtomicU64::new(0);
        static MODE_SEEN: AtomicU64 = AtomicU64::new(0);
        unsafe extern "C" fn on_event(_ctx: *mut c_void, e: *const HostEventC) {
            if (*e).kind == HOST_EVENT_PAIRING_PIN {
                assert_eq!(
                    CStr::from_ptr((*e).pin.as_ptr()).to_str().unwrap(),
                    "112233"
                );
                assert_eq!(
                    CStr::from_ptr((*e).addr.as_ptr()).to_str().unwrap(),
                    "127.0.0.1"
                );
                PINS.fetch_add(1, Ordering::Relaxed);
            }
        }
        unsafe extern "C" fn on_input(_ctx: *mut c_void, a: *const InputActionC) {
            if (*a).kind == INPUT_KEY && (*a).hid == 0x04 {
                INPUTS.fetch_add(1, Ordering::Relaxed);
            }
        }
        let base = std::env::temp_dir().join(format!("glass-ffi-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let hdir = std::ffi::CString::new(base.join("h").to_str().unwrap()).unwrap();
        let viewer = Device::open(&base.join("v"), "FFI viewer", None).unwrap();
        unsafe {
            let hd = glass_device_open(hdir.as_ptr(), c"FFI Mac".as_ptr());
            let mut err = 0;
            let h = glass_host_listen(
                hd,
                0,
                c"112233".as_ptr(),
                crate::protocol::HOST_OS_MAC,
                crate::protocol::HOST_CAP_VIEW_MODES,
                on_event,
                std::ptr::null_mut(),
                &mut err,
            );
            assert!(!h.is_null(), "err {err}");
            let port = glass_host_port(h);
            struct P(*mut HostHandle);
            unsafe impl Send for P {}
            let hp = P(h);
            let info = StreamInfo {
                codec: 1,
                chroma: 0,
                width: 64,
                height: 32,
                fps: 60,
                mode: 0,
            };
            let acceptor = std::thread::spawn(move || {
                let hp = hp;
                let mut out = Vec::new();
                for _ in 0..3 {
                    let mut st = 9;
                    let s = glass_host_accept(hp.0, info, on_input, std::ptr::null_mut(), &mut st);
                    out.push(st);
                    if st == 1 {
                        // Stream a few frames, then close.
                        let mut sent = 0;
                        for i in 0..400 {
                            let f = OutFrame {
                                frame_id: i,
                                flags: crate::protocol::FLAG_IDR,
                                ..Default::default()
                            };
                            let d = [0u8, 0, 1, 0x26, 1, i as u8];
                            if glass_session_send_frame(s, &f, d.as_ptr(), d.len()) == 1 {
                                sent += 1;
                            }
                            if sent >= 5 && INPUTS.load(Ordering::Relaxed) >= 2 {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        // The viewer's Hello, then a mode switch round trip.
                        let v = glass_session_viewer(s);
                        assert_eq!((v.display_w, v.display_h, v.refresh_hz), (2560, 1440, 165));
                        assert_eq!((v.caps, v.mode), (crate::protocol::CAP_HEVC_444, 1));
                        let (mut m, mut a) = (0u8, true);
                        for _ in 0..500 {
                            if glass_session_take_mode_request(s, &mut m, &mut a) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        assert_eq!((m, a), (2, false));
                        let changed = StreamInfo {
                            width: 128,
                            mode: m,
                            ..info
                        };
                        assert!(glass_session_send_stream_change(s, changed));
                        while MODE_SEEN.load(Ordering::Relaxed) == 0 {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        let mut name = [0 as c_char; 64];
                        glass_session_peer_name(s, name.as_mut_ptr(), name.len());
                        assert_eq!(
                            CStr::from_ptr(name.as_ptr()).to_str().unwrap(),
                            "FFI viewer"
                        );
                        assert!(glass_session_counters(s).frames_sent >= 5);
                        glass_session_close(s);
                    }
                }
                out
            });
            // Pair, then connect and send a key.
            let p = pair_begin(&viewer, "127.0.0.1", port).unwrap();
            assert_eq!(p.host_name(), "FFI Mac");
            p.finish("112233").unwrap();
            let hello = crate::net::ViewerHello {
                display_w: 2560,
                display_h: 1440,
                refresh_hz: 165,
                caps: crate::protocol::CAP_HEVC_444,
                mode: 1,
            };
            let c = Client::connect_with(&viewer, "127.0.0.1", port, hello).unwrap();
            assert_eq!(c.host_os(), crate::protocol::HOST_OS_MAC);
            assert_eq!(c.host_caps(), crate::protocol::HOST_CAP_VIEW_MODES);
            let mut frames = 0;
            while frames < 5 {
                if let Wait::Frame(..) = c.wait_frame(Duration::from_secs(5)) {
                    frames += 1;
                    c.release_frame();
                } else {
                    panic!("no frames");
                }
            }
            c.input_key(0x04, true);
            c.input_key(0x04, false);
            while INPUTS.load(Ordering::Relaxed) < 2 {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(c.info_changes(), 0);
            c.set_mode(2, false);
            while c.info_changes() == 0 {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!((c.info().width, c.info().mode), (128, 2));
            MODE_SEEN.store(1, Ordering::Relaxed);
            drop(c);
            // Third accept is woken up.
            std::thread::sleep(Duration::from_millis(100));
            glass_host_wake(h);
            let statuses = acceptor.join().unwrap();
            assert_eq!(statuses, vec![0, 1, -2]);
            assert_eq!(PINS.load(Ordering::Relaxed), 1);
            glass_host_free(h);
            glass_device_free(hd);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn annexb_over_ffi() {
        let vps = [0x40u8, 1, 0xAA];
        let pre = [Bytes {
            ptr: vps.as_ptr(),
            len: vps.len(),
        }];
        let sample = [0u8, 0, 0, 3, 0x26, 1, 0xDD];
        let mut out = [0u8; 32];
        let n = unsafe {
            glass_hevc_from_length_prefixed(
                pre.as_ptr(),
                1,
                sample.as_ptr(),
                sample.len(),
                out.as_mut_ptr(),
                out.len(),
            )
        };
        assert_eq!(
            &out[..n as usize],
            &[0, 0, 0, 1, 0x40, 1, 0xAA, 0, 0, 0, 1, 0x26, 1, 0xDD]
        );
        assert_eq!(glass_keymap_hid_to_mac(0x04, false), 0x00);
        assert_eq!(glass_keymap_hid_to_mac(0x46, false), 0xFFFF);
    }
}

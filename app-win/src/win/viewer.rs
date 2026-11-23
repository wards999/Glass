//! `glass connect`: the Windows viewer (M3b). Shows a paired host (a Mac or
//! another PC) full screen and sends this PC's mouse and keyboard to it.
//!
//! - Render thread: frames from core -> D3D11VA decode -> video processor ->
//!   flip-model swap chain (one frame of latency). Only the newest decoded
//!   picture is shown; older ones are "superseded".
//! - UI thread: the window, a low-level keyboard hook (so Win, Alt+Tab and
//!   friends go to the host while the window is in front), mouse messages,
//!   and the host's pointer shape as this PC's own cursor (no round trip).
//! - A control bar drops down from a floating handle (`bar.rs`): view, colour,
//!   full screen, pin, disconnect, status.
//! - Local chords, never sent: Ctrl+Alt+Shift+F full screen/window,
//!   Ctrl+Alt+Shift+R next view mode (M3c), Ctrl+Alt+Shift+Q disconnect.
//! - 4:2:0 decodes on D3D11VA (any GPU); 4:4:4 on NVDEC (`decode444.rs`),
//!   falling back to asking the host for 4:2:0 if that fails.
//!
//! LATER(M5): on-screen stats overlay; display-linked pacing.

use super::bar::{self, Bar, Handle};
use super::decode::{Decoder, Picture};
use super::decode444::{self, CudaDecoder, Planar};
use super::present::{fit, Presenter};
use super::stats_box::StatsBox;
use super::{d3d, Res};
use glass_core::clock;
use glass_core::input::{BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT};
use glass_core::keymap::{pc_key_for_mac, win_scan_to_hid, PcModifierRule};
use glass_core::net::{Client, ViewerHello, Wait};
use glass_core::protocol::{
    CAP_HEVC_444, CAP_PREFER_420, HOST_CAP_444, HOST_CAP_VIEW_MODES, HOST_OS_MAC, HOST_OS_WINDOWS,
    MODE_COUNT, MODE_FIT, MODE_FIT_LARGER, MODE_FIT_MORE_SPACE, NOTICE_NONE,
};
use glass_core::recording::CHROMA_444;
use glass_core::stats::{StatRing, Summary};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateBitmap, CreateDIBSection, DeleteObject, EnumDisplaySettingsW,
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, ScreenToClient, BITMAPINFO,
    BITMAPV5HEADER, BI_BITFIELDS, DEVMODEW, DIB_RGB_COLORS, ENUM_CURRENT_SETTINGS, HGDIOBJ,
    MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VIRTUAL_KEY, VK_C, VK_CONTROL, VK_F, VK_MENU, VK_Q,
    VK_R, VK_S, VK_SHIFT, VK_W,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// How the viewer runs.
#[derive(Clone)]
pub struct Options {
    pub windowed: bool,
    pub seconds: Option<f64>,
    /// Key rule; `None` = from the host: Ctrl acts as ⌘ on a Mac host, keys
    /// stay as labelled on a PC host.
    pub rule: Option<PcModifierRule>,
    /// Ask for 4:4:4 when possible (else 4:2:0, which uses less bandwidth).
    pub full_colour: bool,
    /// Share the clipboard with the host (M5c).
    pub clipboard: bool,
}

/// Made before connecting: the GPU, and what `Hello` tells the host.
pub struct Prepared {
    gpu: d3d::Gpu,
    pub hello: ViewerHello,
    /// Monitor the window opens on: (full rect, work area, "WxH @ Hz").
    monitor: (RECT, RECT, String),
}

/// The GPU, the screen under the pointer and whether 4:4:4 can be decoded
/// (NVDEC); `mode` is the view mode to ask for; `full_colour` asks for 4:4:4
/// from the start (else 4:2:0, switchable later).
pub fn prepare(mode: u8, full_colour: bool) -> Res<Prepared> {
    d3d::init_process();
    // FFmpeg's own warnings would clutter the console; errors are reported.
    unsafe { ffmpeg_sys_next::av_log_set_level(ffmpeg_sys_next::AV_LOG_ERROR) };
    let gpu = d3d::Gpu::new_for_viewer()?;
    let (mon, work, desc, hz) = monitor_at_pointer();
    let mut caps = if decode444::available(&gpu.device) {
        CAP_HEVC_444
    } else {
        0
    };
    if !full_colour {
        caps |= CAP_PREFER_420;
    }
    Ok(Prepared {
        hello: ViewerHello {
            display_w: (mon.right - mon.left) as u16,
            display_h: (mon.bottom - mon.top) as u16,
            refresh_hz: hz,
            caps,
            mode: mode.min(MODE_COUNT - 1),
        },
        gpu,
        monitor: (mon, work, desc),
    })
}

/// "Mac's screen", "fit to this PC", ...
pub fn mode_name(m: u8) -> &'static str {
    match m {
        MODE_FIT => "View 1 of 4: Fit (fills this screen, sharp text)",
        MODE_FIT_MORE_SPACE => "View 2 of 4: More space (smaller text)",
        MODE_FIT_LARGER => "View 3 of 4: Larger (bigger text)",
        _ => "View 4 of 4: Host's own screen (may show black bars)",
    }
}

const WM_RENDER_DONE: u32 = WM_APP + 1;
/// Posted by the keyboard hook: toggle full screen (outside the hook).
const WM_TOGGLE_FULLSCREEN: u32 = WM_APP + 2;
/// Posted by the render thread: the stream's size/mode changed.
const WM_STREAM_CHANGED: u32 = WM_APP + 3;
/// Posted by the keyboard hook: ask the host for the next view mode.
const WM_NEXT_MODE: u32 = WM_APP + 4;
/// Posted by the keyboard hook: full colour (4:4:4) on/off.
const WM_TOGGLE_444: u32 = WM_APP + 5;
const TIMER_CURSOR: usize = 1;
const TIMER_SHOT: usize = 2;
const TIMER_REHOOK: usize = 3;
const TIMER_FRONT: usize = 4;

/// Brings the viewer to the front. Windows refuses SetForegroundWindow from
/// a thread that doesn't have the input (the viewer starts on a worker
/// thread of the Glass window), so borrow the front window's input briefly.
/// (m3f-test12: the viewer opened behind the Glass window.)
unsafe fn to_front(hwnd: HWND) {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    let fg = GetForegroundWindow();
    if fg == hwnd {
        return;
    }
    let theirs = GetWindowThreadProcessId(fg, None);
    let mine = GetCurrentThreadId();
    let attached = theirs != 0 && theirs != mine && AttachThreadInput(mine, theirs, true).as_bool();
    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);
    let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd));
    if attached {
        let _ = AttachThreadInput(mine, theirs, false);
    }
}

thread_local! {
    static DEBUG_SHOT: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
}

/// Saves the top 300 px of `hwnd`'s area on screen as a 24-bit BMP (debug).
pub(super) unsafe fn save_shot(hwnd: HWND, path: &std::path::Path) {
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, GetDC, GetDIBits, ReleaseDC,
        SelectObject, BITMAPINFOHEADER, BI_RGB, SRCCOPY,
    };
    let mut r = RECT::default();
    let _ = GetWindowRect(hwnd, &mut r);
    let (w, h) = (r.right - r.left, (r.bottom - r.top).min(800));
    if w <= 0 || h <= 0 {
        return;
    }
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, w, h);
    let old = SelectObject(mem, bmp.into());
    let _ = BitBlt(mem, 0, 0, w, h, Some(screen), r.left, r.top, SRCCOPY);
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: h,
            biPlanes: 1,
            biBitCount: 24,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let stride = ((w * 3 + 3) & !3) as usize;
    let mut px = vec![0u8; stride * h as usize];
    GetDIBits(
        mem,
        bmp,
        0,
        h as u32,
        Some(px.as_mut_ptr() as *mut _),
        &mut info,
        DIB_RGB_COLORS,
    );
    SelectObject(mem, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    let mut f = Vec::with_capacity(54 + px.len());
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&((54 + px.len()) as u32).to_le_bytes());
    f.extend_from_slice(&[0; 4]);
    f.extend_from_slice(&54u32.to_le_bytes());
    let hdr = std::slice::from_raw_parts(&info.bmiHeader as *const _ as *const u8, 40);
    f.extend_from_slice(hdr);
    f.extend_from_slice(&px);
    let _ = std::fs::write(path, f);
}

/// Shared by the UI and render threads.
struct Shared {
    client: Client,
    /// This PC can decode 4:4:4, and it hasn't failed.
    can_444: AtomicBool,
    /// The user wants 4:4:4 (Ctrl+Alt+Shift+C, `--colour`).
    want_444: AtomicBool,
    /// The view mode last asked for.
    mode: AtomicU8,
    stop: AtomicBool,
    reason: Mutex<Option<String>>,
    /// Client size, `w << 32 | h` (UI -> render).
    size: AtomicU64,
    rule: PcModifierRule,
    /// The on-screen stats are on (UI), and their latest text (render).
    show_stats: AtomicBool,
    stats_text: Mutex<String>,
    stats_gen: AtomicU32,
    /// Key presses this viewer sent (for the report).
    keys_sent: AtomicU32,
    clipboard: bool,
}

impl Shared {
    /// 4:4:4 if this PC can decode it and the user wants it.
    fn allow_444(&self) -> bool {
        self.can_444.load(Ordering::Relaxed) && self.want_444.load(Ordering::Relaxed)
    }

    fn end(&self, why: &str) {
        let mut r = self.reason.lock().unwrap_or_else(|e| e.into_inner());
        if r.is_none() {
            *r = Some(why.to_string());
        }
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Report {
    frames: u64,
    bytes: u64,
    presented: u64,
    superseded: u64,
    decode_errors: u64,
    last_error: Option<String>,
    /// What was shown, in order ("fit to this PC 2560x1440 4:4:4 (NVDEC)").
    views: Vec<String>,
    exact_present: bool,
    /// On-screen times Windows reported as earlier than the present call.
    stats_before_submit: u64,
    encode: StatRing,
    network: StatRing,
    queue: StatRing,
    decode: StatRing,
    wait: StatRing,
    present: StatRing,
    total: StatRing,
    input: StatRing,
    interval: StatRing,
}

/// UI-thread state (the window procedure and the keyboard hook run there).
struct Ui {
    sh: Arc<Shared>,
    hwnd: HWND,
    fullscreen: bool,
    windowed_rect: RECT,
    dest: RECT,
    arrow: HCURSOR,
    host_cursor: Option<HCURSOR>,
    cursor_visible: bool,
    cursor_gen: u32,
    cursor_shape: (u16, u16, u16, u16),
    rgba: Vec<u8>,
    dest_width: i32,
    ctrl: bool,
    alt: bool,
    shift: bool,
    buttons: u8,
    render_done: bool,
    notice: u8,
    /// The drop-down control bar, and when the pointer was last on it.
    bar: Option<Bar>,
    handle: Option<Handle>,
    stats_box: Option<StatsBox>,
    stats_seen: u32,
    /// The host notice the box is showing with the stats off.
    notice_shown: u8,
    bar_seen_us: u64,
    last_bar_status_us: u64,
    /// The clipboard thread (M5c), if sharing it.
    _clip: Option<super::clip::ClipSync>,
    /// The file-transfer line the box is showing ("" = none).
    files_shown: String,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
    /// The keyboard hook (re-installed every few seconds: Windows silently
    /// drops a low-level hook whose thread was once too slow to answer).
    static HOOK: std::cell::Cell<Option<HHOOK>> = const { std::cell::Cell::new(None) };
}

/// What the keyboard hook saw (for the report): key presses seen, sent,
/// skipped because another window was in front, skipped because the viewer
/// was busy.
static KEYS_SEEN: AtomicU32 = AtomicU32::new(0);
static KEYS_NOT_FRONT: AtomicU32 = AtomicU32::new(0);
static KEYS_BUSY: AtomicU32 = AtomicU32::new(0);
/// Key presses that came as window messages (the hook didn't take them).
static KEYS_WINDOW: AtomicU32 = AtomicU32::new(0);
/// The window that had the keys instead (class and title), for the report.
static KEYS_WENT_TO: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// "class \"title\"" of a window, for the report.
fn describe_window(h: HWND) -> String {
    let mut class = [0u16; 64];
    let mut title = [0u16; 64];
    unsafe {
        let n = GetClassNameW(h, &mut class) as usize;
        let t = GetWindowTextW(h, &mut title) as usize;
        format!(
            "{} \"{}\"",
            String::from_utf16_lossy(&class[..n]),
            String::from_utf16_lossy(&title[..t])
        )
    }
}

/// (Re-)installs the keyboard hook on this (the window's) thread.
fn install_hook() {
    unsafe {
        if let Some(h) = HOOK.with(|c| c.take()) {
            let _ = UnhookWindowsHookEx(h);
        }
        let inst: Option<HINSTANCE> = GetModuleHandleW(None).ok().map(Into::into);
        if let Ok(h) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), inst, 0) {
            HOOK.with(|c| c.set(Some(h)));
        }
    }
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|u| u.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn line(name: &str, s: &Summary) -> String {
    format!(
        "  {name:<18} p50 {:>7.2} ms  p99 {:>7.2} ms  max {:>7.2} ms  sd {:>6.2} ms  (n={})",
        s.p50_us as f64 / 1e3,
        s.p99_us as f64 / 1e3,
        s.max_us as f64 / 1e3,
        s.stddev_us / 1e3,
        s.count
    )
}

fn span(later: u64, earlier: u64) -> u32 {
    later.saturating_sub(earlier).min(u32::MAX as u64) as u32
}

/// The monitor under the pointer: (full rect, work area, "WxH @ Hz", Hz).
fn monitor_at_pointer() -> (RECT, RECT, String, u16) {
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        let _ = GetMonitorInfoW(mon, &mut info.monitorInfo as *mut MONITORINFO);
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let ok = EnumDisplaySettingsW(
            PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
        )
        .as_bool();
        let desc = if ok {
            format!(
                "{}x{} @ {} Hz",
                dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency
            )
        } else {
            "unknown display".into()
        };
        let hz = if ok { dm.dmDisplayFrequency as u16 } else { 0 };
        (
            info.monitorInfo.rcMonitor,
            info.monitorInfo.rcWork,
            desc,
            hz,
        )
    }
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    (
        (r.right - r.left).max(1) as u32,
        (r.bottom - r.top).max(1) as u32,
    )
}

/// Shows `client` until it ends; returns the report and the view mode last
/// asked for (to remember for this host).
pub fn run(client: Client, prep: Prepared, opts: Options, banner: &str) -> Res<(String, u8)> {
    // The keyboard counters are for this session only.
    for c in [&KEYS_SEEN, &KEYS_NOT_FRONT, &KEYS_BUSY, &KEYS_WINDOW] {
        c.store(0, Ordering::Relaxed);
    }
    KEYS_WENT_TO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let Prepared {
        gpu,
        hello,
        monitor: (mon, work, display),
    } = prep;
    let started = clock::now_us();
    let client_os = client.host_os();
    let sh = Arc::new(Shared {
        client,
        can_444: AtomicBool::new(hello.caps & CAP_HEVC_444 != 0),
        want_444: AtomicBool::new(opts.full_colour),
        mode: AtomicU8::new(hello.mode),
        stop: AtomicBool::new(false),
        reason: Mutex::new(None),
        size: AtomicU64::new(0),
        show_stats: AtomicBool::new(load_stats_pref()),
        stats_text: Mutex::new(String::new()),
        stats_gen: AtomicU32::new(0),
        keys_sent: AtomicU32::new(0),
        clipboard: opts.clipboard,
        rule: opts.rule.unwrap_or(if client_os == HOST_OS_MAC {
            PcModifierRule::PcStyle
        } else {
            PcModifierRule::MacStyle
        }),
    });

    let hwnd = unsafe { create_window(&sh, mon, work, !opts.windowed)? };
    let (w, h) = client_size(hwnd);
    sh.size
        .store((w as u64) << 32 | h as u64, Ordering::Relaxed);

    let report = Arc::new(Mutex::new(Report::default()));
    let render = {
        let sh = sh.clone();
        let report = report.clone();
        let device = gpu.device.clone();
        let raw = hwnd.0 as isize;
        let seconds = opts.seconds;
        std::thread::spawn(move || {
            let hwnd = HWND(raw as *mut _);
            let mut r = Report::default();
            if let Err(e) = render_loop(&sh, &device, hwnd, &mut r, seconds) {
                sh.end(&format!("error: {e}"));
            }
            *report.lock().unwrap_or_else(|e| e.into_inner()) = r;
            sh.stop.store(true, Ordering::Relaxed);
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_RENDER_DONE, WPARAM(0), LPARAM(0));
            }
        })
    };

    // Keyboard hook: keys go to the host while our window is in front.
    install_hook();
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Some(h) = HOOK.with(|c| c.take()) {
            let _ = UnhookWindowsHookEx(h);
        }
    }
    let _ = render.join();
    let host_cursor = UI
        .with(|u| u.borrow_mut().take())
        .and_then(|u| u.host_cursor);
    if let Some(c) = host_cursor {
        unsafe {
            let _ = DestroyCursor(c);
        }
    }

    // Report.
    let secs = (clock::now_us() - started) as f64 / 1e6;
    let st = sh.client.stats();
    let r = report.lock().unwrap_or_else(|e| e.into_inner());
    let reason = sh
        .reason
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| "viewer closed".into());
    let host = sh.client.host();
    let info = sh.client.info();
    let mut out = String::new();
    use std::fmt::Write;
    let _ = writeln!(out, "{banner}");
    let _ = writeln!(out, "PC: {} | display {display}", gpu.name);
    let _ = writeln!(
        out,
        "Live: {} {}:{} (paired, encrypted)  {}x{} @ {} Hz HEVC {}, {secs:.1} s (ended: {reason})",
        sh.client.peer().name,
        host.ip(),
        host.port(),
        info.width,
        info.height,
        info.fps,
        if info.chroma == 1 { "4:4:4" } else { "4:2:0" }
    );
    let _ = writeln!(
        out,
        "  received {} frames ({:.1} fps), presented {} ({:.1} fps), superseded {}, decode errors {}",
        r.frames,
        r.frames as f64 / secs,
        r.presented,
        r.presented as f64 / secs,
        r.superseded,
        r.decode_errors
    );
    let _ = writeln!(
        out,
        "  packets {} | lost frames {} | skipped {} | late {} | dup {} | bad {} | rejected {} | queue full {} | keyframe requests {}",
        st.rx.packets,
        st.rx.frames_lost,
        st.rx.frames_skipped,
        st.rx.late,
        st.rx.duplicates,
        st.rx.bad,
        st.crypto_bad + st.crypto_replayed,
        st.queue_full,
        st.keyframe_requests
    );
    let _ = writeln!(
        out,
        "  bitrate {:.1} Mbps avg | clock sync {} (best RTT {:.2} ms, last {:.2} ms)",
        r.bytes as f64 * 8.0 / secs / 1e6,
        if st.clock_valid { "ok" } else { "NOT SYNCED" },
        st.clock_rtt_us as f64 / 1e3,
        st.last_rtt_us as f64 / 1e3
    );
    let _ = writeln!(
        out,
        "  input: {} events in {} packets | keyboard: {} presses via the window, {} seen by the hook, {} sent, {} with another window in front{}, {} while busy | keys {}",
        st.input_events,
        st.input_packets,
        KEYS_WINDOW.load(Ordering::Relaxed),
        KEYS_SEEN.load(Ordering::Relaxed),
        sh.keys_sent.load(Ordering::Relaxed),
        KEYS_NOT_FRONT.load(Ordering::Relaxed),
        {
            let w = KEYS_WENT_TO.lock().unwrap_or_else(|e| e.into_inner());
            if w.is_empty() {
                String::new()
            } else {
                format!(" ({w})")
            }
        },
        KEYS_BUSY.load(Ordering::Relaxed),
        match sh.rule {
            PcModifierRule::PcStyle => "PC-style (Ctrl -> Cmd, Win -> Ctrl)",
            PcModifierRule::MacStyle => "as labelled (Ctrl -> Ctrl, Win -> Cmd)",
        }
    );
    let _ = writeln!(
        out,
        "  clipboard: {}",
        if sh.clipboard {
            super::clip::summary(&sh.client.clip_progress(), "host")
        } else {
            "off".to_string()
        }
    );
    let _ = writeln!(
        out,
        "  shown: {} | 4:4:4 decode on this PC: {} | present timing: {}",
        r.views.join(" -> "),
        if hello.caps & CAP_HEVC_444 == 0 {
            "no"
        } else if sh.can_444.load(Ordering::Relaxed) {
            "yes (NVDEC)"
        } else {
            "failed"
        },
        if !r.exact_present {
            "estimated (no frame statistics)".to_string()
        } else if r.stats_before_submit > 0 {
            format!(
                "DXGI frame statistics ({} of {} implausible, counted as the present call)",
                r.stats_before_submit, r.presented
            )
        } else {
            "DXGI frame statistics".to_string()
        }
    );
    if let Some(e) = &r.last_error {
        let _ = writeln!(out, "  last decode error: {e}");
    }
    for (name, s) in [
        ("encode (host)", &r.encode),
        ("network", &r.network),
        ("queue", &r.queue),
        ("decode", &r.decode),
        ("wait for refresh", &r.wait),
        ("present", &r.present),
        ("capture -> screen", &r.total),
        ("input -> screen", &r.input),
        ("present interval", &r.interval),
    ] {
        let _ = writeln!(out, "{}", line(name, &s.summary()));
    }
    Ok((out, sh.mode.load(Ordering::Relaxed)))
}

/// The decoder for the current stream: D3D11VA (4:2:0), or NVDEC (4:4:4).
enum Dec {
    Hw(Decoder),
    Nv(CudaDecoder),
}

enum Pic {
    Hw(Picture),
    Nv(Planar),
}

/// Rates for the on-screen stats, over the last second or so.
#[derive(Default)]
struct Rates {
    t: u64,
    frames: u64,
    presented: u64,
    bytes: u64,
    rx: f64,
    shown: f64,
    mbps: f64,
}

/// The on-screen stats text (the same lines as the Mac viewer's).
fn overlay_text(
    sh: &Shared,
    r: &Report,
    info: &glass_core::protocol::StreamInfo,
    dec: &Dec,
    rates: &mut Rates,
    now: u64,
) -> String {
    let c = &sh.client;
    let st = c.stats();
    if rates.t == 0 || now.saturating_sub(rates.t) >= 1_000_000 {
        if rates.t != 0 {
            let s = (now - rates.t) as f64 / 1e6;
            rates.rx = (r.frames - rates.frames) as f64 / s;
            rates.shown = (r.presented - rates.presented) as f64 / s;
            rates.mbps = (r.bytes - rates.bytes) as f64 * 8.0 / s / 1e6;
        }
        rates.t = now;
        rates.frames = r.frames;
        rates.presented = r.presented;
        rates.bytes = r.bytes;
    }
    let p50 = |s: &StatRing| {
        let x = s.summary();
        if x.count == 0 {
            "-".to_string()
        } else {
            format!("{:.1}", x.p50_us as f64 / 1e3)
        }
    };
    let both = |s: &StatRing| {
        let x = s.summary();
        (x.count > 0).then(|| {
            format!(
                "p50 {:.1}  p99 {:.1} ms",
                x.p50_us as f64 / 1e3,
                x.p99_us as f64 / 1e3
            )
        })
    };
    let mut lines = Vec::with_capacity(8);
    if st.host_notice != NOTICE_NONE {
        lines.push("\u{26A0} The host isn't taking input right now (see the host).".to_string());
    }
    lines.push(format!(
        "Glass  {}  {}",
        c.peer().name,
        view_line(info, dec)
    ));
    lines.push(format!(
        "shown {:.0} fps  received {:.0} fps  {:.1} Mbps",
        rates.shown, rates.rx, rates.mbps
    ));
    lines.push(format!(
        "capture \u{2192} screen  {}",
        both(&r.total).unwrap_or_else(|| if st.clock_valid {
            "(waiting for frames)".into()
        } else {
            "(syncing clocks \u{2026})".into()
        })
    ));
    lines.push(format!(
        "input \u{2192} screen    {}",
        both(&r.input).unwrap_or_else(|| "(move or type to measure)".into())
    ));
    lines.push(format!(
        "  encode {} | network {} | queue {} | decode {} | wait {} | present {}   (p50 ms)",
        p50(&r.encode),
        p50(&r.network),
        p50(&r.queue),
        p50(&r.decode),
        p50(&r.wait),
        p50(&r.present)
    ));
    lines.push(format!(
        "lost {}  skipped {}  keyframe req {}  decode err {}  RTT {:.1} ms",
        st.rx.frames_lost,
        st.rx.frames_skipped,
        st.keyframe_requests,
        r.decode_errors,
        st.last_rtt_us as f64 / 1e3
    ));
    lines.push("Ctrl+Alt+Shift+S hide stats \u{00B7} +F full screen \u{00B7} +Q disconnect".into());
    lines.join("\n")
}

/// What a host notice means, for the viewer's screen.
/// "Sending files to “QUIETPC” … 45%" while files are on their way.
fn files_line(p: &glass_core::clipboard::ClipProgress, host: &str) -> Option<String> {
    let mb = |b: u64| b as f64 / 1e6;
    let (verb, done, total) = if p.out_total > 0 && p.out_done < p.out_total {
        ("Sending files to", p.out_done, p.out_total)
    } else if p.in_total > 0 && p.in_done < p.in_total {
        ("Receiving files from", p.in_done, p.in_total)
    } else {
        return None;
    };
    Some(format!(
        "{verb} \u{201C}{host}\u{201D} \u{2026} {}%  ({:.1} of {:.1} MB)\nThe paste finishes once they're all there.",
        done * 100 / total,
        mb(done),
        mb(total)
    ))
}

fn notice_text(code: u8, host: &str) -> String {
    match code {
        glass_core::protocol::NOTICE_INPUT_BLOCKED_ELEVATED => format!(
            "\u{26A0} \u{201C}{host}\u{201D} is ignoring the mouse and keyboard: a program running as\nadministrator is in front. Click another window there, or restart Glass on it as administrator."
        ),
        _ => format!(
            "\u{26A0} \u{201C}{host}\u{201D} is locked or showing a Windows security prompt.\nGlass can't show or control that screen yet: unlock it at the PC itself."
        ),
    }
}

/// Whether the on-screen stats are on, kept between sessions.
fn load_stats_pref() -> bool {
    glass_core::identity::default_dir()
        .and_then(|d| std::fs::read_to_string(d.join("viewer-stats.txt")).ok())
        .is_some_and(|s| s.trim() == "1")
}

fn save_stats_pref(on: bool) {
    if let Some(d) = glass_core::identity::default_dir() {
        let _ = std::fs::write(d.join("viewer-stats.txt"), if on { "1" } else { "0" });
    }
}

fn view_line(info: &glass_core::protocol::StreamInfo, dec: &Dec) -> String {
    format!(
        "{} {}x{} {} ({})",
        match info.mode {
            0 => "host's screen",
            MODE_FIT => "fit",
            MODE_FIT_MORE_SPACE => "fit, more space",
            _ => "fit, larger",
        },
        info.width,
        info.height,
        if info.chroma == CHROMA_444 {
            "4:4:4"
        } else {
            "4:2:0"
        },
        match dec {
            Dec::Hw(_) => "D3D11VA",
            Dec::Nv(_) => "NVDEC",
        }
    )
}

/// A present waiting for its on-screen time.
#[derive(Clone, Copy)]
struct Pending {
    id: u32,
    submit: u64,
    capture: u64,
    input_seq: u32,
}

fn render_loop(
    sh: &Shared,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    hwnd: HWND,
    r: &mut Report,
    seconds: Option<f64>,
) -> Res<()> {
    let c = &sh.client;
    let make = |info: &glass_core::protocol::StreamInfo| -> Res<Dec> {
        Ok(if info.chroma == CHROMA_444 {
            Dec::Nv(CudaDecoder::new(device)?)
        } else {
            Dec::Hw(Decoder::new(device)?)
        })
    };
    let mut info = c.info();
    let mut seen_changes = c.info_changes();
    let mut dec = make(&info)?;
    r.views.push(view_line(&info, &dec));
    let mut errors_in_row = 0u32;
    let size = sh.size.load(Ordering::Relaxed);
    let mut pres = Presenter::new(device, hwnd, ((size >> 32) as u32, size as u32))?;
    let mut pending: VecDeque<Pending> = VecDeque::with_capacity(16);
    let mut last_shown = 0u64;
    let mut last_input = 0u32;
    let mut stats_failures = 0u32;
    let end = seconds.map(|s| clock::now_us() + (s * 1e6) as u64);
    let mut next_stats = 0u64;
    let mut rates = Rates::default();
    // Frames that queued up while the window and decoder were being made
    // are stale: drop them and start from a fresh keyframe.
    while let Wait::Frame(..) = c.wait_frame(Duration::ZERO) {
        c.release_frame();
    }
    c.request_keyframe();

    // Records one present once its on-screen time is known.
    let mut record = |r: &mut Report, p: Pending, shown: u64, last_shown: &mut u64| {
        r.present.push(span(shown, p.submit));
        if p.capture > 0 {
            r.total.push(span(shown, p.capture));
        }
        if *last_shown > 0 {
            r.interval.push(span(shown, *last_shown));
        }
        *last_shown = shown;
        if p.input_seq != 0 && p.input_seq != last_input {
            if let Some(sent) = c.input_sent_at(p.input_seq) {
                r.input.push(span(shown, sent));
                last_input = p.input_seq;
            }
        }
    };

    while !sh.stop.load(Ordering::Relaxed) {
        let size = sh.size.load(Ordering::Relaxed);
        pres.resize(((size >> 32) as u32, size as u32))?;
        if end.is_some_and(|e| clock::now_us() >= e) {
            sh.end("--seconds elapsed");
            break;
        }

        // The host changed the stream (view mode, size, chroma).
        if c.info_changes() != seen_changes {
            seen_changes = c.info_changes();
            let new = c.info();
            // A new decoder for every change: NVDEC can't grow past the
            // size it was opened at (m3c-test3: 2560x1440 -> 2940x1912 failed).
            let size_changed = new.width != info.width || new.height != info.height;
            if size_changed
                || new.chroma != info.chroma
                || matches!(dec, Dec::Nv(_)) != (new.chroma == CHROMA_444)
            {
                match make(&new) {
                    Ok(d) => dec = d,
                    Err(e) => {
                        // Can't decode 4:4:4 here after all: ask for 4:2:0.
                        r.last_error = Some(e.to_string());
                        sh.can_444.store(false, Ordering::Relaxed);
                        c.set_mode(sh.mode.load(Ordering::Relaxed), false);
                    }
                }
            }
            info = new;
            r.views.push(view_line(&info, &dec));
            errors_in_row = 0;
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_STREAM_CHANGED, WPARAM(0), LPARAM(0));
            }
        }

        // Decode everything waiting; show only the newest picture.
        let mut newest: Option<(Pic, u64, u32)> = None;
        let mut timeout = Duration::from_millis(20);
        loop {
            match c.wait_frame(timeout) {
                Wait::Frame(meta, data) => {
                    let t0 = clock::now_us();
                    r.frames += 1;
                    r.bytes += data.len() as u64;
                    r.encode.push(meta.encode_us);
                    r.queue.push(span(t0, meta.last_packet_us));
                    let capture = c.host_to_local(meta.capture_ts_us).unwrap_or(0);
                    if capture > 0 {
                        r.network
                            .push(span(meta.last_packet_us, capture + meta.encode_us as u64));
                    }
                    let res = match &mut dec {
                        Dec::Hw(d) => d.decode(data).map(|p| p.map(Pic::Hw)),
                        Dec::Nv(d) => d.decode(data).map(|p| p.map(Pic::Nv)),
                    };
                    c.release_frame();
                    r.decode.push(span(clock::now_us(), t0));
                    match res {
                        Ok(Some(pic)) => {
                            errors_in_row = 0;
                            if newest.is_some() {
                                r.superseded += 1;
                            }
                            newest = Some((pic, capture, meta.input_seq));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            r.decode_errors += 1;
                            r.last_error = Some(e.to_string());
                            c.request_keyframe();
                            errors_in_row += 1;
                            if errors_in_row == 30 && matches!(dec, Dec::Nv(_)) {
                                // 4:4:4 keeps failing here: ask for 4:2:0.
                                sh.can_444.store(false, Ordering::Relaxed);
                                c.set_mode(sh.mode.load(Ordering::Relaxed), false);
                            }
                        }
                    }
                    timeout = Duration::ZERO;
                }
                Wait::Timeout => break,
                Wait::Disconnected => {
                    sh.end(if c.host_said_bye() {
                        "the host ended the session"
                    } else {
                        "the connection to the host dropped"
                    });
                    break;
                }
            }
        }

        if let Some((pic, capture, input_seq)) = newest {
            r.wait.push(pres.wait_ready(100) as u32);
            match &pic {
                Pic::Hw(p) => pres.draw(p)?,
                Pic::Nv(p) => pres.draw_planar(p)?,
            }
            let submit = clock::now_us();
            let id = pres.present()?;
            r.presented += 1;
            if pending.len() == pending.capacity() {
                pending.pop_front();
            }
            pending.push_back(Pending {
                id,
                submit,
                capture,
                input_seq,
            });
        }

        // The on-screen stats, four times a second while they're shown.
        let now = clock::now_us();
        if now >= next_stats && sh.show_stats.load(Ordering::Relaxed) {
            next_stats = now + 250_000;
            let text = overlay_text(sh, r, &info, &dec, &mut rates, now);
            *sh.stats_text.lock().unwrap_or_else(|e| e.into_inner()) = text;
            sh.stats_gen.fetch_add(1, Ordering::Relaxed);
        }

        // Resolve on-screen times.
        match pres.displayed() {
            Some((count, shown)) => {
                r.exact_present = true;
                while let Some(p) = pending.front().copied() {
                    if p.id > count {
                        break;
                    }
                    pending.pop_front();
                    if p.id == count {
                        // Windows sometimes reports a time before the
                        // present (composed windows): don't trust those.
                        let shown = if shown < p.submit {
                            r.stats_before_submit += 1;
                            p.submit
                        } else {
                            shown
                        };
                        record(r, p, shown, &mut last_shown);
                    }
                }
            }
            None => {
                stats_failures += 1;
                if stats_failures > 120 && !r.exact_present {
                    // No frame statistics here: count the present call.
                    while let Some(p) = pending.pop_front() {
                        record(r, p, p.submit, &mut last_shown);
                    }
                }
            }
        }
    }
    Ok(())
}

unsafe fn create_window(sh: &Arc<Shared>, mon: RECT, work: RECT, fullscreen: bool) -> Res<HWND> {
    let inst: HINSTANCE = GetModuleHandleW(None)?.into();
    let arrow = LoadCursorW(None, IDC_ARROW)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: inst,
        hCursor: arrow,
        lpszClassName: w!("GlassViewer"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    // Windowed: 3/4 of the work area, centred.
    let (ww, wh) = (
        (work.right - work.left) * 3 / 4,
        (work.bottom - work.top) * 3 / 4,
    );
    let windowed_rect = RECT {
        left: work.left + (work.right - work.left - ww) / 2,
        top: work.top + (work.bottom - work.top - wh) / 2,
        right: work.left + (work.right - work.left - ww) / 2 + ww,
        bottom: work.top + (work.bottom - work.top - wh) / 2 + wh,
    };
    let (style, r) = if fullscreen {
        (WS_POPUP, mon)
    } else {
        (WS_OVERLAPPEDWINDOW, windowed_rect)
    };
    let title = wide(&format!(
        "Glass - {}  (Ctrl+Alt+Shift+F full screen, +R view, +Q disconnect)",
        sh.client.peer().name
    ));
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("GlassViewer"),
        PCWSTR(title.as_ptr()),
        style,
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
        None,
        None,
        Some(inst),
        None,
    )?;
    UI.with(|u| {
        *u.borrow_mut() = Some(Ui {
            sh: sh.clone(),
            hwnd,
            fullscreen,
            windowed_rect,
            dest: RECT::default(),
            arrow,
            host_cursor: None,
            cursor_visible: true,
            cursor_gen: 0,
            cursor_shape: (0, 0, 0, 0),
            rgba: Vec::with_capacity(96 * 96 * 4),
            dest_width: 0,
            ctrl: false,
            alt: false,
            shift: false,
            buttons: 0,
            render_done: false,
            notice: NOTICE_NONE,
            bar: Bar::new(hwnd).ok(),
            handle: Handle::new(hwnd, load_handle_frac()).ok(),
            stats_box: StatsBox::new(hwnd).ok(),
            stats_seen: 0,
            notice_shown: NOTICE_NONE,
            bar_seen_us: 0,
            last_bar_status_us: 0,
            // Sends what's copied now too: copy, connect, paste.
            _clip: sh.clipboard.then(|| {
                sh.client
                    .set_file_dir(Some(super::clip::file_dir(false, "viewer")));
                super::clip::ClipSync::start(sh.client.clip_link(), true)
            }),
            files_shown: String::new(),
        })
    });
    with_ui(|ui| ui.layout());
    let _ = ShowWindow(hwnd, SW_SHOW);
    to_front(hwnd);
    // Once more shortly after, in case the Glass window took the focus back.
    SetTimer(Some(hwnd), TIMER_FRONT, 300, None);
    // GLASS_SHOW_BAR=1: start with the bar shown and pinned (checks, demos).
    if std::env::var_os("GLASS_SHOW_BAR").is_some() {
        with_ui(|ui| {
            if let Some(b) = &mut ui.bar {
                b.pinned = true;
            }
            ui.show_bar();
        });
    }
    // GLASS_DEBUG_SHOT=FILE.bmp: save the top of the window (with the bar)
    // two seconds in, to check how it looks without a person watching.
    if let Some(path) = std::env::var_os("GLASS_DEBUG_SHOT") {
        SetTimer(Some(hwnd), TIMER_SHOT, 2000, None);
        DEBUG_SHOT.with(|d| *d.borrow_mut() = Some(path.into()));
    }
    SetTimer(Some(hwnd), TIMER_CURSOR, 15, None);
    SetTimer(Some(hwnd), TIMER_REHOOK, 3000, None);
    Ok(hwnd)
}

/// Where the bar's handle sits across the top (0..1), kept between sessions.
fn load_handle_frac() -> f64 {
    glass_core::identity::default_dir()
        .and_then(|d| std::fs::read_to_string(d.join("bar-handle.txt")).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0.5)
}

fn save_handle_frac(f: f64) {
    if let Some(d) = glass_core::identity::default_dir() {
        let _ = std::fs::write(d.join("bar-handle.txt"), format!("{f:.3}"));
    }
}

impl Ui {
    /// The viewer's client area in screen coordinates.
    fn client_on_screen(&self) -> RECT {
        let mut r = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut r);
            let mut tl = POINT {
                x: r.left,
                y: r.top,
            };
            let _ = ClientToScreen(self.hwnd, &mut tl);
            RECT {
                left: tl.x,
                top: tl.y,
                right: tl.x + (r.right - r.left),
                bottom: tl.y + (r.bottom - r.top),
            }
        }
    }

    /// Puts the current view, colour and status on the bar.
    fn update_bar(&mut self) {
        let Some(bar) = &self.bar else { return };
        let c = &self.sh.client;
        let info = c.info();
        let st = c.stats();
        let status = format!(
            "{}  ·  {}x{}  ·  {}  ·  round trip {:.0} ms",
            c.peer().name,
            info.width,
            info.height,
            if info.chroma == CHROMA_444 {
                "4:4:4"
            } else {
                "4:2:0"
            },
            st.last_rtt_us as f64 / 1e3
        );
        let caps = c.host_caps();
        bar.set_state(
            info.mode,
            info.chroma == CHROMA_444,
            self.sh.can_444.load(Ordering::Relaxed) && caps & HOST_CAP_444 != 0,
            caps & HOST_CAP_VIEW_MODES != 0,
            self.fullscreen,
            self.sh.show_stats.load(Ordering::Relaxed),
            &status,
            match c.host_os() {
                HOST_OS_MAC => "Mac's screen",
                HOST_OS_WINDOWS => "PC's screen",
                _ => "Host's screen",
            },
        );
        self.last_bar_status_us = clock::now_us();
    }

    fn show_bar(&mut self) {
        self.update_bar();
        let r = self.client_on_screen();
        let centre = self
            .handle
            .as_ref()
            .map_or((r.left + r.right) / 2, |h| h.centre_x());
        if let Some(bar) = &mut self.bar {
            bar.show(r, centre);
        }
        if let Some(h) = &mut self.handle {
            h.hide();
        }
        self.bar_seen_us = clock::now_us();
    }

    /// Shows, moves or hides the on-screen stats, with the latest text.
    fn tick_stats(&mut self) {
        let show = self.sh.show_stats.load(Ordering::Relaxed);
        let c = self.client_on_screen();
        // Why the host isn't taking input (locked, a UAC prompt, an admin
        // window): shown even with the stats off, as on the Mac.
        let notice = self.sh.client.stats().host_notice;
        let host = self.sh.client.peer().name.clone();
        let files = files_line(&self.sh.client.clip_progress(), &host);
        let Some(b) = &mut self.stats_box else { return };
        // Files on their way: shown over the stats while they go.
        if let Some(line) = files.filter(|_| notice == NOTICE_NONE) {
            if line != self.files_shown {
                b.set_text(&line);
                self.files_shown = line;
            }
            b.place(c);
            return;
        }
        if !self.files_shown.is_empty() {
            self.files_shown.clear();
            self.notice_shown = NOTICE_NONE;
            self.stats_seen = self.stats_seen.wrapping_sub(1);
            if !show && notice == NOTICE_NONE {
                b.hide();
                return;
            }
        }
        if !show {
            if notice == NOTICE_NONE {
                self.notice_shown = NOTICE_NONE;
                b.hide();
                return;
            }
            if notice != self.notice_shown {
                self.notice_shown = notice;
                b.set_text(&notice_text(notice, &host));
            }
            b.place(c);
            return;
        }
        if self.notice_shown != NOTICE_NONE {
            // Back to the stats text on the next update.
            self.notice_shown = NOTICE_NONE;
            self.stats_seen = self.stats_seen.wrapping_sub(1);
        }
        let g = self.sh.stats_gen.load(Ordering::Relaxed);
        if g != self.stats_seen {
            self.stats_seen = g;
            let t = self
                .sh
                .stats_text
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            b.set_text(&t);
        }
        b.place(c);
    }

    fn hide_bar(&mut self) {
        if let Some(b) = &mut self.bar {
            b.hide();
        }
    }

    /// Hides the bar once the pointer has left it for a moment (unless pinned).
    fn tick_bar(&mut self) {
        let now = clock::now_us();
        let (visible, pinned, r) = match &self.bar {
            Some(b) => (b.visible, b.pinned, b.rect()),
            None => return,
        };
        if !visible {
            // The handle follows the window (moves, resizes, full screen).
            let c = self.client_on_screen();
            if let Some(h) = &mut self.handle {
                h.place(c);
            }
            return;
        }
        if now.saturating_sub(self.last_bar_status_us) > 500_000 {
            self.update_bar();
        }
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
        }
        let near = pt.x >= r.left - 12
            && pt.x < r.right + 12
            && pt.y >= r.top - 12
            && pt.y < r.bottom + 12;
        if near || pinned {
            self.bar_seen_us = now;
        } else if now.saturating_sub(self.bar_seen_us) > 700_000 {
            self.hide_bar();
        }
    }

    /// A click on the bar.
    fn bar_command(&mut self, id: usize) {
        let sh = self.sh.clone();
        let c = &sh.client;
        match id {
            i if (bar::ID_VIEW..bar::ID_VIEW + MODE_COUNT as usize).contains(&i) => {
                let m = (i - bar::ID_VIEW) as u8;
                sh.mode.store(m, Ordering::Relaxed);
                println!("Asking the host for: {} ...", mode_name(m));
                c.set_mode(m, sh.allow_444());
            }
            bar::ID_420 | bar::ID_444 => {
                let want = id == bar::ID_444;
                sh.want_444.store(want, Ordering::Relaxed);
                println!(
                    "Asking the host for {} ...",
                    if want { "full colour (4:4:4)" } else { "4:2:0" }
                );
                c.set_mode(sh.mode.load(Ordering::Relaxed), sh.allow_444());
            }
            bar::ID_FULLSCREEN => {
                self.toggle_fullscreen();
                if self.bar.as_ref().is_some_and(|b| b.visible) {
                    self.show_bar();
                }
            }
            bar::ID_HANDLE => {
                if self.bar.as_ref().is_some_and(|b| b.visible) {
                    self.hide_bar();
                } else {
                    self.show_bar();
                }
            }
            bar::ID_HANDLE_MOVED => {
                let c = self.client_on_screen();
                if let Some(h) = &mut self.handle {
                    h.moved(c);
                    save_handle_frac(h.frac);
                }
            }
            bar::ID_STATS => {
                let on = !self.sh.show_stats.load(Ordering::Relaxed);
                self.sh.show_stats.store(on, Ordering::Relaxed);
                save_stats_pref(on);
                self.tick_stats();
            }
            bar::ID_PIN => {
                if let Some(b) = &mut self.bar {
                    b.pinned = !b.pinned;
                }
            }
            bar::ID_DISCONNECT => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            _ => {}
        }
        self.update_bar();
    }

    /// Recomputes the picture area after a size change.
    fn layout(&mut self) {
        let (w, h) = client_size(self.hwnd);
        self.sh
            .size
            .store((w as u64) << 32 | h as u64, Ordering::Relaxed);
        let info = self.sh.client.info();
        self.dest = fit((w, h), (info.width as u32, info.height as u32));
        if self.dest.right - self.dest.left != self.dest_width {
            self.dest_width = self.dest.right - self.dest.left;
            self.cursor_gen = self.cursor_gen.wrapping_sub(1); // rescale the pointer
        }
    }

    /// Client point -> normalised picture position (clamped to the picture).
    fn to_host(&self, x: i32, y: i32) -> (u16, u16) {
        let d = self.dest;
        let (w, h) = ((d.right - d.left).max(2), (d.bottom - d.top).max(2));
        let nx = ((x - d.left).clamp(0, w - 1) as i64 * 65535 / (w - 1) as i64) as u16;
        let ny = ((y - d.top).clamp(0, h - 1) as i64 * 65535 / (h - 1) as i64) as u16;
        (nx, ny)
    }

    fn in_picture(&self, x: i32, y: i32) -> bool {
        let d = self.dest;
        x >= d.left && x < d.right && y >= d.top && y < d.bottom
    }

    fn toggle_fullscreen(&mut self) {
        unsafe {
            if self.fullscreen {
                let r = self.windowed_rect;
                SetWindowLongPtrW(
                    self.hwnd,
                    GWL_STYLE,
                    (WS_OVERLAPPEDWINDOW | WS_VISIBLE).0 as isize,
                );
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOP),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_FRAMECHANGED,
                );
            } else {
                let _ = GetWindowRect(self.hwnd, &mut self.windowed_rect);
                let mon = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                let _ = GetMonitorInfoW(mon, &mut info);
                let r = info.rcMonitor;
                SetWindowLongPtrW(self.hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOP),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_FRAMECHANGED,
                );
            }
        }
        self.fullscreen = !self.fullscreen;
        self.layout();
    }

    /// Polls the host's pointer and rebuilds our cursor when it changes.
    fn poll_cursor(&mut self) {
        let info = self.sh.client.cursor(self.cursor_gen, &mut self.rgba);
        if info.generation == self.cursor_gen {
            return;
        }
        self.cursor_gen = info.generation;
        self.cursor_visible = info.visible;
        let shape = (info.width, info.height, info.hot_x, info.hot_y);
        let n = info.width as usize * info.height as usize * 4;
        if n > 0 && self.rgba.len() >= n {
            let scale = self.dest_width.max(1) as f64 / self.sh.client.info().width.max(1) as f64;
            if let Some(c) = unsafe { make_cursor(&self.rgba[..n], shape, scale) } {
                if let Some(old) = self.host_cursor.replace(c) {
                    unsafe {
                        let _ = DestroyCursor(old);
                    }
                }
            }
            self.cursor_shape = shape;
        }
        self.apply_cursor();
    }

    /// Sets the cursor now if the pointer is over the picture.
    fn apply_cursor(&self) {
        unsafe {
            let mut pt = POINT::default();
            if GetCursorPos(&mut pt).is_err() || GetForegroundWindow() != self.hwnd {
                return;
            }
            let _ = ScreenToClient(self.hwnd, &mut pt);
            if self.in_picture(pt.x, pt.y) {
                SetCursor(self.picture_cursor());
            }
        }
    }

    fn picture_cursor(&self) -> Option<HCURSOR> {
        if !self.cursor_visible {
            None
        } else {
            Some(self.host_cursor.unwrap_or(self.arrow))
        }
    }

    fn button(&mut self, b: u8, down: bool, lparam: LPARAM) {
        // A click on the picture brings the viewer to the front, so keys
        // follow (m3f-test3: keys went to another window after clicking).
        if down {
            unsafe {
                if GetForegroundWindow() != self.hwnd {
                    let _ = SetForegroundWindow(self.hwnd);
                }
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(self.hwnd));
            }
        }
        let (x, y) = xy(lparam);
        let (nx, ny) = self.to_host(x, y);
        self.sh.client.input_button(b, down, nx, ny);
        let bit = 1u8 << b;
        unsafe {
            if down {
                if self.buttons == 0 {
                    SetCapture(self.hwnd);
                }
                self.buttons |= bit;
            } else {
                self.buttons &= !bit;
                if self.buttons == 0 {
                    let _ = ReleaseCapture();
                }
            }
        }
    }
}

fn xy(lparam: LPARAM) -> (i32, i32) {
    let v = lparam.0 as u32;
    ((v & 0xFFFF) as i16 as i32, (v >> 16) as i16 as i32)
}

/// Straight-alpha RGBA -> a Windows cursor, scaled like the picture.
unsafe fn make_cursor(
    rgba: &[u8],
    (w, h, hx, hy): (u16, u16, u16, u16),
    scale: f64,
) -> Option<HCURSOR> {
    let (w, h) = (w as usize, h as usize);
    let tw = ((w as f64 * scale).round() as usize).clamp(1, 256);
    let th = ((h as f64 * scale).round() as usize).clamp(1, 256);
    let mut hdr = BITMAPV5HEADER {
        bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: tw as i32,
        bV5Height: -(th as i32), // top-down
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5RedMask: 0x00FF_0000,
        bV5GreenMask: 0x0000_FF00,
        bV5BlueMask: 0x0000_00FF,
        bV5AlphaMask: 0xFF00_0000,
        ..Default::default()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let color = CreateDIBSection(
        None,
        &mut hdr as *mut BITMAPV5HEADER as *const BITMAPINFO,
        DIB_RGB_COLORS,
        &mut bits,
        None,
        0,
    )
    .ok()?;
    if bits.is_null() {
        let _ = DeleteObject(HGDIOBJ(color.0));
        return None;
    }
    let dst = std::slice::from_raw_parts_mut(bits as *mut u8, tw * th * 4);
    for y in 0..th {
        let sy = (y * h / th).min(h - 1);
        for x in 0..tw {
            let sx = (x * w / tw).min(w - 1);
            let s = (sy * w + sx) * 4;
            let d = (y * tw + x) * 4;
            dst[d] = rgba[s + 2];
            dst[d + 1] = rgba[s + 1];
            dst[d + 2] = rgba[s];
            dst[d + 3] = rgba[s + 3];
        }
    }
    let mask = CreateBitmap(tw as i32, th as i32, 1, 1, None);
    let ii = ICONINFO {
        fIcon: false.into(),
        xHotspot: ((hx as f64 * scale).round() as u32).min(tw as u32 - 1),
        yHotspot: ((hy as f64 * scale).round() as u32).min(th as u32 - 1),
        hbmMask: mask,
        hbmColor: color,
    };
    let icon = CreateIconIndirect(&ii);
    let _ = DeleteObject(HGDIOBJ(color.0));
    let _ = DeleteObject(HGDIOBJ(mask.0));
    icon.ok().map(|i| HCURSOR(i.0))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handled = with_ui(|ui| -> Option<LRESULT> {
        let sh = ui.sh.clone();
        let c = &sh.client;
        match msg {
            WM_MOUSEMOVE => {
                let (x, y) = xy(lparam);
                let (nx, ny) = ui.to_host(x, y);
                c.input_move(nx, ny);
                Some(LRESULT(0))
            }
            bar::WM_BAR_COMMAND => {
                ui.bar_command(wparam.0);
                Some(LRESULT(0))
            }
            // Typing: when the keyboard hook takes a key it never gets here;
            // when Windows has dropped the hook (m3f-test5: 0 presses seen
            // by it) the focused viewer still gets every key as a message.
            // Keys a Glass host on this PC typed into this window (a test on
            // one PC) aren't the user's: sending them back loops for ever.
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
                if windows::Win32::UI::WindowsAndMessaging::GetMessageExtraInfo().0 as usize
                    == super::inject::GLASS_MARK =>
            {
                Some(LRESULT(0))
            }
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                let vk = wparam.0 as u16;
                let held = |k: VIRTUAL_KEY| GetKeyState(k.0 as i32) < 0;
                if down && held(VK_CONTROL) && held(VK_MENU) && held(VK_SHIFT) {
                    let chord = if vk == VK_F.0 {
                        Some(WM_TOGGLE_FULLSCREEN)
                    } else if vk == VK_C.0 {
                        Some(WM_TOGGLE_444)
                    } else if vk == VK_R.0 {
                        Some(WM_NEXT_MODE)
                    } else if vk == VK_Q.0 || vk == VK_W.0 {
                        Some(WM_CLOSE)
                    } else {
                        None
                    };
                    if let Some(m) = chord {
                        c.input_release_all();
                        let _ = PostMessageW(Some(hwnd), m, WPARAM(0), LPARAM(0));
                        return Some(LRESULT(0));
                    }
                    if vk == VK_S.0 {
                        let _ = PostMessageW(
                            Some(hwnd),
                            bar::WM_BAR_COMMAND,
                            WPARAM(bar::ID_STATS),
                            LPARAM(0),
                        );
                        return Some(LRESULT(0));
                    }
                }
                let scan = ((lparam.0 >> 16) & 0xFF) as u16;
                let ext = (lparam.0 >> 24) & 1 == 1;
                let hid = win_scan_to_hid(scan, ext);
                if hid != 0 {
                    c.input_key(pc_key_for_mac(hid, sh.rule), down);
                    if down {
                        KEYS_WINDOW.fetch_add(1, Ordering::Relaxed);
                        sh.keys_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP => {
                ui.button(BUTTON_LEFT, msg == WM_LBUTTONDOWN, lparam);
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN | WM_RBUTTONUP => {
                ui.button(BUTTON_RIGHT, msg == WM_RBUTTONDOWN, lparam);
                Some(LRESULT(0))
            }
            WM_MBUTTONDOWN | WM_MBUTTONUP => {
                ui.button(BUTTON_MIDDLE, msg == WM_MBUTTONDOWN, lparam);
                Some(LRESULT(0))
            }
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let b = if (wparam.0 >> 16) as u16 == XBUTTON1 {
                    BUTTON_BACK
                } else {
                    BUTTON_FORWARD
                };
                ui.button(b, msg == WM_XBUTTONDOWN, lparam);
                Some(LRESULT(1))
            }
            WM_MOUSEWHEEL => {
                c.input_scroll((wparam.0 >> 16) as u16 as i16, 0);
                Some(LRESULT(0))
            }
            WM_MOUSEHWHEEL => {
                c.input_scroll(0, (wparam.0 >> 16) as u16 as i16);
                Some(LRESULT(0))
            }
            WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                let cur = if ui.in_picture(pt.x, pt.y) {
                    ui.picture_cursor()
                } else {
                    Some(ui.arrow)
                };
                SetCursor(cur);
                Some(LRESULT(1))
            }
            WM_TIMER if wparam.0 == TIMER_FRONT => {
                let _ = KillTimer(Some(hwnd), TIMER_FRONT);
                to_front(hwnd);
                Some(LRESULT(0))
            }
            WM_TIMER if wparam.0 == TIMER_REHOOK => {
                install_hook();
                Some(LRESULT(0))
            }
            WM_TIMER if wparam.0 == TIMER_SHOT => {
                let _ = KillTimer(Some(hwnd), TIMER_SHOT);
                if let Some(p) = DEBUG_SHOT.with(|d| d.borrow_mut().take()) {
                    save_shot(hwnd, &p);
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wparam.0 == TIMER_CURSOR => {
                ui.poll_cursor();
                ui.tick_bar();
                ui.tick_stats();
                let notice = c.stats().host_notice;
                if notice != ui.notice {
                    ui.notice = notice;
                    match notice {
                        NOTICE_NONE => println!("Input works again."),
                        _ => println!(
                            "The host can't take input right now (notice {notice}); see the host's window."
                        ),
                    }
                }
                Some(LRESULT(0))
            }
            WM_SIZE => {
                ui.layout();
                Some(LRESULT(0))
            }
            WM_TOGGLE_FULLSCREEN => {
                ui.toggle_fullscreen();
                Some(LRESULT(0))
            }
            WM_STREAM_CHANGED => {
                ui.layout();
                ui.update_bar();
                let i = c.info();
                println!(
                    "Now showing: {} at {}x{}, {}.",
                    mode_name(i.mode),
                    i.width,
                    i.height,
                    if i.chroma == CHROMA_444 {
                        "full colour 4:4:4"
                    } else {
                        "4:2:0"
                    }
                );
                Some(LRESULT(0))
            }
            WM_NEXT_MODE if c.host_caps() & HOST_CAP_VIEW_MODES == 0 => {
                println!("This host can only show its own screen (other views need a virtual display there).");
                Some(LRESULT(0))
            }
            WM_NEXT_MODE => {
                let next = (sh.mode.load(Ordering::Relaxed) + 1) % MODE_COUNT;
                sh.mode.store(next, Ordering::Relaxed);
                println!("Asking the host for: {} ...", mode_name(next));
                c.set_mode(next, sh.allow_444());
                Some(LRESULT(0))
            }
            WM_TOGGLE_444 => {
                let want = !sh.want_444.load(Ordering::Relaxed);
                sh.want_444.store(want, Ordering::Relaxed);
                if want && !sh.can_444.load(Ordering::Relaxed) {
                    println!("Full colour (4:4:4) isn't available on this PC; staying on 4:2:0.");
                } else {
                    println!("Asking the host for {} ...", if want { "full colour (4:4:4)" } else { "4:2:0" });
                }
                c.set_mode(sh.mode.load(Ordering::Relaxed), sh.allow_444());
                Some(LRESULT(0))
            }
            WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => {
                // Let go of everything so nothing stays held on the host.
                c.input_release_all();
                ui.ctrl = false;
                ui.alt = false;
                ui.shift = false;
                ui.buttons = 0;
                None
            }
            WM_CLOSE => {
                ui.sh.end("viewer closed");
                if ui.render_done {
                    let _ = DestroyWindow(hwnd);
                }
                Some(LRESULT(0))
            }
            WM_RENDER_DONE => {
                ui.render_done = true;
                None // destroy below, outside the borrow
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                Some(LRESULT(0))
            }
            _ => None,
        }
    })
    .flatten();
    if msg == WM_RENDER_DONE {
        let _ = DestroyWindow(hwnd);
        return LRESULT(0);
    }
    if msg == WM_DESTROY && handled.is_none() {
        PostQuitMessage(0);
    }
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
}

/// Sends keys to the host while the viewer window is in front, so Windows
/// shortcuts (Win, Alt+Tab, Alt+F4) go there too. Ctrl+Alt+Del can't be
/// caught; it stays with this PC.
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        // GLASS_DEBUG_INJECTED_KEYS=1: take scripted keys too (local checks).
        static TAKE_INJECTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let take =
            *TAKE_INJECTED.get_or_init(|| std::env::var_os("GLASS_DEBUG_INJECTED_KEYS").is_some());
        let ours = kb.dwExtraInfo == super::inject::GLASS_MARK;
        let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0 && (!take || ours);
        if down && !injected {
            KEYS_SEEN.fetch_add(1, Ordering::Relaxed);
        }
        let handled = if injected {
            Some(false)
        } else {
            with_ui(|ui| {
                // Ours if the viewer or one of its own pop-ups (bar, handle,
                // stats) is in front.
                let front = GetForegroundWindow();
                let mine = front == ui.hwnd || GetAncestor(front, GA_ROOTOWNER) == ui.hwnd;
                if !mine {
                    if down {
                        KEYS_NOT_FRONT.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut w) = KEYS_WENT_TO.try_lock() {
                            if w.is_empty() {
                                *w = describe_window(front);
                            }
                        }
                    }
                    return false;
                }
                let vk = kb.vkCode as u16;
                match vk {
                    0x10 | 0xA0 | 0xA1 => ui.shift = down,
                    0x11 | 0xA2 | 0xA3 => ui.ctrl = down,
                    0x12 | 0xA4 | 0xA5 => ui.alt = down,
                    _ => {}
                }
                if down && ui.ctrl && ui.alt && ui.shift {
                    let msg = if vk == VK_F.0 {
                        Some(WM_TOGGLE_FULLSCREEN)
                    } else if vk == VK_C.0 {
                        Some(WM_TOGGLE_444)
                    } else if vk == VK_S.0 {
                        let _ = PostMessageW(
                            Some(ui.hwnd),
                            bar::WM_BAR_COMMAND,
                            WPARAM(bar::ID_STATS),
                            LPARAM(0),
                        );
                        return true;
                    } else if vk == VK_R.0 {
                        Some(WM_NEXT_MODE)
                    } else if vk == VK_Q.0 || vk == VK_W.0 {
                        Some(WM_CLOSE)
                    } else {
                        None
                    };
                    if let Some(msg) = msg {
                        ui.sh.client.input_release_all();
                        let _ = PostMessageW(Some(ui.hwnd), msg, WPARAM(0), LPARAM(0));
                        return true;
                    }
                }
                let ext = kb.flags.0 & LLKHF_EXTENDED.0 != 0;
                let hid = win_scan_to_hid(kb.scanCode as u16, ext);
                if hid != 0 {
                    ui.sh
                        .client
                        .input_key(pc_key_for_mac(hid, ui.sh.rule), down);
                    if down {
                        ui.sh.keys_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
                true
            })
        };
        if handled.is_none() && down && !injected {
            KEYS_BUSY.fetch_add(1, Ordering::Relaxed);
        }
        let eaten = handled.unwrap_or(false);
        if eaten {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

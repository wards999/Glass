//! `glass connect`: the Windows viewer (M3b). Shows a paired host (a Mac or
//! another PC) full screen and sends this PC's mouse and keyboard to it.
//!
//! - Render thread: frames from core -> D3D11VA decode -> video processor ->
//!   flip-model swap chain (one frame of latency). Only the newest decoded
//!   picture is shown; older ones are "superseded".
//! - UI thread: the window, a low-level keyboard hook (so Win, Alt+Tab and
//!   friends go to the host while the window is in front), mouse messages,
//!   and the host's pointer shape as this PC's own cursor (no round trip).
//! - Local chords, never sent: Ctrl+Alt+Shift+F full screen/window,
//!   Ctrl+Alt+Shift+Q disconnect.
//!
//! LATER(M5): on-screen stats overlay; display-linked pacing.

use super::decode::Decoder;
use super::present::{fit, Presenter};
use super::{d3d, Res};
use glass_core::clock;
use glass_core::input::{BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT};
use glass_core::keymap::{pc_key_for_mac, win_scan_to_hid, PcModifierRule};
use glass_core::net::{Client, Wait};
use glass_core::protocol::{StreamInfo, NOTICE_NONE};
use glass_core::stats::{StatRing, Summary};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, EnumDisplaySettingsW, GetMonitorInfoW,
    MonitorFromPoint, MonitorFromWindow, ScreenToClient, BITMAPINFO, BITMAPV5HEADER, BI_BITFIELDS,
    DEVMODEW, DIB_RGB_COLORS, ENUM_CURRENT_SETTINGS, HGDIOBJ, MONITORINFO, MONITORINFOEXW,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_F, VK_Q, VK_W};
use windows::Win32::UI::WindowsAndMessaging::*;

/// How the viewer runs.
pub struct Options {
    pub windowed: bool,
    pub seconds: Option<f64>,
    pub rule: PcModifierRule,
}

const WM_RENDER_DONE: u32 = WM_APP + 1;
/// Posted by the keyboard hook: toggle full screen (outside the hook).
const WM_TOGGLE_FULLSCREEN: u32 = WM_APP + 2;
const TIMER_CURSOR: usize = 1;

/// Shared by the UI and render threads.
struct Shared {
    client: Client,
    info: StreamInfo,
    stop: AtomicBool,
    reason: Mutex<Option<String>>,
    /// Client size, `w << 32 | h` (UI -> render).
    size: AtomicU64,
    rule: PcModifierRule,
}

impl Shared {
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
    exact_present: bool,
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
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
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

/// The monitor under the pointer: (full rect, work area, "WxH @ Hz").
fn monitor_at_pointer() -> (RECT, RECT, String) {
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
        let desc = if EnumDisplaySettingsW(
            PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
        )
        .as_bool()
        {
            format!(
                "{}x{} @ {} Hz",
                dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency
            )
        } else {
            "unknown display".into()
        };
        (info.monitorInfo.rcMonitor, info.monitorInfo.rcWork, desc)
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

/// Shows `client` until it ends; returns the report.
pub fn run(client: Client, opts: Options, banner: &str) -> Res<String> {
    d3d::init_process();
    let gpu = d3d::Gpu::new_for_viewer()?;
    let info = client.info();
    let started = clock::now_us();
    let (mon, work, display) = monitor_at_pointer();
    let sh = Arc::new(Shared {
        client,
        info,
        stop: AtomicBool::new(false),
        reason: Mutex::new(None),
        size: AtomicU64::new(0),
        rule: opts.rule,
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
    let hook = unsafe {
        let inst: HINSTANCE = GetModuleHandleW(None)?.into();
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(inst), 0)?
    };
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hook);
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
        "  input: {} events in {} packets | keys {}",
        st.input_events,
        st.input_packets,
        match sh.rule {
            PcModifierRule::PcStyle => "PC-style (Ctrl -> Cmd, Win -> Ctrl)",
            PcModifierRule::MacStyle => "as labelled (Ctrl -> Ctrl, Win -> Cmd)",
        }
    );
    let _ = writeln!(
        out,
        "  decoder: D3D11VA hardware | present timing: {}",
        if r.exact_present {
            "DXGI frame statistics"
        } else {
            "estimated (no frame statistics)"
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
    Ok(out)
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
    let mut dec = Decoder::new(device)?;
    let size = sh.size.load(Ordering::Relaxed);
    let mut pres = Presenter::new(device, hwnd, ((size >> 32) as u32, size as u32))?;
    let mut pending: VecDeque<Pending> = VecDeque::with_capacity(16);
    let mut last_shown = 0u64;
    let mut last_input = 0u32;
    let mut stats_failures = 0u32;
    let end = seconds.map(|s| clock::now_us() + (s * 1e6) as u64);
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

        // Decode everything waiting; show only the newest picture.
        let mut newest: Option<(super::decode::Picture, u64, u32)> = None;
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
                    let res = dec.decode(data);
                    c.release_frame();
                    r.decode.push(span(clock::now_us(), t0));
                    match res {
                        Ok(Some(pic)) => {
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
                        }
                    }
                    timeout = Duration::ZERO;
                }
                Wait::Timeout => break,
                Wait::Disconnected => {
                    sh.end("the host ended the session");
                    break;
                }
            }
        }

        if let Some((pic, capture, input_seq)) = newest {
            r.wait.push(pres.wait_ready(100) as u32);
            pres.draw(&pic)?;
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
        "Glass - {}  (Ctrl+Alt+Shift+F full screen, Ctrl+Alt+Shift+Q disconnect)",
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
        })
    });
    with_ui(|ui| ui.layout());
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    SetTimer(Some(hwnd), TIMER_CURSOR, 15, None);
    Ok(hwnd)
}

impl Ui {
    /// Recomputes the picture area after a size change.
    fn layout(&mut self) {
        let (w, h) = client_size(self.hwnd);
        self.sh
            .size
            .store((w as u64) << 32 | h as u64, Ordering::Relaxed);
        self.dest = fit(
            (w, h),
            (self.sh.info.width as u32, self.sh.info.height as u32),
        );
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
            let scale = self.dest_width.max(1) as f64 / self.sh.info.width.max(1) as f64;
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
            WM_TIMER if wparam.0 == TIMER_CURSOR => {
                ui.poll_cursor();
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
        let injected = kb.flags.0 & LLKHF_INJECTED.0 != 0;
        let eaten = !injected
            && with_ui(|ui| {
                if GetForegroundWindow() != ui.hwnd {
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
                    let chord = if vk == VK_F.0 {
                        Some(true)
                    } else if vk == VK_Q.0 || vk == VK_W.0 {
                        Some(false)
                    } else {
                        None
                    };
                    if let Some(full) = chord {
                        ui.sh.client.input_release_all();
                        if full {
                            let _ = PostMessageW(
                                Some(ui.hwnd),
                                WM_TOGGLE_FULLSCREEN,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        } else {
                            let _ = PostMessageW(Some(ui.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                        return true;
                    }
                }
                let ext = kb.flags.0 & LLKHF_EXTENDED.0 != 0;
                let hid = win_scan_to_hid(kb.scanCode as u16, ext);
                if hid != 0 {
                    ui.sh
                        .client
                        .input_key(pc_key_for_mac(hid, ui.sh.rule), down);
                }
                true
            })
            .unwrap_or(false);
        if eaten {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

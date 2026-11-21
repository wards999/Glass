//! The viewer's drop-down control bar (M3d): click the floating handle near
//! the top of the viewer and a slim bar appears with one-click switches for the view and the
//! colour mode, full screen, pin and disconnect, plus a status line. It never
//! takes the keyboard (the viewer stays in front, so keys still go to the
//! host), and clicks on it never reach the host.

use super::Res;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, GetSysColorBrush, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_BTNFACE,
    DEFAULT_CHARSET, DEFAULT_PITCH, FF_DONTCARE, FW_NORMAL, HFONT, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the viewer window when a bar control is clicked; `wparam` = id.
pub const WM_BAR_COMMAND: u32 = WM_APP + 6;

/// Control ids. Views are `ID_VIEW + mode` (protocol `MODE_*`).
pub const ID_VIEW: usize = 100;
pub const ID_420: usize = 110;
pub const ID_444: usize = 111;
pub const ID_FULLSCREEN: usize = 120;
pub const ID_PIN: usize = 121;
pub const ID_STATS: usize = 123;
pub const ID_DISCONNECT: usize = 122;
const ID_STATUS: usize = 130;

const BS_PUSHLIKE: u32 = 0x1000;
const BS_AUTORADIOBUTTON: u32 = 0x9;
const BS_AUTOCHECKBOX: u32 = 0x3;
const BS_PUSHBUTTON: u32 = 0x0;
const BM_SETCHECK: u32 = 0xF1;
const SS_CENTERIMAGE: u32 = 0x200;

pub struct Bar {
    hwnd: HWND,
    pub visible: bool,
    pub pinned: bool,
}

unsafe extern "system" fn bar_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // Clicking the bar must not take focus from the viewer.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
                let _ = PostMessageW(Some(owner), WM_BAR_COMMAND, WPARAM(id), LPARAM(0));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl Bar {
    /// Creates the (hidden) bar for viewer window `owner`.
    pub fn new(owner: HWND) -> Res<Self> {
        unsafe {
            let inst: HINSTANCE = GetModuleHandleW(None)?.into();
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(bar_proc),
                hInstance: inst,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
                lpszClassName: w!("GlassBar"),
                ..Default::default()
            };
            RegisterClassExW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("GlassBar"),
                w!("Glass"),
                WS_POPUP | WS_BORDER,
                0,
                0,
                100,
                30,
                Some(owner),
                None,
                Some(inst),
                None,
            )?;
            let bar = Self {
                hwnd,
                visible: false,
                pinned: false,
            };
            bar.build(inst)?;
            Ok(bar)
        }
    }

    fn scale(&self) -> f64 {
        unsafe { GetDpiForWindow(self.hwnd).max(96) as f64 / 96.0 }
    }

    unsafe fn build(&self, inst: HINSTANCE) -> Res<()> {
        let k = self.scale();
        let px = |v: i32| (v as f64 * k).round() as i32;
        let font: HFONT = CreateFontW(
            -px(13),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Segoe UI"),
        );
        // (label, id, width at 100 %, style, starts a group, gap before)
        let views = [
            ("Fit", ID_VIEW + 1, 52),
            ("More space", ID_VIEW + 2, 96),
            ("Larger", ID_VIEW + 3, 66),
            ("Mac's screen", ID_VIEW, 104),
        ];
        let x = std::cell::Cell::new(px(8));
        let (y, h) = (px(5), px(26));
        let add = |label: &str, id: usize, width: i32, style: u32, group: bool| -> Res<()> {
            let text = wide(label);
            let mut s = WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style);
            if group {
                s |= WS_GROUP;
            }
            let c = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                PCWSTR(text.as_ptr()),
                s,
                x.get(),
                y,
                px(width),
                h,
                Some(self.hwnd),
                Some(HMENU(id as *mut _)),
                Some(inst),
                None,
            )?;
            SendMessageW(
                c,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
            x.set(x.get() + px(width) + px(4));
            Ok(())
        };
        for (i, (label, id, width)) in views.iter().enumerate() {
            add(label, *id, *width, BS_AUTORADIOBUTTON | BS_PUSHLIKE, i == 0)?;
        }
        x.set(x.get() + px(10));
        add(
            "Fast (4:2:0)",
            ID_420,
            92,
            BS_AUTORADIOBUTTON | BS_PUSHLIKE,
            true,
        )?;
        add(
            "Full colour (4:4:4)",
            ID_444,
            134,
            BS_AUTORADIOBUTTON | BS_PUSHLIKE,
            false,
        )?;
        x.set(x.get() + px(10));
        add("Full screen", ID_FULLSCREEN, 90, BS_PUSHBUTTON, true)?;
        add("Stats", ID_STATS, 56, BS_AUTOCHECKBOX | BS_PUSHLIKE, true)?;
        add("Pin", ID_PIN, 44, BS_AUTOCHECKBOX | BS_PUSHLIKE, true)?;
        add("Disconnect", ID_DISCONNECT, 90, BS_PUSHBUTTON, true)?;
        x.set(x.get() + px(8));
        let status = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_CENTERIMAGE),
            x.get(),
            y,
            px(330),
            h,
            Some(self.hwnd),
            Some(HMENU(ID_STATUS as *mut _)),
            Some(inst),
            None,
        )?;
        SendMessageW(
            status,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        let width = x.get() + px(330) + px(8);
        let _ = SetWindowPos(
            self.hwnd,
            None,
            0,
            0,
            width,
            px(36),
            SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER,
        );
        Ok(())
    }

    fn check(&self, id: usize, on: bool) {
        unsafe {
            if let Ok(c) = GetDlgItem(Some(self.hwnd), id as i32) {
                SendMessageW(c, BM_SETCHECK, Some(WPARAM(on as usize)), None);
            }
        }
    }

    /// Shows which view and colour mode are on, what full screen does, and
    /// a status line. Greys out what the host or this PC can't do: the
    /// "fit" views without `views_ok`, 4:4:4 without `can_444`.
    /// `host_screen` labels the host's-own-screen view ("Mac's screen").
    #[allow(clippy::too_many_arguments)]
    pub fn set_state(
        &self,
        mode: u8,
        full_colour: bool,
        can_444: bool,
        views_ok: bool,
        fullscreen: bool,
        stats: bool,
        status: &str,
        host_screen: &str,
    ) {
        for m in 0..4u8 {
            self.check(ID_VIEW + m as usize, m == mode);
        }
        self.check(ID_420, !full_colour);
        self.check(ID_444, full_colour);
        self.check(ID_PIN, self.pinned);
        self.check(ID_STATS, stats);
        unsafe {
            if let Ok(c) = GetDlgItem(Some(self.hwnd), ID_444 as i32) {
                let _ = EnableWindow(c, can_444);
            }
            for m in 1..4usize {
                if let Ok(c) = GetDlgItem(Some(self.hwnd), (ID_VIEW + m) as i32) {
                    let _ = EnableWindow(c, views_ok);
                }
            }
            if let Ok(c) = GetDlgItem(Some(self.hwnd), ID_VIEW as i32) {
                let t = wide(host_screen);
                let _ = SetWindowTextW(c, PCWSTR(t.as_ptr()));
            }
            if let Ok(c) = GetDlgItem(Some(self.hwnd), ID_FULLSCREEN as i32) {
                let t = wide(if fullscreen { "Window" } else { "Full screen" });
                let _ = SetWindowTextW(c, PCWSTR(t.as_ptr()));
            }
            if let Ok(c) = GetDlgItem(Some(self.hwnd), ID_STATUS as i32) {
                let t = wide(status);
                let _ = SetWindowTextW(c, PCWSTR(t.as_ptr()));
            }
        }
    }

    /// Shows the bar at the top of `client` (screen coordinates), centred
    /// on `centre_x` (the handle) and kept inside the client area.
    pub fn show(&mut self, client: RECT, centre_x: i32) {
        unsafe {
            let mut r = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut r);
            let w = r.right - r.left;
            let x = (centre_x - w / 2).min(client.right - w);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x.max(client.left),
                client.top,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        self.visible = true;
    }

    pub fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.visible = false;
    }

    /// The bar's rectangle on screen.
    pub fn rect(&self) -> RECT {
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        r
    }
}

impl Drop for Bar {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// The bar's floating handle: a small rounded tab near the top of the
/// viewer with a chevron. Faint until hovered; a click toggles the bar; it can
/// be dragged sideways (the position is kept). It never takes the keyboard,
/// and clicks on it never reach the host.
pub const ID_HANDLE: usize = 140;
/// Posted after the handle was dragged (the viewer saves its position).
pub const ID_HANDLE_MOVED: usize = 141;

pub struct Handle {
    hwnd: HWND,
    /// Centre of the handle across the client area, 0..1.
    pub frac: f64,
    placed: RECT,
    pub visible: bool,
}

/// Drag state (one viewer at a time): (cursor x at press, handle left at
/// press, moved), plus hover and the client area it must stay inside.
static DRAG: std::sync::Mutex<Option<(i32, i32, bool)>> = std::sync::Mutex::new(None);
static HOVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static BOUNDS: std::sync::Mutex<(i32, i32)> = std::sync::Mutex::new((0, 0));

const WM_MOUSELEAVE_: u32 = 0x02A3;
const ALPHA_IDLE: u8 = 205;
const ALPHA_HOVER: u8 = 255;

unsafe fn set_alpha(hwnd: HWND, a: u8) {
    let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), a, LWA_ALPHA);
}

unsafe extern "system" fn handle_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    };
    let notify = |id: usize| {
        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            let _ = PostMessageW(Some(owner), WM_BAR_COMMAND, WPARAM(id), LPARAM(0));
        }
    };
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR => {
            if let Ok(c) = LoadCursorW(None, IDC_HAND) {
                SetCursor(Some(c));
            }
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            if !HOVER.swap(true, Ordering::Relaxed) {
                set_alpha(hwnd, ALPHA_HOVER);
                let mut t = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut t);
            }
            let mut drag = DRAG.lock().unwrap();
            if let Some((x0, left0, moved)) = drag.as_mut() {
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                let dx = p.x - *x0;
                if dx.abs() > 3 {
                    *moved = true;
                }
                if *moved {
                    let mut r = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut r);
                    let w = r.right - r.left;
                    let (lo, hi) = *BOUNDS.lock().unwrap();
                    let x = (*left0 + dx).clamp(lo, (hi - w).max(lo));
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        x,
                        r.top,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_ => {
            HOVER.store(false, Ordering::Relaxed);
            if DRAG.lock().unwrap().is_none() {
                set_alpha(hwnd, ALPHA_IDLE);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            *DRAG.lock().unwrap() = Some((p.x, r.left, false));
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let d = DRAG.lock().unwrap().take();
            let _ = ReleaseCapture();
            if !HOVER.load(Ordering::Relaxed) {
                set_alpha(hwnd, ALPHA_IDLE);
            }
            match d {
                Some((_, _, true)) => notify(ID_HANDLE_MOVED),
                Some(_) => notify(ID_HANDLE),
                None => {}
            }
            LRESULT(0)
        }
        WM_PAINT => {
            use windows::Win32::Foundation::COLORREF;
            use windows::Win32::Graphics::Gdi::{
                BeginPaint, CreatePen, CreateRoundRectRgn, CreateSolidBrush, DeleteObject,
                EndPaint, FillRect, FrameRgn, LineTo, MoveToEx, SelectObject, PAINTSTRUCT,
                PS_SOLID,
            };
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            let k = GetDpiForWindow(hwnd).max(96) as f64 / 96.0;
            // COLORREF is 0x00BBGGRR: rgb(27, 31, 37), the app's dark card.
            let bg = CreateSolidBrush(COLORREF(0x0038_2E28)); // rgb(40, 46, 56)
            FillRect(dc, &r, bg);
            let _ = DeleteObject(bg.into());
            // A light outline so it shows on dark pictures too.
            let edge = CreateSolidBrush(COLORREF(0x0096_8C82)); // rgb(130, 140, 150)
            let rgn = CreateRoundRectRgn(0, 0, r.right, r.bottom, r.bottom, r.bottom);
            let _ = FrameRgn(dc, rgn, edge, 1, 1);
            let _ = DeleteObject(rgn.into());
            let _ = DeleteObject(edge.into());
            let pen = CreatePen(PS_SOLID, (2.0 * k).round() as i32, COLORREF(0x00F5_F1EE));
            let old = SelectObject(dc, pen.into());
            let (cx, cy) = ((r.right / 2) as f64, (r.bottom / 2) as f64);
            let pt =
                |dx: f64, dy: f64| ((cx + dx * k).round() as i32, (cy + dy * k).round() as i32);
            let (a, b, c) = (pt(-6.0, -2.5), pt(0.0, 3.5), pt(6.0, -2.5));
            let _ = MoveToEx(dc, a.0, a.1, None);
            let _ = LineTo(dc, b.0, b.1);
            let _ = LineTo(dc, c.0, c.1);
            SelectObject(dc, old);
            let _ = DeleteObject(pen.into());
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

impl Handle {
    /// Creates the (hidden) handle for viewer window `owner`. It is owned,
    /// not topmost: it stays above the viewer and goes away with it.
    pub fn new(owner: HWND, frac: f64) -> Res<Self> {
        unsafe {
            use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn};
            let inst: HINSTANCE = GetModuleHandleW(None)?.into();
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(handle_proc),
                hInstance: inst,
                lpszClassName: w!("GlassBarHandle"),
                ..Default::default()
            };
            RegisterClassExW(&class);
            let k = GetDpiForWindow(owner).max(96) as f64 / 96.0;
            let (w, h) = ((60.0 * k).round() as i32, (24.0 * k).round() as i32);
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
                w!("GlassBarHandle"),
                w!("Glass controls"),
                WS_POPUP,
                0,
                0,
                w,
                h,
                Some(owner),
                None,
                Some(inst),
                None,
            )?;
            set_alpha(hwnd, ALPHA_IDLE);
            let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, h, h);
            let _ = SetWindowRgn(hwnd, Some(rgn), true);
            Ok(Self {
                hwnd,
                frac: frac.clamp(0.0, 1.0),
                placed: RECT::default(),
                visible: false,
            })
        }
    }

    /// Puts the handle near the top of `client` (screen coordinates) at
    /// `frac`, and shows it. Cheap when nothing changed.
    pub fn place(&mut self, client: RECT) {
        if DRAG.lock().unwrap().is_some() {
            return;
        }
        let r = self.rect();
        let w = r.right - r.left;
        let k = unsafe { GetDpiForWindow(self.hwnd).max(96) as f64 / 96.0 };
        let span = (client.right - client.left - w).max(0);
        let x = client.left + (span as f64 * self.frac).round() as i32;
        let y = client.top + (6.0 * k).round() as i32;
        *BOUNDS.lock().unwrap() = (client.left, client.right);
        if self.visible && client == self.placed && r.left == x && r.top == y {
            return;
        }
        self.placed = client;
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        self.visible = true;
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
        }
    }

    /// After a drag: work out `frac` from where it was left.
    pub fn moved(&mut self, client: RECT) {
        let r = self.rect();
        let span = (client.right - client.left - (r.right - r.left)).max(1);
        self.frac = ((r.left - client.left) as f64 / span as f64).clamp(0.0, 1.0);
    }

    pub fn rect(&self) -> RECT {
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        r
    }

    /// The centre of the handle on screen.
    pub fn centre_x(&self) -> i32 {
        let r = self.rect();
        (r.left + r.right) / 2
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

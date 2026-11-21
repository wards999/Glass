//! The viewer's drop-down control bar (M3d), drawn with Direct2D and
//! DirectWrite in the Glass window's dark style: click the floating handle
//! near the top of the viewer and a slim bar appears with one-click switches for the view and the
//! colour mode, full screen, pin and disconnect, plus a status line. It never
//! takes the keyboard (the viewer stays in front, so keys still go to the
//! host), and clicks on it never reach the host.

use super::Res;
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, D2D1_DRAW_TEXT_OPTIONS_CLIP,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
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

/// One clickable thing on the bar.
struct Item {
    id: usize,
    label: String,
    /// 1 = the view switch, 2 = the colour switch (segmented), 0 = a button.
    group: u8,
    on: bool,
    enabled: bool,
    danger: bool,
    x: f32,
    w: f32,
}

/// What the bar draws with, and what it shows (UI thread only).
struct State {
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    font: IDWriteTextFormat,
    status_font: IDWriteTextFormat,
    rt: Option<ID2D1HwndRenderTarget>,
    k: f32,
    items: Vec<Item>,
    status: String,
    status_x: f32,
    hover: Option<usize>,
    pressed: Option<usize>,
    tracking: bool,
}

/// The bar's look (dark, like the Glass window's dark theme).
mod look {
    pub const BG: (u8, u8, u8) = (27, 31, 37);
    pub const TRACK: (u8, u8, u8) = (37, 42, 50);
    pub const HOVER: (u8, u8, u8) = (50, 57, 68);
    pub const ON: (u8, u8, u8) = (72, 81, 96);
    pub const ACCENT: (u8, u8, u8) = (76, 141, 235);
    pub const TEXT: (u8, u8, u8) = (205, 210, 218);
    pub const STRONG: (u8, u8, u8) = (245, 247, 250);
    pub const WEAK: (u8, u8, u8) = (139, 149, 163);
    pub const DISABLED: (u8, u8, u8) = (88, 96, 108);
    pub const DANGER: (u8, u8, u8) = (240, 110, 110);
    pub const EDGE: u32 = 0x0048_3E37; // COLORREF 0x00BBGGRR: rgb(55, 62, 72)
}

fn colour((r, g, b): (u8, u8, u8)) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

fn wide_no_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

impl State {
    fn text_width(&self, s: &str, f: &IDWriteTextFormat) -> f32 {
        unsafe {
            let t = wide_no_nul(s);
            let Ok(layout) = self.dw.CreateTextLayout(&t, f, 10_000.0, 1_000.0) else {
                return 8.0 * s.len() as f32;
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut m);
            m.widthIncludingTrailingWhitespace
        }
    }

    /// Positions everything and returns the bar's size in pixels.
    fn layout(&mut self) -> (i32, i32) {
        let k = self.k;
        let mut x = 10.0 * k;
        let mut last_group = 255u8;
        let widths: Vec<f32> = self
            .items
            .iter()
            .map(|i| self.text_width(&i.label, &self.font))
            .collect();
        for (i, item) in self.items.iter_mut().enumerate() {
            let pad = if item.group != 0 { 12.0 } else { 14.0 } * k;
            if i > 0 {
                x += if item.group != 0 && item.group == last_group {
                    2.0 * k
                } else {
                    12.0 * k
                };
            }
            item.x = x;
            item.w = widths[i] + 2.0 * pad;
            x += item.w;
            last_group = item.group;
        }
        self.status_x = x + 14.0 * k;
        let status_w = self
            .text_width(&self.status, &self.status_font)
            .min(380.0 * k);
        (
            (self.status_x + status_w + 14.0 * k).ceil() as i32,
            (44.0 * k).round() as i32,
        )
    }

    fn item_at(&self, x: f32, y: f32) -> Option<usize> {
        let (top, h) = (7.0 * self.k, 30.0 * self.k);
        if y < top || y > top + h {
            return None;
        }
        self.items
            .iter()
            .position(|i| i.enabled && x >= i.x && x <= i.x + i.w)
    }

    unsafe fn paint(&mut self, hwnd: HWND) {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let size = D2D_SIZE_U {
            width: rc.right.max(1) as u32,
            height: rc.bottom.max(1) as u32,
        };
        if self.rt.is_none() {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: size,
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            self.rt = self.d2d.CreateHwndRenderTarget(&props, &hprops).ok();
        }
        let Some(rt) = self.rt.clone() else { return };
        let _ = rt.Resize(&size);
        let k = self.k;
        rt.BeginDraw();
        rt.Clear(Some(&colour(look::BG)));
        let brush = |c| rt.CreateSolidColorBrush(&colour(c), None).ok();
        let fill = |l: f32, t: f32, r: f32, b: f32, radius: f32, c| {
            if let Some(br) = brush(c) {
                let rr = D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: l,
                        top: t,
                        right: r,
                        bottom: b,
                    },
                    radiusX: radius,
                    radiusY: radius,
                };
                rt.FillRoundedRectangle(&rr, &br);
            }
        };
        let (top, h) = (7.0 * k, 30.0 * k);
        // The segmented tracks behind each switch.
        for g in [1u8, 2] {
            let mut it = self.items.iter().filter(|i| i.group == g);
            if let Some(first) = it.next() {
                let last = it.next_back().unwrap_or(first);
                fill(
                    first.x - 3.0 * k,
                    top - 3.0 * k,
                    last.x + last.w + 3.0 * k,
                    top + h + 3.0 * k,
                    9.0 * k,
                    look::TRACK,
                );
            }
        }
        for (i, item) in self.items.iter().enumerate() {
            let hover = self.hover == Some(i) && item.enabled;
            let (bg, fg) = if item.group != 0 {
                if item.on {
                    (Some(look::ON), look::STRONG)
                } else if hover {
                    (Some(look::HOVER), look::STRONG)
                } else {
                    (
                        None,
                        if item.enabled {
                            look::TEXT
                        } else {
                            look::DISABLED
                        },
                    )
                }
            } else if item.on {
                (Some(look::ACCENT), look::STRONG)
            } else {
                (
                    Some(if hover { look::HOVER } else { look::TRACK }),
                    if item.danger {
                        look::DANGER
                    } else {
                        look::TEXT
                    },
                )
            };
            if let Some(bg) = bg {
                fill(item.x, top, item.x + item.w, top + h, 7.0 * k, bg);
            }
            if let Some(br) = brush(fg) {
                let t = wide_no_nul(&item.label);
                let r = D2D_RECT_F {
                    left: item.x,
                    top,
                    right: item.x + item.w,
                    bottom: top + h,
                };
                rt.DrawText(
                    &t,
                    &self.font,
                    &r,
                    &br,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
        if let Some(br) = brush(look::WEAK) {
            let t = wide_no_nul(&self.status);
            let r = D2D_RECT_F {
                left: self.status_x,
                top,
                right: rc.right as f32 - 10.0 * k,
                bottom: top + h,
            };
            rt.DrawText(
                &t,
                &self.status_font,
                &r,
                &br,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
        if rt.EndDraw(None, None).is_err() {
            self.rt = None; // device lost: make a new one next time
        }
    }
}

pub struct Bar {
    hwnd: HWND,
    state: *mut State,
    pub visible: bool,
    pub pinned: bool,
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a mut State> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    p.as_mut()
}

unsafe extern "system" fn bar_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    };
    let xy = || {
        (
            (lparam.0 & 0xFFFF) as i16 as f32,
            ((lparam.0 >> 16) & 0xFFFF) as i16 as f32,
        )
    };
    match msg {
        // Clicking the bar must not take focus from the viewer.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            if let Some(s) = state(hwnd) {
                s.paint(hwnd);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let hand = state(hwnd).is_some_and(|s| s.hover.is_some());
            if let Ok(c) = LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }) {
                SetCursor(Some(c));
            }
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            if let Some(s) = state(hwnd) {
                if !s.tracking {
                    s.tracking = true;
                    let mut t = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut t);
                }
                let (x, y) = xy();
                let h = s.item_at(x, y);
                if h != s.hover {
                    s.hover = h;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_ => {
            if let Some(s) = state(hwnd) {
                s.tracking = false;
                if s.hover.take().is_some() {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if let Some(s) = state(hwnd) {
                let (x, y) = xy();
                s.pressed = s.item_at(x, y);
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            if let Some(s) = state(hwnd) {
                let (x, y) = xy();
                let at = s.item_at(x, y);
                if let (Some(p), Some(a)) = (s.pressed.take(), at) {
                    if p == a {
                        let id = s.items[a].id;
                        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
                            let _ =
                                PostMessageW(Some(owner), WM_BAR_COMMAND, WPARAM(id), LPARAM(0));
                        }
                    }
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
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
                lpszClassName: w!("GlassBar"),
                ..Default::default()
            };
            RegisterClassExW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("GlassBar"),
                w!("Glass"),
                WS_POPUP,
                0,
                0,
                100,
                44,
                Some(owner),
                None,
                Some(inst),
                None,
            )?;
            // Rounded corners and a soft edge where Windows offers them (11).
            let round = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &round as *const _ as *const core::ffi::c_void,
                std::mem::size_of_val(&round) as u32,
            );
            let edge = look::EDGE;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &edge as *const _ as *const core::ffi::c_void,
                4,
            );
            let k = GetDpiForWindow(owner).max(96) as f32 / 96.0;
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let format = |weight, size: f32, centre: bool| -> Res<IDWriteTextFormat> {
                let f = dw.CreateTextFormat(
                    w!("Segoe UI Variable Text"),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size * k,
                    w!("en-us"),
                )?;
                f.SetTextAlignment(if centre {
                    DWRITE_TEXT_ALIGNMENT_CENTER
                } else {
                    DWRITE_TEXT_ALIGNMENT_LEADING
                })?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                Ok(f)
            };
            let font = format(DWRITE_FONT_WEIGHT_SEMI_BOLD, 13.0, true)?;
            let status_font = format(DWRITE_FONT_WEIGHT_NORMAL, 12.5, false)?;
            let item = |id, label: &str, group, danger| Item {
                id,
                label: label.into(),
                group,
                on: false,
                enabled: true,
                danger,
                x: 0.0,
                w: 0.0,
            };
            let items = vec![
                item(ID_VIEW + 1, "Fit", 1, false),
                item(ID_VIEW + 2, "More space", 1, false),
                item(ID_VIEW + 3, "Larger", 1, false),
                item(ID_VIEW, "Host's screen", 1, false),
                item(ID_420, "Fast \u{00B7} 4:2:0", 2, false),
                item(ID_444, "Full colour \u{00B7} 4:4:4", 2, false),
                item(ID_FULLSCREEN, "Full screen", 0, false),
                item(ID_STATS, "Stats", 0, false),
                item(ID_PIN, "Pin", 0, false),
                item(ID_DISCONNECT, "Disconnect", 0, true),
            ];
            let st = Box::new(State {
                d2d,
                dw,
                font,
                status_font,
                rt: None,
                k,
                items,
                status: String::new(),
                status_x: 0.0,
                hover: None,
                pressed: None,
                tracking: false,
            });
            let state = Box::into_raw(st);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            let bar = Self {
                hwnd,
                state,
                visible: false,
                pinned: false,
            };
            bar.relayout();
            Ok(bar)
        }
    }

    fn relayout(&self) {
        unsafe {
            let s = &mut *self.state;
            let (w, h) = s.layout();
            let mut r = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut r);
            if r.right - r.left != w || r.bottom - r.top != h {
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    w,
                    h,
                    SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Shows which view and colour mode are on, what full screen does,
    /// whether stats and pin are on, and a status line. Greys out what the
    /// host or this PC can't do: the "fit" views without `views_ok`, 4:4:4
    /// without `can_444`. `host_screen` labels the host's-own-screen view
    /// ("Mac's screen").
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
        let s = unsafe { &mut *self.state };
        for i in &mut s.items {
            match i.id {
                id if (ID_VIEW..ID_VIEW + 4).contains(&id) => {
                    let m = (id - ID_VIEW) as u8;
                    i.on = m == mode;
                    i.enabled = m == 0 || views_ok;
                    if m == 0 {
                        i.label = host_screen.into();
                    }
                }
                ID_420 => i.on = !full_colour,
                ID_444 => {
                    i.on = full_colour;
                    i.enabled = can_444;
                }
                ID_FULLSCREEN => i.label = if fullscreen { "Window" } else { "Full screen" }.into(),
                ID_STATS => i.on = stats,
                ID_PIN => i.on = self.pinned,
                _ => {}
            }
        }
        s.status = status.into();
        self.relayout();
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
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
            drop(Box::from_raw(self.state));
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

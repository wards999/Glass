//! The viewer's on-screen stats (M3d): a see-through box at the top left
//! with live numbers, as on the Mac viewer. Off unless chosen (bar "Stats" or
//! Ctrl+Alt+Shift+S; kept). Clicks go through it to the host, and it never
//! takes the keyboard. The render thread builds the text four times a
//! second while it's shown; nothing per frame.

use super::Res;
use std::cell::RefCell;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC, InvalidateRect, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DT_CALCRECT, DT_LEFT, DT_NOPREFIX, FF_MODERN, FIXED_PITCH, FW_NORMAL, HFONT,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

thread_local! {
    /// The text being shown (UTF-16, no NUL) and the font (UI thread only).
    static TEXT: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
    static FONT: RefCell<Option<HFONT>> = const { RefCell::new(None) };
}

fn pad(hwnd: HWND) -> i32 {
    let k = unsafe { GetDpiForWindow(hwnd).max(96) as f64 / 96.0 };
    (8.0 * k).round() as i32
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => LRESULT(-1), // HTTRANSPARENT: clicks go to the viewer
        // Painted whole (below): no erase first, which showed as a flicker
        // every time the numbers changed (m5c-test3).
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let screen = BeginPaint(hwnd, &mut ps);
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            // Drawn off-screen, then copied in one go.
            let dc = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, r.right, r.bottom);
            let old_bmp = SelectObject(dc, bmp.into());
            let bg = CreateSolidBrush(COLORREF(0));
            FillRect(dc, &r, bg);
            let _ = DeleteObject(bg.into());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
            let old = FONT.with(|f| f.borrow().map(|f| SelectObject(dc, f.into())));
            let p = pad(hwnd);
            let mut tr = RECT {
                left: p,
                top: p,
                right: r.right - p,
                bottom: r.bottom - p,
            };
            TEXT.with(|t| {
                let mut t = t.borrow_mut();
                if !t.is_empty() {
                    DrawTextW(dc, &mut t, &mut tr, DT_LEFT | DT_NOPREFIX);
                }
            });
            if let Some(o) = old {
                SelectObject(dc, o);
            }
            let _ = BitBlt(screen, 0, 0, r.right, r.bottom, Some(dc), 0, 0, SRCCOPY);
            SelectObject(dc, old_bmp);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(dc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub struct StatsBox {
    hwnd: HWND,
    pub visible: bool,
    at: (i32, i32),
}

impl StatsBox {
    /// Creates the (hidden) box for viewer window `owner`.
    pub fn new(owner: HWND) -> Res<Self> {
        unsafe {
            let inst: HINSTANCE = GetModuleHandleW(None)?.into();
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(proc),
                hInstance: inst,
                lpszClassName: w!("GlassStats"),
                ..Default::default()
            };
            RegisterClassExW(&class);
            let k = GetDpiForWindow(owner).max(96) as f64 / 96.0;
            let font = CreateFontW(
                -((12.0 * k).round() as i32),
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
                (FIXED_PITCH.0 | FF_MODERN.0) as u32,
                w!("Consolas"),
            );
            FONT.with(|f| *f.borrow_mut() = Some(font));
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                w!("GlassStats"),
                w!("Glass stats"),
                WS_POPUP,
                0,
                0,
                100,
                40,
                Some(owner),
                None,
                Some(inst),
                None,
            )?;
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 175, LWA_ALPHA);
            let b = Self {
                hwnd,
                visible: false,
                at: (i32::MIN, i32::MIN),
            };
            b.set_text("Stats: waiting for frames \u{2026}");
            Ok(b)
        }
    }

    /// New text: resizes the box to fit and repaints it.
    pub fn set_text(&self, text: &str) {
        let new: Vec<u16> = text.encode_utf16().collect();
        if TEXT.with(|t| *t.borrow() == new) {
            return;
        }
        TEXT.with(|t| *t.borrow_mut() = new);
        unsafe {
            let dc = GetDC(Some(self.hwnd));
            let old = FONT.with(|f| f.borrow().map(|f| SelectObject(dc, f.into())));
            let mut r = RECT::default();
            TEXT.with(|t| {
                let mut t = t.borrow_mut();
                DrawTextW(dc, &mut t, &mut r, DT_LEFT | DT_NOPREFIX | DT_CALCRECT);
            });
            if let Some(o) = old {
                SelectObject(dc, o);
            }
            ReleaseDC(Some(self.hwnd), dc);
            let p = pad(self.hwnd);
            let (w, h) = (r.right + 2 * p, r.bottom + 2 * p);
            let mut now = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut now);
            if (now.right - now.left, now.bottom - now.top) != (w, h) {
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    w,
                    h,
                    SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOREDRAW,
                );
            }
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Shows it at the top left of `client` (screen coordinates).
    pub fn place(&mut self, client: RECT) {
        let p = pad(self.hwnd) * 3 / 2;
        let at = (client.left + p, client.top + p);
        if self.visible && at == self.at {
            return;
        }
        self.at = at;
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                at.0,
                at.1,
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
}

impl Drop for StatsBox {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        FONT.with(|f| {
            if let Some(font) = f.borrow_mut().take() {
                unsafe {
                    let _ = DeleteObject(font.into());
                }
            }
        });
    }
}

//! The viewer's drop-down control bar (M3d): move the pointer to the top
//! edge and a slim bar appears with one-click switches for the view and the
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
    /// a status line; greys out 4:4:4 if this PC can't decode it.
    pub fn set_state(
        &self,
        mode: u8,
        full_colour: bool,
        can_444: bool,
        fullscreen: bool,
        status: &str,
    ) {
        for m in 0..4u8 {
            self.check(ID_VIEW + m as usize, m == mode);
        }
        self.check(ID_420, !full_colour);
        self.check(ID_444, full_colour);
        self.check(ID_PIN, self.pinned);
        unsafe {
            if let Ok(c) = GetDlgItem(Some(self.hwnd), ID_444 as i32) {
                let _ = EnableWindow(c, can_444);
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

    /// Shows the bar centred at the top of `client` (screen coordinates).
    pub fn show(&mut self, client: RECT) {
        unsafe {
            let mut r = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut r);
            let w = r.right - r.left;
            let x = client.left + ((client.right - client.left) - w) / 2;
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

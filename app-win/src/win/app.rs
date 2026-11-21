//! The Glass window on Windows (M3d): what you get by double-clicking
//! Glass.exe. A list of Glass machines on the network (and paired ones),
//! Connect, Share this PC, pairing PINs in dialogs, paired devices with
//! Forget, session results with Copy, and a tray icon. The command line
//! (`glass host`, `glass connect`, ...) stays for tests and CI.
//!
//! Threads: this window's thread runs the UI. Connecting, pairing and each
//! viewer session run on worker threads (the viewer opens its own window);
//! sharing runs `host_with` on its own thread. Workers report back with
//! posted messages carrying boxed values.

use super::{browse, viewer, Res};
use glass_core::identity::{fingerprint_hex, fingerprint_short, Device, ROLE_HOST, ROLE_VIEWER};
use glass_core::net::{pair_begin, Client, ConnectError, HostEvent, Pairing};
use glass_core::protocol::{DEFAULT_PORT, MODE_FIT};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, GetStockObject, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_GUI_FONT, DEFAULT_PITCH, FF_DONTCARE, FW_BOLD, FW_NORMAL, HFONT, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

// Control ids.
const ID_LIST: usize = 10;
const ID_ADDRESS: usize = 11;
const ID_CONNECT: usize = 12;
const ID_SHARE: usize = 13;
const ID_PAIRED: usize = 14;
const ID_REFRESH: usize = 15;
const ID_STATUS: usize = 16;
const ID_PIN: usize = 17;
const ID_ADMIN: usize = 18;

// Messages from workers (lparam = Box<..> raw pointer, taken by the UI).
const WM_ASK_PIN: u32 = WM_APP + 20; // Box<(Pairing, String host, u16 port)>
const WM_CONNECT_FAILED: u32 = WM_APP + 21; // Box<String>
const WM_SESSION_REPORT: u32 = WM_APP + 22; // Box<String>
const WM_HOST_EVENT: u32 = WM_APP + 23; // Box<HostEvent>
const WM_HOST_LINE: u32 = WM_APP + 24; // Box<String>
const WM_HOST_STOPPED: u32 = WM_APP + 25; // Box<String> (why)
const WM_TRAY: u32 = WM_APP + 26;
const TIMER_REFRESH: usize = 1;
const TIMER_SHOT: usize = 2;

// Tray menu ids.
const MENU_OPEN: usize = 100;
const MENU_SHARE: usize = 101;
const MENU_QUIT: usize = 102;

struct App {
    hwnd: HWND,
    device: Arc<Device>,
    font: HFONT,
    big: HFONT,
    rows: Vec<Row>,
    sharing: Option<Sharing>,
    connecting: bool,
    quitting: bool,
}

struct Sharing {
    stop: Arc<AtomicBool>,
    port: u16,
}

/// A line in the machine list.
#[derive(Clone)]
struct Row {
    name: String,
    kind: String,
    address: String,
    port: u16,
    status: String,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn post<T>(hwnd: HWND, msg: u32, value: T) {
    let p = Box::into_raw(Box::new(value)) as isize;
    unsafe {
        if PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(p)).is_err() {
            drop(Box::from_raw(p as *mut T));
        }
    }
}

unsafe fn take<T>(lparam: LPARAM) -> T {
    *Box::from_raw(lparam.0 as *mut T)
}

unsafe fn child(hwnd: HWND, id: usize) -> HWND {
    GetDlgItem(Some(hwnd), id as i32).unwrap_or_default()
}

unsafe fn set_text(hwnd: HWND, id: usize, text: &str) {
    let t = wide(text);
    let _ = SetWindowTextW(child(hwnd, id), PCWSTR(t.as_ptr()));
}

unsafe fn get_text(hwnd: HWND) -> String {
    let n = GetWindowTextLengthW(hwnd).max(0) as usize;
    let mut buf = vec![0u16; n + 1];
    let got = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
    String::from_utf16_lossy(&buf[..got])
}

/// Opens the Glass window and runs it until Quit. `share` starts sharing
/// straight away (after "Restart as administrator").
pub fn run(share: bool) -> Res<()> {
    super::d3d::init_process();
    let device = crate::device()?;
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
        let inst: HINSTANCE = GetModuleHandleW(None)?.into();
        // MAKEINTRESOURCE(1): resource ids travel as small pointer values.
        #[allow(clippy::manual_dangling_ptr)]
        let icon = LoadIconW(Some(inst), PCWSTR(1 as *const u16)).unwrap_or_default();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: inst,
            hIcon: icon,
            hIconSm: icon,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: windows::Win32::Graphics::Gdi::GetSysColorBrush(
                windows::Win32::Graphics::Gdi::COLOR_BTNFACE,
            ),
            lpszClassName: w!("GlassApp"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("GlassApp"),
            w!("Glass"),
            WS_OVERLAPPEDWINDOW & !WS_MAXIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            100,
            100,
            None,
            None,
            Some(inst),
            None,
        )?;
        let k = GetDpiForWindow(hwnd).max(96) as f64 / 96.0;
        let px = |v: i32| (v as f64 * k).round() as i32;
        let font = CreateFontW(
            -px(15),
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
        let big = CreateFontW(
            -px(34),
            0,
            0,
            0,
            FW_BOLD.0 as i32,
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
        build(hwnd, inst, font, big, px)?;
        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                hwnd,
                device: device.clone(),
                font,
                big,
                rows: Vec::new(),
                sharing: None,
                connecting: false,
                quitting: false,
            })
        });
        add_tray(hwnd, icon, true);
        let _ = browse::start();
        SetTimer(Some(hwnd), TIMER_REFRESH, 1500, None);
        with_app(|a| a.refresh());
        set_text(
            hwnd,
            ID_STATUS,
            &format!(
                "This PC: \u{201C}{}\u{201D}, id {}. Pick a machine and Connect, or Share this PC so another can view it.",
                device.name,
                fingerprint_short(&device.fingerprint())
            ),
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        // GLASS_DEBUG_SHOT=FILE.bmp: save the window after 3 s and quit (checks).
        if std::env::var_os("GLASS_DEBUG_SHOT").is_some() {
            SetTimer(Some(hwnd), TIMER_SHOT, 3000, None);
        }
        if share {
            with_app(|a| a.toggle_share());
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        add_tray(hwnd, icon, false);
        browse::stop();
    }
    Ok(())
}

unsafe fn build(
    hwnd: HWND,
    inst: HINSTANCE,
    font: HFONT,
    big: HFONT,
    px: impl Fn(i32) -> i32,
) -> Res<()> {
    let mk = |class: PCWSTR,
              text: &str,
              style: WINDOW_STYLE,
              ex: WINDOW_EX_STYLE,
              r: (i32, i32, i32, i32),
              id: usize|
     -> Res<HWND> {
        let t = wide(text);
        let c = CreateWindowExW(
            ex,
            class,
            PCWSTR(t.as_ptr()),
            WS_CHILD | WS_VISIBLE | style,
            px(r.0),
            px(r.1),
            px(r.2),
            px(r.3),
            Some(hwnd),
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
        Ok(c)
    };
    let none = WINDOW_EX_STYLE::default();
    mk(
        w!("STATIC"),
        "Machines with Glass on this network (double-click to connect):",
        WINDOW_STYLE(0),
        none,
        (16, 12, 640, 22),
        0,
    )?;
    let list = mk(
        WC_LISTVIEWW,
        "",
        WS_TABSTOP | WS_BORDER | WINDOW_STYLE(LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS),
        none,
        (16, 38, 720, 230),
        ID_LIST,
    )?;
    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        Some(WPARAM(0)),
        Some(LPARAM(
            (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize,
        )),
    );
    for (i, (title, width)) in [
        ("Name", 260),
        ("Type", 70),
        ("Address", 160),
        ("Status", 220),
    ]
    .iter()
    .enumerate()
    {
        let t = wide(title);
        let col = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH,
            cx: px(*width),
            pszText: PWSTR(t.as_ptr() as *mut u16),
            ..Default::default()
        };
        SendMessageW(
            list,
            LVM_INSERTCOLUMNW,
            Some(WPARAM(i)),
            Some(LPARAM(&col as *const _ as isize)),
        );
    }
    mk(
        w!("STATIC"),
        "Or its address:",
        WINDOW_STYLE(0),
        none,
        (16, 282, 110, 24),
        0,
    )?;
    let last = glass_core::identity::default_dir()
        .and_then(|d| std::fs::read_to_string(d.join("last-host.txt")).ok())
        .unwrap_or_default();
    mk(
        w!("EDIT"),
        last.trim(),
        WS_TABSTOP | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        none,
        (128, 280, 300, 26),
        ID_ADDRESS,
    )?;
    mk(
        w!("BUTTON"),
        "Connect",
        WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
        none,
        (440, 279, 120, 28),
        ID_CONNECT,
    )?;
    mk(
        w!("BUTTON"),
        "Refresh",
        WS_TABSTOP,
        none,
        (616, 279, 120, 28),
        ID_REFRESH,
    )?;
    mk(
        w!("BUTTON"),
        "Share this PC",
        WS_TABSTOP,
        none,
        (16, 322, 170, 32),
        ID_SHARE,
    )?;
    mk(
        w!("BUTTON"),
        "Paired devices\u{2026}",
        WS_TABSTOP,
        none,
        (196, 322, 160, 32),
        ID_PAIRED,
    )?;
    let admin = mk(
        w!("BUTTON"),
        "Restart as administrator",
        WS_TABSTOP,
        none,
        (366, 322, 200, 32),
        ID_ADMIN,
    )?;
    if super::blocker::self_elevated() {
        let _ = ShowWindow(admin, SW_HIDE);
    }
    mk(
        w!("STATIC"),
        "",
        WINDOW_STYLE(0),
        none,
        (16, 366, 720, 60),
        ID_STATUS,
    )?;
    let pin = mk(
        w!("STATIC"),
        "",
        WINDOW_STYLE(0x1), /* SS_CENTER */
        none,
        (16, 428, 720, 50),
        ID_PIN,
    )?;
    SendMessageW(
        pin,
        WM_SETFONT,
        Some(WPARAM(big.0 as usize)),
        Some(LPARAM(1)),
    );
    // Window size for that layout.
    let mut r = RECT {
        left: 0,
        top: 0,
        right: px(752),
        bottom: px(490),
    };
    let _ = AdjustWindowRectExForDpi(
        &mut r,
        WS_OVERLAPPEDWINDOW,
        false,
        WINDOW_EX_STYLE::default(),
        GetDpiForWindow(hwnd),
    );
    let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        r.right - r.left,
        r.bottom - r.top,
        SWP_NOMOVE | SWP_NOZORDER,
    );
    let _ = GetStockObject(DEFAULT_GUI_FONT);
    Ok(())
}

unsafe fn add_tray(hwnd: HWND, icon: HICON, add: bool) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    let tip = wide("Glass");
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    let _ = Shell_NotifyIconW(if add { NIM_ADD } else { NIM_DELETE }, &nid);
}

impl App {
    /// Rebuilds the machine list: found on the network, plus paired hosts
    /// that aren't showing up.
    fn refresh(&mut self) {
        let mine = fingerprint_hex(&self.device.fingerprint());
        let peers = self.device.peers();
        let mut rows: Vec<Row> = Vec::new();
        for f in browse::list() {
            let fp = f.fp.clone().unwrap_or_default().to_lowercase();
            if !fp.is_empty() && mine.starts_with(&fp) {
                continue; // this PC itself
            }
            let paired = !fp.is_empty()
                && peers.iter().any(|p| {
                    p.roles & ROLE_HOST != 0 && fingerprint_hex(&p.fingerprint).starts_with(&fp)
                });
            let version_ok = f
                .version
                .as_deref()
                .is_none_or(|v| v == glass_core::protocol::PROTOCOL_VERSION.to_string());
            rows.push(Row {
                name: f.name.clone(),
                kind: match f.os.as_deref() {
                    Some("mac") => "Mac".into(),
                    Some("windows") => "PC".into(),
                    _ => "?".into(),
                },
                address: f.address.clone(),
                port: f.port,
                status: if !version_ok {
                    "different Glass version".into()
                } else if paired {
                    "\u{2713} paired".into()
                } else {
                    "not paired yet (PIN)".into()
                },
            });
        }
        if rows
            .iter()
            .map(|r| (&r.name, &r.address))
            .ne(self.rows.iter().map(|r| (&r.name, &r.address)))
            || rows
                .iter()
                .map(|r| &r.status)
                .ne(self.rows.iter().map(|r| &r.status))
        {
            self.rows = rows;
            self.fill_list();
        }
    }

    fn fill_list(&self) {
        unsafe {
            let list = child(self.hwnd, ID_LIST);
            SendMessageW(list, LVM_DELETEALLITEMS, None, None);
            for (i, r) in self.rows.iter().enumerate() {
                for (col, text) in [&r.name, &r.kind, &r.address, &r.status].iter().enumerate() {
                    let t = wide(text);
                    let mut item = LVITEMW {
                        mask: LVIF_TEXT,
                        iItem: i as i32,
                        iSubItem: col as i32,
                        pszText: PWSTR(t.as_ptr() as *mut u16),
                        ..Default::default()
                    };
                    let msg = if col == 0 {
                        LVM_INSERTITEMW
                    } else {
                        LVM_SETITEMW
                    };
                    SendMessageW(list, msg, None, Some(LPARAM(&mut item as *mut _ as isize)));
                }
            }
        }
    }

    fn selected_row(&self) -> Option<Row> {
        unsafe {
            let list = child(self.hwnd, ID_LIST);
            let i = SendMessageW(
                list,
                LVM_GETNEXTITEM,
                Some(WPARAM(usize::MAX)),
                Some(LPARAM(LVNI_SELECTED as isize)),
            );
            self.rows.get(i.0 as usize).cloned().filter(|_| i.0 >= 0)
        }
    }

    /// Connect to the selected machine (or the typed address).
    fn connect(&mut self, from_list: bool) {
        if self.connecting {
            return;
        }
        let (host, port) = match (from_list, self.selected_row()) {
            (true, Some(r)) => (r.address, r.port),
            _ => {
                let text = unsafe { get_text(child(self.hwnd, ID_ADDRESS)) };
                let text = text.trim().to_string();
                if text.is_empty() {
                    match self.selected_row() {
                        Some(r) => (r.address, r.port),
                        None => {
                            unsafe {
                                set_text(
                                    self.hwnd,
                                    ID_STATUS,
                                    "Pick a machine in the list or type its address.",
                                )
                            };
                            return;
                        }
                    }
                } else {
                    match text.rsplit_once(':') {
                        Some((h, p)) if p.parse::<u16>().is_ok() => {
                            (h.to_string(), p.parse().unwrap())
                        }
                        _ => (text, DEFAULT_PORT),
                    }
                }
            }
        };
        if let Some(dir) = glass_core::identity::default_dir() {
            let a = if port == DEFAULT_PORT {
                host.clone()
            } else {
                format!("{host}:{port}")
            };
            let _ = std::fs::write(dir.join("last-host.txt"), a);
        }
        self.connecting = true;
        unsafe {
            set_text(
                self.hwnd,
                ID_STATUS,
                &format!("Connecting to {host} \u{2026}"),
            )
        };
        let (hwnd, device) = (self.hwnd.0 as isize, self.device.clone());
        std::thread::spawn(move || connect_worker(HWND(hwnd as *mut _), device, host, port, None));
    }

    fn toggle_share(&mut self) {
        if let Some(s) = self.sharing.take() {
            s.stop.store(true, Ordering::Relaxed);
            // Wake the listener so it sees `stop`.
            let _ = std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], s.port)),
                std::time::Duration::from_millis(500),
            );
            unsafe {
                set_text(self.hwnd, ID_SHARE, "Share this PC");
                set_text(self.hwnd, ID_STATUS, "Stopping sharing \u{2026}");
                set_text(self.hwnd, ID_PIN, "");
            }
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let hwnd = self.hwnd.0 as isize;
        let ui = super::HostUi {
            event: Box::new(move |e| post(HWND(hwnd as *mut _), WM_HOST_EVENT, e)),
            line: Box::new(move |l| post(HWND(hwnd as *mut _), WM_HOST_LINE, l.to_string())),
            report: Box::new(move |r| {
                save_report("results-host.txt", r);
                post(HWND(hwnd as *mut _), WM_SESSION_REPORT, r.to_string())
            }),
            stop: stop.clone(),
        };
        std::thread::spawn(move || {
            let why = match super::host_with(&[], ui) {
                Ok(()) => "Sharing stopped.".to_string(),
                Err(e) => format!("Sharing stopped: {e}"),
            };
            post(HWND(hwnd as *mut _), WM_HOST_STOPPED, why);
        });
        self.sharing = Some(Sharing {
            stop,
            port: DEFAULT_PORT,
        });
        unsafe {
            set_text(self.hwnd, ID_SHARE, "Stop sharing");
            set_text(
                self.hwnd,
                ID_STATUS,
                "Sharing this PC: waiting for a viewer \u{2026}",
            );
        }
    }

    fn host_event(&mut self, e: HostEvent) {
        unsafe {
            match e {
                HostEvent::PairingPin { device, pin, addr, .. } => {
                    let (a, b) = pin.split_at(3);
                    set_text(self.hwnd, ID_PIN, &format!("PIN  {a} {b}"));
                    set_text(
                        self.hwnd,
                        ID_STATUS,
                        &format!(
                            "\u{201C}{device}\u{201D} ({}) wants to pair. Type this PIN on that device (valid 2 minutes). Pair only your own devices.",
                            addr.ip()
                        ),
                    );
                    show(self.hwnd);
                    let _ = FlashWindow(self.hwnd, true);
                }
                HostEvent::Paired { device } => {
                    set_text(self.hwnd, ID_PIN, "");
                    set_text(self.hwnd, ID_STATUS, &format!("Paired with \u{201C}{device}\u{201D}. It can now view and control this PC."));
                }
                HostEvent::WrongPin { device, left } => set_text(
                    self.hwnd,
                    ID_STATUS,
                    &format!("Wrong PIN from \u{201C}{device}\u{201D}. {left} more tries with this PIN."),
                ),
                HostEvent::PairingLocked => {
                    set_text(self.hwnd, ID_PIN, "");
                    set_text(self.hwnd, ID_STATUS, "Too many wrong PINs: pairing is off. Stop and start sharing to pair a device.");
                }
                HostEvent::Refused { fingerprint, addr } => set_text(
                    self.hwnd,
                    ID_STATUS,
                    &format!("Refused a device that isn't paired ({}, id {fingerprint}). Pair it from that device first.", addr.ip()),
                ),
                HostEvent::Denied { device } => set_text(self.hwnd, ID_STATUS, &format!("Did not allow \u{201C}{device}\u{201D} to connect.")),
                HostEvent::Loop { device } => set_text(
                    self.hwnd,
                    ID_STATUS,
                    &format!("Refused \u{201C}{device}\u{201D}: this PC is already viewing it, so it would be a loop."),
                ),
            }
        }
    }

    fn host_line(&mut self, l: &str) {
        // Only the lines worth showing in the window.
        if l.starts_with("Viewer connected")
            || l.starts_with("Input blocked")
            || l.starts_with("Input works")
            || l.starts_with("Viewer switched")
            || l.starts_with("Waiting for a viewer")
        {
            unsafe { set_text(self.hwnd, ID_STATUS, l.trim()) };
        }
    }
}

fn save_report(file: &str, report: &str) {
    use std::io::Write;
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(file))
            {
                let _ = writeln!(f, "{report}");
            }
        }
    }
}

unsafe fn show(hwnd: HWND) {
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = ShowWindow(hwnd, SW_RESTORE);
    let _ = SetForegroundWindow(hwnd);
}

/// Connects (pairing first if needed, PIN from the UI), then views.
fn connect_worker(
    hwnd: HWND,
    device: Arc<Device>,
    host: String,
    port: u16,
    pin: Option<(Pairing, String)>,
) {
    let fail = |e: String| post(hwnd, WM_CONNECT_FAILED, e);
    if let Some((p, pin)) = pin {
        if let Err(e) = p.finish(&pin) {
            return fail(match e {
                ConnectError::WrongPin => {
                    "That PIN wasn't right (or it expired). Connect again for a new try.".into()
                }
                e => format!("{host}: {e}"),
            });
        }
    }
    let modes_file = glass_core::identity::default_dir()
        .unwrap_or_default()
        .join("view-modes.txt");
    let _ = modes_file;
    let prep = match viewer::prepare(MODE_FIT, false) {
        Ok(p) => p,
        Err(e) => return fail(format!("Can't start the viewer: {e}")),
    };
    match Client::connect_with(&device, &host, port, prep.hello) {
        Ok(c) => {
            let opts = viewer::Options {
                windowed: false,
                seconds: None,
                rule: None,
                full_colour: false,
            };
            post(hwnd, WM_HOST_LINE, String::new()); // clears "Connecting…" via refresh
            match viewer::run(c, prep, opts, &crate::banner()) {
                Ok((report, _mode)) => {
                    save_report("results-viewer.txt", &report);
                    post(hwnd, WM_SESSION_REPORT, report);
                }
                Err(e) => fail(format!("The viewer stopped: {e}")),
            }
        }
        Err(ConnectError::NotPaired) => match pair_begin(&device, &host, port) {
            Ok(p) => post(hwnd, WM_ASK_PIN, (p, host, port)),
            Err(e) => fail(format!("{host}: {e}")),
        },
        Err(e) => fail(format!("{host}: {e}")),
    }
}

/// A small modal box asking for the PIN. None if cancelled.
unsafe fn ask_pin(owner: HWND, host_name: &str) -> Option<String> {
    let inst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
    let k = GetDpiForWindow(owner).max(96) as f64 / 96.0;
    let px = |v: i32| (v as f64 * k).round() as i32;
    let font = APP.with(|a| a.borrow().as_ref().map(|a| (a.font, a.big)))?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(dialog_proc),
        hInstance: inst,
        hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
        hbrBackground: windows::Win32::Graphics::Gdi::GetSysColorBrush(
            windows::Win32::Graphics::Gdi::COLOR_BTNFACE,
        ),
        lpszClassName: w!("GlassPin"),
        ..Default::default()
    };
    RegisterClassExW(&class);
    let mut r = RECT {
        left: 0,
        top: 0,
        right: px(420),
        bottom: px(190),
    };
    let _ = AdjustWindowRectExForDpi(
        &mut r,
        WS_CAPTION | WS_SYSMENU,
        false,
        WS_EX_DLGMODALFRAME,
        GetDpiForWindow(owner),
    );
    let mut or = RECT::default();
    let _ = GetWindowRect(owner, &mut or);
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let dlg = CreateWindowExW(
        WS_EX_DLGMODALFRAME,
        w!("GlassPin"),
        w!("Pair with Glass"),
        WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        or.left + ((or.right - or.left) - w) / 2,
        or.top + ((or.bottom - or.top) - h) / 2,
        w,
        h,
        Some(owner),
        None,
        Some(inst),
        None,
    )
    .ok()?;
    let mk =
        |class: PCWSTR, text: &str, style: u32, r: (i32, i32, i32, i32), id: usize, f: HFONT| {
            let t = wide(text);
            if let Ok(c) = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                PCWSTR(t.as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                px(r.0),
                px(r.1),
                px(r.2),
                px(r.3),
                Some(dlg),
                Some(HMENU(id as *mut _)),
                Some(inst),
                None,
            ) {
                SendMessageW(c, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
            }
        };
    mk(
        w!("STATIC"),
        &format!("\u{201C}{host_name}\u{201D} is showing a 6-digit PIN. Type it here. Pair only with your own devices."),
        0,
        (16, 12, 388, 44),
        0,
        font.0,
    );
    mk(
        w!("EDIT"),
        "",
        (WS_BORDER | WS_TABSTOP).0 | ES_CENTER as u32 | ES_NUMBER as u32,
        (110, 64, 200, 48),
        1,
        font.1,
    );
    mk(
        w!("BUTTON"),
        "Pair",
        (WS_TABSTOP).0 | BS_DEFPUSHBUTTON as u32,
        (196, 132, 100, 32),
        2,
        font.0,
    );
    mk(
        w!("BUTTON"),
        "Cancel",
        (WS_TABSTOP).0,
        (304, 132, 100, 32),
        3,
        font.0,
    );
    let edit = child(dlg, 1);
    SendMessageW(edit, EM_SETLIMITTEXT, Some(WPARAM(6)), None);
    let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(edit));
    let _ = EnableWindow(owner, false);
    DIALOG_RESULT.with(|r| *r.borrow_mut() = None);
    let mut msg = MSG::default();
    while DIALOG_RESULT.with(|r| r.borrow().is_none())
        && GetMessageW(&mut msg, None, 0, 0).as_bool()
    {
        if !IsDialogMessageW(dlg, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    let ok = DIALOG_RESULT.with(|r| r.borrow_mut().take()) == Some(true);
    let pin = get_text(edit);
    let _ = EnableWindow(owner, true);
    let _ = DestroyWindow(dlg);
    let _ = SetForegroundWindow(owner);
    let pin: String = pin.chars().filter(char::is_ascii_digit).collect();
    (ok && !pin.is_empty()).then_some(pin)
}

thread_local! {
    static DIALOG_RESULT: RefCell<Option<bool>> = const { RefCell::new(None) };
}

unsafe extern "system" fn dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match wparam.0 & 0xFFFF {
                2 | 1 if (wparam.0 >> 16) as u32 == BN_CLICKED || wparam.0 & 0xFFFF == 2 => {
                    DIALOG_RESULT.with(|r| *r.borrow_mut() = Some(true))
                }
                3 => DIALOG_RESULT.with(|r| *r.borrow_mut() = Some(false)),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            DIALOG_RESULT.with(|r| *r.borrow_mut() = Some(false));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Session results with a Copy button.
unsafe fn show_report(owner: HWND, report: &str) {
    let text = report.replace('\n', "\r\n");
    let t = wide(&format!(
        "{text}\r\n\r\nCopy these with Ctrl+A, Ctrl+C (or press Yes to copy them now) and paste them back to Claude."
    ));
    let r = MessageBoxW(
        Some(owner),
        PCWSTR(t.as_ptr()),
        w!("Glass session results"),
        MB_YESNO | MB_ICONINFORMATION,
    );
    if r == IDYES {
        copy_to_clipboard(owner, report);
    }
}

unsafe fn copy_to_clipboard(owner: HWND, text: &str) {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let w = wide(&text.replace('\n', "\r\n"));
    if OpenClipboard(Some(owner)).is_err() {
        return;
    }
    let _ = EmptyClipboard();
    if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2) {
        let p = GlobalLock(h) as *mut u16;
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
            let _ = GlobalUnlock(h);
            if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0))).is_err() {
                let _ = GlobalFree(Some(h));
            }
        }
    }
    let _ = CloseClipboard();
}

/// Paired devices with Forget (a simple numbered prompt keeps this small).
unsafe fn paired_devices(owner: HWND, device: &Device) {
    loop {
        let peers = device.peers();
        if peers.is_empty() {
            MessageBoxW(
                Some(owner),
                w!("No paired devices yet."),
                w!("Paired devices"),
                MB_OK,
            );
            return;
        }
        let mut text = String::from("Paired with this PC:\r\n\r\n");
        for p in &peers {
            let mut roles = Vec::new();
            if p.roles & ROLE_VIEWER != 0 {
                roles.push("may view this PC");
            }
            if p.roles & ROLE_HOST != 0 {
                roles.push("this PC may view it");
            }
            text.push_str(&format!(
                "\u{2022} {}  (id {}; {})\r\n",
                p.name,
                fingerprint_short(&p.fingerprint),
                roles.join(", ")
            ));
        }
        text.push_str("\r\nForget one? Press Yes to choose the first one listed, No to close.\r\n(Forget others one at a time.)");
        let t = wide(&text);
        if MessageBoxW(
            Some(owner),
            PCWSTR(t.as_ptr()),
            w!("Paired devices"),
            MB_YESNO,
        ) != IDYES
        {
            return;
        }
        let p = &peers[0];
        let q = wide(&format!(
            "Forget \u{201C}{}\u{201D}? It will have to pair again with a PIN.",
            p.name
        ));
        if MessageBoxW(
            Some(owner),
            PCWSTR(q.as_ptr()),
            w!("Forget"),
            MB_OKCANCEL | MB_ICONWARNING,
        ) == IDOK
        {
            let _ = device.forget(&p.fingerprint);
        }
    }
}

unsafe fn tray_menu(hwnd: HWND) {
    let Ok(m) = CreatePopupMenu() else { return };
    let sharing = with_app(|a| a.sharing.is_some()).unwrap_or(false);
    let _ = AppendMenuW(m, MF_STRING, MENU_OPEN, w!("Open Glass"));
    let share = wide(if sharing {
        "Stop sharing"
    } else {
        "Share this PC"
    });
    let _ = AppendMenuW(m, MF_STRING, MENU_SHARE, PCWSTR(share.as_ptr()));
    let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(m, MF_STRING, MENU_QUIT, w!("Quit Glass"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(m, TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), hwnd, None);
    let _ = DestroyMenu(m);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Messages that open nested loops (dialogs), or arrive while the app
    // state is borrowed (DestroyWindow sends WM_DESTROY), are handled here.
    match msg {
        WM_DESTROY => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        WM_ASK_PIN => {
            let (p, host, port): (Pairing, String, u16) = take(lparam);
            let name = p.host_name().to_string();
            match ask_pin(hwnd, &name) {
                Some(pin) => {
                    set_text(
                        hwnd,
                        ID_STATUS,
                        &format!("Pairing with \u{201C}{name}\u{201D} \u{2026}"),
                    );
                    let device = with_app(|a| a.device.clone());
                    if let Some(device) = device {
                        let h = hwnd.0 as isize;
                        std::thread::spawn(move || {
                            connect_worker(HWND(h as *mut _), device, host, port, Some((p, pin)))
                        });
                    }
                }
                None => {
                    drop(p);
                    with_app(|a| a.connecting = false);
                    set_text(hwnd, ID_STATUS, "Pairing cancelled.");
                }
            }
            return LRESULT(0);
        }
        WM_SESSION_REPORT => {
            let report: String = take(lparam);
            with_app(|a| {
                a.connecting = false;
            });
            set_text(
                hwnd,
                ID_STATUS,
                "Session ended. Results saved next to Glass.exe.",
            );
            show_report(hwnd, &report);
            return LRESULT(0);
        }
        WM_CONNECT_FAILED => {
            let e: String = take(lparam);
            with_app(|a| a.connecting = false);
            set_text(hwnd, ID_STATUS, &e);
            let t = wide(&e);
            MessageBoxW(
                Some(hwnd),
                PCWSTR(t.as_ptr()),
                w!("Couldn't connect"),
                MB_OK | MB_ICONWARNING,
            );
            return LRESULT(0);
        }
        WM_COMMAND if wparam.0 & 0xFFFF == ID_PAIRED => {
            if let Some(device) = with_app(|a| a.device.clone()) {
                paired_devices(hwnd, &device);
                with_app(|a| {
                    a.rows.clear();
                    a.refresh()
                });
            }
            return LRESULT(0);
        }
        _ => {}
    }
    let handled = with_app(|a| -> Option<LRESULT> {
        match msg {
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                match id {
                    ID_CONNECT => a.connect(false),
                    ID_REFRESH => {
                        let _ = browse::start();
                        a.rows.clear();
                        a.refresh();
                    }
                    ID_SHARE | MENU_SHARE => a.toggle_share(),
                    ID_ADMIN => {
                        if let Ok(exe) = std::env::current_exe() {
                            let e = wide(&exe.to_string_lossy());
                            let share = a.sharing.is_some();
                            let r = ShellExecuteW(
                                Some(hwnd),
                                w!("runas"),
                                PCWSTR(e.as_ptr()),
                                if share { w!("app --share") } else { w!("app") },
                                None,
                                SW_SHOWNORMAL,
                            );
                            if r.0 as isize > 32 {
                                // The elevated copy takes over.
                                if let Some(s) = a.sharing.take() {
                                    s.stop.store(true, Ordering::Relaxed);
                                    let _ = std::net::TcpStream::connect(("127.0.0.1", s.port));
                                }
                                a.quitting = true;
                                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                            }
                        }
                    }
                    MENU_OPEN => unsafe { show(hwnd) },
                    MENU_QUIT => {
                        a.quitting = true;
                        let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
                    }
                    _ => return None,
                }
                Some(LRESULT(0))
            }
            WM_NOTIFY => {
                let h = unsafe { &*(lparam.0 as *const NMHDR) };
                if h.idFrom == ID_LIST && h.code == NM_DBLCLK {
                    a.connect(true);
                }
                None
            }
            WM_TIMER if wparam.0 == TIMER_SHOT => {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_SHOT);
                    if let Some(p) = std::env::var_os("GLASS_DEBUG_SHOT") {
                        super::viewer::save_shot(hwnd, std::path::Path::new(&p));
                    }
                    a.quitting = true;
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wparam.0 == TIMER_REFRESH => {
                a.refresh();
                Some(LRESULT(0))
            }
            WM_HOST_EVENT => {
                let e: HostEvent = unsafe { take(lparam) };
                a.host_event(e);
                Some(LRESULT(0))
            }
            WM_HOST_LINE => {
                let l: String = unsafe { take(lparam) };
                a.host_line(&l);
                Some(LRESULT(0))
            }
            WM_HOST_STOPPED => {
                let why: String = unsafe { take(lparam) };
                a.sharing = None;
                unsafe {
                    set_text(hwnd, ID_SHARE, "Share this PC");
                    set_text(hwnd, ID_PIN, "");
                    set_text(hwnd, ID_STATUS, &why);
                }
                if a.quitting {
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                }
                Some(LRESULT(0))
            }
            WM_TRAY => {
                match (lparam.0 & 0xFFFF) as u32 {
                    WM_LBUTTONDBLCLK | WM_LBUTTONUP => unsafe { show(hwnd) },
                    WM_RBUTTONUP | WM_CONTEXTMENU => unsafe { tray_menu(hwnd) },
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                // While sharing, closing the window keeps Glass in the tray.
                if a.sharing.is_some() && !a.quitting {
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                    }
                    return Some(LRESULT(0));
                }
                if let Some(s) = a.sharing.take() {
                    s.stop.store(true, Ordering::Relaxed);
                    let _ = std::net::TcpStream::connect_timeout(
                        &std::net::SocketAddr::from(([127, 0, 0, 1], s.port)),
                        std::time::Duration::from_millis(500),
                    );
                }
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    })
    .flatten();
    handled.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
}

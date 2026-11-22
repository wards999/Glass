//! The Glass window on Windows (M3d): `Glass.exe`, drawn with egui.
//!
//! Glass is an enabler: this window exists to get you into a remote screen
//! fast, so it is one calm column. Devices on the network are big rows (click
//! one to connect); sharing this PC is a switch in a card at the bottom, with
//! the pairing code shown in large tiles when another device asks. Settings
//! (theme, start view, colour, paired devices) sit behind the gear; light and
//! dark follow Windows unless chosen. Closing while sharing keeps Glass in the
//! tray. The command line (`glass-cli`) stays for tests and CI.
//!
//! Threads: egui's on the main thread; connecting, pairing and each viewer
//! session on workers (the viewer opens its own window); sharing runs
//! `host_with` on its own thread. Workers report back on a channel and wake
//! the UI with `request_repaint`.
//!
//! Checks: GLASS_DEBUG_SHOT=FILE.bmp saves the window after 3 s and quits;
//! GLASS_DEBUG_DEMO=1 (or =share, =settings) shows sample devices (and a pairing code).

use super::{browse, viewer, Res};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Layout,
    Margin, Rect, RichText, Sense, Stroke, StrokeKind, ThemePreference,
};
use glass_core::identity::{
    fingerprint_hex, fingerprint_short, Device, Peer, ROLE_HOST, ROLE_VIEWER,
};
use glass_core::net::{pair_begin, Client, ConnectError, HostEvent, Pairing};
use glass_core::protocol::{
    DEFAULT_PORT, MODE_FIT, MODE_FIT_LARGER, MODE_FIT_MORE_SPACE, MODE_HOST_SCREEN,
    PROTOCOL_VERSION,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// From workers (and the tray) to the UI.
enum Msg {
    AskPin(Pairing, String, u16),
    Failed(String),
    Report(String),
    Status(String),
    Host(HostEvent),
    HostLine(String),
    HostStopped(String),
    TrayOpen,
    /// The connection dropped; the viewer is trying again.
    Reconnecting,
    TrayShare,
    TrayQuit,
}

/// Where workers send, and how they wake the UI.
#[derive(Clone)]
struct Wake {
    tx: Sender<Msg>,
    ctx: egui::Context,
}

impl Wake {
    fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
        self.ctx.request_repaint();
    }
}

static TRAY_WAKE: OnceLock<Mutex<Option<Wake>>> = OnceLock::new();

fn tray_send(m: Msg) {
    if let Some(w) = TRAY_WAKE.get().and_then(|w| w.lock().ok()?.clone()) {
        w.send(m);
    }
}

/// Saved choices (`settings.txt` next to the identity).
#[derive(Clone, PartialEq)]
struct Settings {
    theme: ThemePreference,
    view: u8,
    full_colour: bool,
}

impl Settings {
    fn path() -> std::path::PathBuf {
        glass_core::identity::default_dir()
            .unwrap_or_default()
            .join("settings.txt")
    }

    fn load() -> Self {
        let mut s = Settings {
            theme: ThemePreference::System,
            view: MODE_FIT,
            full_colour: false,
        };
        for line in std::fs::read_to_string(Self::path())
            .unwrap_or_default()
            .lines()
        {
            match line.split_once('=') {
                Some(("theme", "light")) => s.theme = ThemePreference::Light,
                Some(("theme", "dark")) => s.theme = ThemePreference::Dark,
                Some(("view", v)) => s.view = v.parse().unwrap_or(MODE_FIT),
                Some(("colour", v)) => s.full_colour = v == "444",
                _ => {}
            }
        }
        s
    }

    fn save(&self) {
        let theme = match self.theme {
            ThemePreference::Light => "light",
            ThemePreference::Dark => "dark",
            ThemePreference::System => "system",
        };
        let colour = if self.full_colour { "444" } else { "420" };
        let _ = std::fs::write(
            Self::path(),
            format!("theme={theme}\nview={}\ncolour={colour}\n", self.view),
        );
    }
}

/// A device row.
#[derive(Clone)]
struct Machine {
    name: String,
    mac: bool,
    kind: &'static str,
    address: String,
    port: u16,
    paired: bool,
    version_ok: bool,
}

enum Dialog {
    None,
    Pin {
        pairing: Option<Pairing>,
        host: String,
        port: u16,
        pin: String,
        name: String,
    },
    Report(String, bool),
    Settings,
    Message(String, String),
}

struct GlassApp {
    device: Arc<Device>,
    wake: Wake,
    rx: Receiver<Msg>,
    settings: Settings,
    machines: Vec<Machine>,
    last_scan: Instant,
    address: String,
    show_address: bool,
    /// The address being connected to (its row shows a spinner).
    connecting: Option<String>,
    /// The address being viewed (its row says Viewing, with Disconnect).
    viewing: Option<String>,
    /// The name of the machine being viewed (for the session bar).
    viewing_name: String,
    /// Someone is viewing this PC (sharing, in a session).
    viewer_on: bool,
    /// The Glass service (unattended access, M3f) and its agent's status.
    svc: super::service::State,
    svc_status: Option<super::service::Status>,
    /// The service's sharing is switched off (Share switch).
    svc_off: bool,
    /// This window runs in a Remote Desktop session.
    remote: bool,
    notice: Option<(String, Instant)>,
    sharing: Option<Arc<AtomicBool>>,
    /// Ends the current shared session (sharing goes on).
    end_session: Arc<AtomicBool>,
    share_status: String,
    /// Pairing code and the device asking.
    pin: Option<(String, String)>,
    dialog: Dialog,
    quitting: bool,
    elevated: bool,
    icon: Option<egui::TextureHandle>,
    share_h: f32,
    gear_rect: Option<Rect>,
    demo: Option<String>,
    _tray: Option<tray_icon::TrayIcon>,
    shot_at: Option<Instant>,
}

fn icon_rgba(size: u32) -> Vec<u8> {
    match size {
        32 => include_bytes!("../../packaging/glass-32.rgba").to_vec(),
        _ => include_bytes!("../../packaging/glass-64.rgba").to_vec(),
    }
}

/// Opens the Glass window and runs it until Quit. `share` starts sharing
/// straight away (after "Restart as administrator").
pub fn run(share: bool) -> Res<()> {
    super::d3d::init_process();
    let device = crate::device()?;
    let settings = Settings::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Glass")
            .with_inner_size([460.0, 700.0])
            .with_min_inner_size([400.0, 520.0])
            // COM is already multithreaded here (capture, DNS-SD); winit's
            // OLE drag and drop needs single-threaded. LATER(M5c): files.
            .with_drag_and_drop(false)
            .with_icon(egui::IconData {
                rgba: icon_rgba(64),
                width: 64,
                height: 64,
            }),
        ..Default::default()
    };
    eframe::run_native(
        "Glass",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            style(&ctx);
            ctx.set_theme(settings.theme);
            let (tx, rx) = channel();
            let wake = Wake { tx, ctx };
            *TRAY_WAKE.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(wake.clone());
            let tray = make_tray();
            let _ = browse::start();
            let address = glass_core::identity::default_dir()
                .and_then(|d| std::fs::read_to_string(d.join("last-host.txt")).ok())
                .unwrap_or_default()
                .trim()
                .to_string();
            let demo = std::env::var("GLASS_DEBUG_DEMO").ok();
            let mut app = GlassApp {
                device,
                wake,
                rx,
                settings,
                machines: Vec::new(),
                last_scan: Instant::now() - Duration::from_secs(10),
                address,
                show_address: false,
                connecting: None,
                viewing: None,
                viewing_name: String::new(),
                viewer_on: false,
                svc: super::service::state(),
                svc_status: super::service::Status::read(),
                svc_off: super::service::sharing_off(),
                remote: super::service::in_remote_session(),
                notice: None,
                sharing: None,
                end_session: Arc::new(AtomicBool::new(false)),
                share_status: String::new(),
                pin: None,
                dialog: Dialog::None,
                quitting: false,
                elevated: super::blocker::self_elevated(),
                icon: None,
                share_h: 0.0,
                gear_rect: None,
                _tray: tray,
                shot_at: std::env::var_os("GLASS_DEBUG_SHOT").map(|_| Instant::now()),
                demo,
            };
            if app.demo.as_deref() == Some("share") {
                app.share_status = "Waiting for a viewer \u{2026}".into();
                app.pin = Some(("482913".into(), "MacBook Air".into()));
            }
            if app.demo.as_deref() == Some("service") {
                // As QUIETPC: the service shares, a viewer is on, through Remote Desktop.
                app.svc = super::service::State::Running;
                app.svc_off = false;
                app.remote = true;
                app.svc_status = Some(super::service::Status {
                    line: "Viewer connected: \"QUIETPC22\" from 192.168.4.22 (encrypted)".into(),
                    pin: None,
                    connected: true,
                });
            }
            if app.demo.as_deref() == Some("session") {
                app.viewer_on = true;
                app.share_status =
                    "Viewer connected: \"MacBook Air\" from 192.168.4.26 (encrypted)".into();
            }
            if app.demo.as_deref() == Some("settings") {
                app.dialog = Dialog::Settings;
            }
            if app.demo.as_deref() == Some("report") {
                app.dialog = Dialog::Report("Glass copy check: results text".into(), false);
            }
            if share {
                app.toggle_share();
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| format!("window: {e}"))?;
    browse::stop();
    Ok(())
}

// ---------------------------------------------------------------- look

/// Colours for one theme.
#[derive(Clone, Copy)]
struct Pal {
    bg: Color32,
    card: Color32,
    border: Color32,
    hover: Color32,
    strong: Color32,
    text: Color32,
    weak: Color32,
    accent: Color32,
    ok: Color32,
    warn: Color32,
    track: Color32,
    off: Color32,
    danger: Color32,
}

fn pal(dark: bool) -> Pal {
    let c = Color32::from_rgb;
    if dark {
        Pal {
            bg: c(17, 19, 23),
            card: c(27, 31, 37),
            border: c(42, 48, 57),
            hover: c(33, 39, 48),
            strong: c(238, 241, 245),
            text: c(205, 210, 218),
            weak: c(139, 149, 163),
            accent: c(76, 141, 235),
            ok: c(64, 200, 125),
            warn: c(240, 175, 60),
            track: c(37, 42, 50),
            off: c(72, 80, 92),
            danger: c(240, 105, 105),
        }
    } else {
        Pal {
            bg: c(243, 245, 248),
            card: Color32::WHITE,
            border: c(225, 229, 235),
            hover: c(241, 246, 253),
            strong: c(17, 21, 26),
            text: c(40, 46, 54),
            weak: c(100, 110, 124),
            accent: c(37, 99, 201),
            ok: c(24, 150, 82),
            warn: c(200, 125, 0),
            track: c(234, 237, 242),
            off: c(198, 204, 212),
            danger: c(200, 52, 52),
        }
    }
}

static ICONS: OnceLock<bool> = OnceLock::new();

/// A Segoe Fluent / MDL2 icon, or a plain fallback if the font is missing.
fn ic(fluent: char, fallback: &str) -> String {
    if *ICONS.get().unwrap_or(&false) {
        fluent.to_string()
    } else {
        fallback.to_string()
    }
}

fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}

/// Windows' own fonts: Segoe UI (Variable on 11), Semibold for titles, and
/// Segoe Fluent Icons (MDL2 Assets on 10). egui's fonts stay as fallbacks.
fn fonts(ctx: &egui::Context) {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let read = |names: &[&str]| {
        names
            .iter()
            .find_map(|f| std::fs::read(format!(r"{windir}\Fonts\{f}")).ok())
    };
    let mut fonts = egui::FontDefinitions::default();
    let mut put = |key: &str, bytes: Option<Vec<u8>>| -> Option<String> {
        let b = bytes?;
        fonts
            .font_data
            .insert(key.into(), Arc::new(egui::FontData::from_owned(b)));
        Some(key.to_string())
    };
    let regular = put("segoe", read(&["SegUIVar.ttf", "segoeui.ttf"]));
    let bold = put("segoe-sb", read(&["seguisb.ttf"]));
    let icons = put("icons", read(&["SegoeIcons.ttf", "segmdl2.ttf"]));
    let mut prop = fonts.families[&FontFamily::Proportional].clone();
    if let Some(r) = &regular {
        prop.insert(0, r.clone());
    }
    let mut sb = prop.clone();
    if let Some(b) = bold {
        sb.insert(0, b);
    }
    let mut ico = prop.clone();
    let _ = ICONS.set(icons.is_some());
    if let Some(i) = icons {
        ico.insert(0, i);
    }
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts
        .families
        .insert(FontFamily::Name("semibold".into()), sb);
    fonts.families.insert(FontFamily::Name("icons".into()), ico);
    ctx.set_fonts(fonts);
}

fn style(ctx: &egui::Context) {
    fonts(ctx);
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        let p = pal(theme == egui::Theme::Dark);
        ctx.style_mut_of(theme, |s| {
            s.spacing.item_spacing = vec2(8.0, 8.0);
            s.spacing.button_padding = vec2(14.0, 6.0);
            s.spacing.interact_size.y = 30.0;
            for f in s.text_styles.values_mut() {
                f.size = (f.size * 1.08).round();
            }
            let v = &mut s.visuals;
            let r = CornerRadius::same(8);
            v.panel_fill = p.bg;
            v.window_fill = p.card;
            v.window_stroke = Stroke::new(1.0, p.border);
            v.window_corner_radius = CornerRadius::same(16);
            v.window_shadow = egui::Shadow {
                offset: [0, 10],
                blur: 30,
                spread: 0,
                color: Color32::from_black_alpha(if theme == egui::Theme::Dark { 120 } else { 45 }),
            };
            v.popup_shadow = v.window_shadow;
            v.extreme_bg_color = p.track;
            v.faint_bg_color = p.track;
            v.selection.bg_fill = p.accent;
            v.selection.stroke = Stroke::new(1.5, p.accent);
            v.hyperlink_color = p.accent;
            v.override_text_color = Some(p.text);
            for w in [
                &mut v.widgets.noninteractive,
                &mut v.widgets.inactive,
                &mut v.widgets.hovered,
                &mut v.widgets.active,
                &mut v.widgets.open,
            ] {
                w.corner_radius = r;
                w.fg_stroke.color = p.text;
            }
            v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
            v.widgets.inactive.weak_bg_fill = p.track;
            v.widgets.inactive.bg_fill = p.track;
            v.widgets.inactive.bg_stroke = Stroke::NONE;
            v.widgets.hovered.weak_bg_fill = p.hover;
            v.widgets.hovered.bg_fill = p.hover;
            v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.border);
            v.widgets.active.weak_bg_fill = p.border;
            v.widgets.active.bg_fill = p.border;
            v.widgets.open.weak_bg_fill = p.hover;
        });
    }
}

fn make_tray() -> Option<tray_icon::TrayIcon> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Open Glass", true, None);
    let share = MenuItem::with_id("share", "Share this PC / Stop sharing", true, None);
    let quit = MenuItem::with_id("quit", "Quit Glass", true, None);
    let _ = menu.append_items(&[&open, &share, &PredefinedMenuItem::separator(), &quit]);
    MenuEvent::set_event_handler(Some(|e: MenuEvent| {
        tray_send(match e.id.as_ref() {
            "open" => Msg::TrayOpen,
            "share" => Msg::TrayShare,
            "quit" => Msg::TrayQuit,
            _ => return,
        })
    }));
    tray_icon::TrayIconEvent::set_event_handler(Some(|e: tray_icon::TrayIconEvent| {
        if let tray_icon::TrayIconEvent::DoubleClick { .. } = e {
            tray_send(Msg::TrayOpen);
        }
    }));
    let icon = tray_icon::Icon::from_rgba(icon_rgba(32), 32, 32).ok()?;
    tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Glass")
        .with_icon(icon)
        .build()
        .ok()
}

fn show(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

fn save_report(file: &str, report: &str) {
    use std::io::Write;
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
    {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(file))
        {
            let _ = writeln!(f, "{report}");
        }
    }
}

// ---------------------------------------------------------------- behaviour

impl GlassApp {
    fn scan(&mut self) {
        self.last_scan = Instant::now();
        if self.demo.is_some() {
            let m = |name: &str, mac, address: &str, paired, version_ok| Machine {
                name: name.into(),
                mac,
                kind: if mac { "Mac" } else { "Windows PC" },
                address: address.into(),
                port: DEFAULT_PORT,
                paired,
                version_ok,
            };
            self.machines = vec![
                m("MacBook Air", true, "192.168.4.46", true, true),
                m("Studio PC", false, "192.168.4.31", false, true),
                m("Old Laptop", false, "192.168.4.80", false, false),
            ];
            return;
        }
        let mine = fingerprint_hex(&self.device.fingerprint());
        let peers = self.device.peers();
        let me = PROTOCOL_VERSION.to_string();
        let mut list: Vec<Machine> = browse::list()
            .into_iter()
            .filter_map(|f| {
                let fp = f.fp.clone().unwrap_or_default().to_lowercase();
                if !fp.is_empty() && mine.starts_with(&fp) {
                    return None; // this PC itself
                }
                let mac = f.os.as_deref() == Some("mac");
                Some(Machine {
                    paired: !fp.is_empty()
                        && peers.iter().any(|p| {
                            p.roles & ROLE_HOST != 0
                                && fingerprint_hex(&p.fingerprint).starts_with(&fp)
                        }),
                    version_ok: f.version.as_deref().is_none_or(|v| v == me),
                    mac,
                    kind: match f.os.as_deref() {
                        Some("mac") => "Mac",
                        Some("windows") => "Windows PC",
                        _ => "Glass",
                    },
                    name: f.name,
                    address: f.address,
                    port: f.port,
                })
            })
            .collect();
        // Ready ones first, then by name.
        list.sort_by(|a, b| {
            (!a.version_ok, !a.paired, a.name.to_lowercase()).cmp(&(
                !b.version_ok,
                !b.paired,
                b.name.to_lowercase(),
            ))
        });
        self.machines = list;
    }

    fn connect(&mut self, host: String, port: u16) {
        if self.connecting.is_some() {
            return;
        }
        if let Some(dir) = glass_core::identity::default_dir() {
            let a = if port == DEFAULT_PORT {
                host.clone()
            } else {
                format!("{host}:{port}")
            };
            let _ = std::fs::write(dir.join("last-host.txt"), a);
        }
        self.connecting = Some(host.clone());
        self.viewing_name = self
            .machines
            .iter()
            .find(|m| m.address == host)
            .map_or(host.clone(), |m| m.name.clone());
        self.notice = None;
        let (wake, device, s) = (
            self.wake.clone(),
            self.device.clone(),
            self.settings.clone(),
        );
        std::thread::spawn(move || connect_worker(wake, device, host, port, None, s));
    }

    fn connect_typed(&mut self) {
        let text = self.address.trim().to_string();
        if text.is_empty() {
            return;
        }
        let (h, p) = match text.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().unwrap()),
            _ => (text, DEFAULT_PORT),
        };
        // A machine's name works too (addresses change; names don't): use
        // the address it has right now.
        if let Some(m) = self
            .machines
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(&h))
        {
            let (a, port) = (m.address.clone(), m.port);
            return self.connect(a, port);
        }
        self.connect(h, p);
    }

    fn toggle_share(&mut self) {
        if self.svc != super::service::State::NotInstalled && self.sharing.is_none() {
            // With the service installed, the switch turns the service's
            // sharing on and off instead (no admin prompt; kept across
            // restarts). A fully stopped service needs starting (admin).
            let on = self.svc == super::service::State::Running && !self.svc_off;
            if on {
                super::service::set_sharing(false);
                self.svc_off = true;
            } else if self.svc == super::service::State::Running {
                super::service::set_sharing(true);
                self.svc_off = false;
            } else {
                super::service::run_elevated("on");
            }
            return;
        }
        if let Some(stop) = self.sharing.take() {
            stop.store(true, Ordering::Relaxed);
            // Wake the listener so it sees `stop`.
            let _ = std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
                Duration::from_millis(500),
            );
            self.share_status = "Stopping \u{2026}".into();
            self.pin = None;
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (w1, w2, w3, w4) = (
            self.wake.clone(),
            self.wake.clone(),
            self.wake.clone(),
            self.wake.clone(),
        );
        let ui = super::HostUi {
            event: Box::new(move |e| w1.send(Msg::Host(e))),
            line: Box::new(move |l| w2.send(Msg::HostLine(l.to_string()))),
            report: Box::new(move |r| {
                save_report("results-host.txt", r);
                w3.send(Msg::Report(r.to_string()))
            }),
            stop: stop.clone(),
            end: self.end_session.clone(),
        };
        std::thread::spawn(move || {
            let why = match super::host_with(&[], ui) {
                Ok(()) => String::new(),
                Err(e) => format!("Sharing stopped: {e}"),
            };
            w4.send(Msg::HostStopped(why));
        });
        self.sharing = Some(stop);
        self.share_status = "Waiting for a viewer \u{2026}".into();
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        if self.sharing.is_some() {
            self.toggle_share(); // closes once the host has stopped
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn handle(&mut self, ctx: &egui::Context, m: Msg) {
        match m {
            Msg::AskPin(p, host, port) => {
                self.dialog = Dialog::Pin {
                    name: p.host_name().to_string(),
                    pairing: Some(p),
                    host,
                    port,
                    pin: String::new(),
                };
                show(ctx);
            }
            Msg::Failed(e) => {
                self.connecting = None;
                self.viewing = None;
                self.dialog = Dialog::Message("Couldn\u{2019}t connect".into(), e);
                show(ctx);
            }
            Msg::Report(r) => {
                self.connecting = None;
                self.viewing = None;
                self.notice = Some((
                    "Session ended. Results are saved next to Glass.exe.".into(),
                    Instant::now(),
                ));
                self.dialog = Dialog::Report(r, false);
                show(ctx);
            }
            Msg::Reconnecting => {
                self.notice = Some((
                    "The connection dropped: reconnecting \u{2026}".into(),
                    Instant::now(),
                ));
            }
            Msg::Status(s) => {
                // Connected: the row being connected now says Viewing (and
                // still does after a reconnect).
                if let Some(c) = self.connecting.take() {
                    self.viewing = Some(c);
                }
                self.notice = Some((s, Instant::now()));
            }
            Msg::Host(e) => self.host_event(ctx, e),
            Msg::HostLine(l) => {
                let l = l.trim();
                if [
                    "Viewer connected",
                    "Input blocked",
                    "Input works",
                    "Viewer switched",
                    "Waiting for a viewer",
                    "Session ended",
                ]
                .iter()
                .any(|p| l.starts_with(p))
                {
                    self.share_status = l.to_string();
                }
                if l.starts_with("Viewer connected") {
                    self.viewer_on = true;
                } else if l.starts_with("Session ended") || l.starts_with("Waiting for a viewer") {
                    self.viewer_on = false;
                }
            }
            Msg::HostStopped(why) => {
                self.viewer_on = false;
                self.sharing = None;
                self.pin = None;
                self.share_status = why;
                if self.quitting {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Msg::TrayOpen => show(ctx),
            Msg::TrayShare => self.toggle_share(),
            Msg::TrayQuit => self.quit(ctx),
        }
    }

    fn host_event(&mut self, ctx: &egui::Context, e: HostEvent) {
        match e {
            HostEvent::PairingPin { device, pin, .. } => {
                self.pin = Some((pin.to_string(), device.to_string()));
                show(ctx);
            }
            HostEvent::Paired { device } => {
                self.pin = None;
                self.share_status = format!("Paired with \u{201C}{device}\u{201D}");
            }
            HostEvent::WrongPin { device, left } => {
                self.share_status =
                    format!("Wrong code from \u{201C}{device}\u{201D} \u{00B7} {left} tries left")
            }
            HostEvent::PairingLocked => {
                self.pin = None;
                self.share_status =
                    "Too many wrong codes: pairing is off until sharing restarts".into();
            }
            HostEvent::Refused { addr, .. } => {
                self.share_status = format!("Refused an unpaired device ({})", addr.ip())
            }
            HostEvent::Denied { device } => {
                self.share_status = format!("Did not allow \u{201C}{device}\u{201D}")
            }
            HostEvent::Loop { device } => {
                self.share_status =
                    format!("Refused \u{201C}{device}\u{201D}: this PC is viewing it (a loop)")
            }
        }
    }
}

/// Connects (pairing first if needed, PIN from the UI), then views.
fn connect_worker(
    wake: Wake,
    device: Arc<Device>,
    host: String,
    port: u16,
    pin: Option<(Pairing, String)>,
    s: Settings,
) {
    if let Some((p, pin)) = pin {
        if let Err(e) = p.finish(&pin) {
            return wake.send(Msg::Failed(match e {
                ConnectError::WrongPin => {
                    "That code wasn\u{2019}t right (or it expired). Connect again for a new one."
                        .into()
                }
                e => format!("{host}: {e}"),
            }));
        }
    }
    let prep = match viewer::prepare(s.view, s.full_colour) {
        Ok(p) => p,
        Err(e) => return wake.send(Msg::Failed(format!("Can\u{2019}t start the viewer: {e}"))),
    };
    match Client::connect_with(&device, &host, port, prep.hello) {
        Ok(first) => {
            // When the connection drops without the host ending it (its agent
            // restarted, a Wi-Fi blip), reconnect quietly for up to 15 s and
            // carry on in the same view; only a real end shows the results.
            let (mut next, mut reports) = (Some((first, prep)), String::new());
            while let Some((c, prep)) = next.take() {
                wake.send(Msg::Status(format!(
                    "Viewing \u{201C}{}\u{201D}",
                    c.peer().name
                )));
                let opts = viewer::Options {
                    windowed: false,
                    seconds: None,
                    rule: None,
                    full_colour: s.full_colour,
                };
                let (report, m) = match viewer::run(c, prep, opts, &crate::banner()) {
                    Ok(r) => r,
                    Err(e) => return wake.send(Msg::Failed(format!("The viewer stopped: {e}"))),
                };
                save_report("results-viewer.txt", &report);
                reports.push_str(&report);
                let mode = m;
                if !report.contains("(ended: the connection to the host dropped)") {
                    break;
                }
                wake.send(Msg::Reconnecting);
                let until = Instant::now() + Duration::from_secs(15);
                while next.is_none() && Instant::now() < until {
                    std::thread::sleep(Duration::from_secs(1));
                    let Ok(prep) = viewer::prepare(mode, s.full_colour) else {
                        continue;
                    };
                    if let Ok(c) = Client::connect_with(&device, &host, port, prep.hello) {
                        reports.push_str("(reconnected after the connection dropped)\n\n");
                        next = Some((c, prep));
                    }
                }
                if next.is_none() {
                    reports.push_str("(couldn\u{2019}t reconnect within 15 s)\n");
                }
            }
            wake.send(Msg::Report(reports));
        }
        Err(ConnectError::NotPaired) => match pair_begin(&device, &host, port) {
            Ok(p) => wake.send(Msg::AskPin(p, host, port)),
            Err(e) => wake.send(Msg::Failed(format!("{host}: {e}"))),
        },
        Err(e) => wake.send(Msg::Failed(format!("{host}: {e}"))),
    }
}

// ---------------------------------------------------------------- widgets

fn card(ui: &mut egui::Ui, p: &Pal, add: impl FnOnce(&mut egui::Ui)) -> Rect {
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .response
        .rect
}

fn accent_button(text: &str, p: &Pal) -> egui::Button<'static> {
    egui::Button::new(
        RichText::new(text.to_string())
            .font(semibold(14.0))
            .color(Color32::WHITE),
    )
    .fill(p.accent)
    .stroke(Stroke::NONE)
    .corner_radius(CornerRadius::same(8))
    .min_size(vec2(96.0, 34.0))
}

fn plain_button(text: &str, p: &Pal) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(p.text))
        .fill(p.track)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(8))
        .min_size(vec2(84.0, 34.0))
}

fn icon_button(ui: &mut egui::Ui, glyph: &str, tip: &str, p: &Pal) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::click());
    let h = ui.ctx().animate_bool_responsive(resp.id, resp.hovered());
    ui.painter().rect_filled(rect, 8, p.track.gamma_multiply(h));
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        icon_font(16.0),
        p.text,
    );
    resp.on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// An on/off switch.
fn toggle(ui: &mut egui::Ui, on: bool, p: &Pal) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(46.0, 26.0), Sense::click());
    let t = ui.ctx().animate_bool_responsive(resp.id, on);
    let painter = ui.painter();
    painter.rect_filled(rect, 13, p.off.lerp_to_gamma(p.accent, t));
    let x = egui::lerp(rect.left() + 13.0..=rect.right() - 13.0, t);
    painter.circle_filled(
        pos2(x, rect.center().y + 0.5),
        10.0,
        Color32::from_black_alpha(30),
    );
    painter.circle_filled(pos2(x, rect.center().y), 10.0, Color32::WHITE);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// A pill-shaped segmented control. True if the value changed.
fn segmented<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    value: &mut T,
    opts: &[(T, &str)],
    p: &Pal,
) -> bool {
    let mut changed = false;
    // Equal segments across the full width.
    let w = ((ui.available_width() - 6.0 - 2.0 * (opts.len() as f32 - 1.0)) / opts.len() as f32)
        .floor();
    egui::Frame::new()
        .fill(p.track)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for (v, label) in opts {
                    let sel = *value == *v;
                    let b = egui::Button::new(RichText::new(*label).color(if sel {
                        p.strong
                    } else {
                        p.weak
                    }))
                    .fill(if sel { p.card } else { Color32::TRANSPARENT })
                    .stroke(if sel {
                        Stroke::new(1.0, p.border)
                    } else {
                        Stroke::NONE
                    })
                    .corner_radius(CornerRadius::same(8))
                    .min_size(vec2(w, 28.0));
                    if ui.add(b).clicked() && !sel {
                        *value = *v;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// A monitor (PC) or a laptop (Mac), line-drawn around `c`.
fn device_glyph(painter: &egui::Painter, c: egui::Pos2, mac: bool, col: Color32) {
    let s = Stroke::new(1.8, col);
    if mac {
        let scr = Rect::from_center_size(c + vec2(0.0, -2.5), vec2(24.0, 16.0));
        painter.rect_stroke(scr, 2.5, s, StrokeKind::Inside);
        painter.line_segment(
            [c + vec2(-15.0, 8.0), c + vec2(15.0, 8.0)],
            Stroke::new(2.4, col),
        );
    } else {
        let scr = Rect::from_center_size(c + vec2(0.0, -3.5), vec2(26.0, 17.0));
        painter.rect_stroke(scr, 2.5, s, StrokeKind::Inside);
        painter.line_segment([c + vec2(0.0, 5.0), c + vec2(0.0, 9.0)], s);
        painter.line_segment([c + vec2(-6.5, 9.5), c + vec2(6.5, 9.5)], s);
    }
}

fn section(ui: &mut egui::Ui, text: &str, p: &Pal) {
    ui.label(RichText::new(text).font(semibold(14.0)).color(p.strong));
}

fn modal(id: &str, p: &Pal) -> egui::Modal {
    egui::Modal::new(egui::Id::unique(id))
        .backdrop_color(Color32::from_black_alpha(110))
        .frame(
            egui::Frame::new()
                .fill(p.card)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(16))
                .inner_margin(Margin::same(22))
                .shadow(egui::Shadow {
                    offset: [0, 12],
                    blur: 36,
                    spread: 0,
                    color: Color32::from_black_alpha(80),
                }),
        )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}\u{2026}", s.chars().take(max - 1).collect::<String>())
    }
}

// ---------------------------------------------------------------- the window

impl GlassApp {
    fn header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: &Pal) {
        ui.horizontal(|ui| {
            if let Some(t) = &self.icon {
                ui.add(
                    egui::Image::from_texture(egui::load::SizedTexture::from_handle(t))
                        .fit_to_exact_size(vec2(42.0, 42.0)),
                );
            }
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("Glass").font(semibold(22.0)).color(p.strong));
                ui.label(
                    RichText::new(format!("This PC \u{00B7} {}", self.device.name))
                        .size(13.0)
                        .color(p.weak),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let gear = icon_button(ui, &ic('\u{E713}', "\u{2699}"), "Settings", p);
                self.gear_rect = Some(gear.rect);
                if gear.clicked() {
                    // The gear toggles the settings panel (as on the Mac).
                    self.dialog = if matches!(self.dialog, Dialog::Settings) {
                        Dialog::None
                    } else {
                        Dialog::Settings
                    };
                }
                let dark = ui.visuals().dark_mode;
                let (glyph, tip) = if dark {
                    (ic('\u{E706}', "\u{2600}"), "Switch to light")
                } else {
                    (ic('\u{E708}', "\u{263E}"), "Switch to dark")
                };
                if icon_button(ui, &glyph, tip, p).clicked() {
                    self.settings.theme = if dark {
                        ThemePreference::Light
                    } else {
                        ThemePreference::Dark
                    };
                    ctx.set_theme(self.settings.theme);
                    self.settings.save();
                }
            });
        });
    }

    /// One device: click anywhere to connect. True if clicked.
    fn machine_row(&self, ui: &mut egui::Ui, m: &Machine, p: &Pal) -> bool {
        let busy_here = self.connecting.as_deref() == Some(m.address.as_str());
        let viewing = self.viewing.as_deref() == Some(m.address.as_str());
        // One session at a time: while viewing, only that row is live (it
        // disconnects).
        let enabled =
            viewing || (m.version_ok && self.connecting.is_none() && self.session().is_none());
        let (rect, resp) = ui.allocate_exact_size(
            vec2(ui.available_width(), 70.0),
            if enabled {
                Sense::click()
            } else {
                Sense::hover()
            },
        );
        let h = ui
            .ctx()
            .animate_bool_responsive(resp.id.with("hover"), resp.hovered() && enabled);
        let painter = ui.painter();
        painter.rect_filled(rect, 14, p.card.lerp_to_gamma(p.hover, h));
        painter.rect_stroke(
            rect,
            14,
            Stroke::new(1.0, p.border.lerp_to_gamma(p.accent.gamma_multiply(0.7), h)),
            StrokeKind::Inside,
        );
        let fade = if m.version_ok && (enabled || busy_here) {
            1.0
        } else {
            0.5
        };
        let tile =
            Rect::from_center_size(pos2(rect.left() + 38.0, rect.center().y), vec2(44.0, 44.0));
        painter.rect_filled(tile, 11, p.accent.gamma_multiply(0.13 * fade));
        device_glyph(painter, tile.center(), m.mac, p.accent.gamma_multiply(fade));
        let x = tile.right() + 14.0;
        painter.text(
            pos2(x, rect.center().y - 10.0),
            Align2::LEFT_CENTER,
            truncate(&m.name, 26),
            semibold(16.0),
            p.strong.gamma_multiply(fade),
        );
        painter.text(
            pos2(x, rect.center().y + 11.0),
            Align2::LEFT_CENTER,
            format!("{} \u{00B7} {}", m.kind, m.address),
            FontId::proportional(13.0),
            p.weak,
        );
        let right = pos2(rect.right() - 16.0, rect.center().y);
        if busy_here {
            let r = Rect::from_center_size(right - vec2(10.0, 0.0), vec2(20.0, 20.0));
            ui.put(r, egui::Spinner::new().size(20.0).color(p.accent));
        } else {
            // Status, crossfading to a Connect pill on hover.
            let (dot, label) = if viewing {
                (p.ok, "Viewing")
            } else if !m.version_ok {
                (p.weak, "Update Glass")
            } else if m.paired {
                (p.ok, "Ready")
            } else {
                (p.warn, "Pair")
            };
            let a = 1.0 - h;
            if a > 0.01 {
                let r = painter.text(
                    right,
                    Align2::RIGHT_CENTER,
                    label,
                    FontId::proportional(13.0),
                    p.weak.gamma_multiply(a),
                );
                painter.circle_filled(pos2(r.left() - 9.0, right.y), 4.0, dot.gamma_multiply(a));
            }
            if h > 0.01 {
                let action = if viewing { "Disconnect" } else { "Connect" };
                let g = painter.layout_no_wrap(action.to_string(), semibold(13.0), Color32::WHITE);
                let pill = Rect::from_min_max(
                    pos2(right.x - g.size().x - 28.0, right.y - 15.0),
                    pos2(right.x, right.y + 15.0),
                );
                let fill = if viewing { p.danger } else { p.accent };
                painter.rect_filled(pill, 15, fill.gamma_multiply(h));
                painter.text(
                    pill.center(),
                    Align2::CENTER_CENTER,
                    action,
                    semibold(13.0),
                    Color32::WHITE.gamma_multiply(h),
                );
            }
        }
        let resp = if enabled {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else if !m.version_ok {
            resp.on_hover_text(
                "This device runs a different version of Glass. Install the same build on both.",
            )
        } else {
            resp
        };
        resp.clicked()
    }

    fn empty_state(&self, ui: &mut egui::Ui, p: &Pal) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 210.0), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 14, p.card);
        painter.rect_stroke(rect, 14, Stroke::new(1.0, p.border), StrokeKind::Inside);
        let c = pos2(rect.center().x, rect.top() + 78.0);
        // During a session it's still (no searching animation) and says so.
        let svc_connected = self.svc_status.as_ref().is_some_and(|s| s.connected);
        let in_session = self.viewing.is_some() || self.viewer_on || svc_connected;
        let col = if in_session { p.ok } else { p.accent };
        if !in_session {
            let t = ui.input(|i| i.time) as f32;
            for i in 0..3 {
                let phase = (t * 0.45 + i as f32 / 3.0).fract();
                painter.circle_stroke(
                    c,
                    24.0 + phase * 44.0,
                    Stroke::new(1.5, p.accent.gamma_multiply((1.0 - phase) * 0.55)),
                );
            }
        }
        painter.circle_filled(c, 24.0, col.gamma_multiply(0.14));
        device_glyph(painter, c, false, col);
        let (title, sub) = if in_session {
            (
                "In a session".to_string(),
                "Other devices appear here when the session ends.".to_string(),
            )
        } else {
            (
                "Looking for devices \u{2026}".to_string(),
                "Turn on sharing on your Mac or PC and it appears here.".to_string(),
            )
        };
        painter.text(
            pos2(c.x, rect.top() + 150.0),
            Align2::CENTER_CENTER,
            title,
            semibold(15.0),
            p.strong,
        );
        painter.text(
            pos2(c.x, rect.top() + 174.0),
            Align2::CENTER_CENTER,
            sub,
            FontId::proportional(13.0),
            p.weak,
        );
        if !in_session {
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
    }

    /// Ends the session this window is in: the one being viewed here, or the
    /// one viewing this PC (sharing stays on).
    /// The session bar's button. Viewing: Disconnect (leave). Being
    /// viewed: Stop sharing, the same as switching Allow connections off
    /// (one idea per side; reconnecting a paired device right after an
    /// "end only this visit" made the difference meaningless).
    fn end_session(&mut self) {
        if self.viewing.is_some() {
            disconnect_viewer();
        } else if self.sharing.is_some() || self.svc == super::service::State::Running {
            self.toggle_share();
        }
    }

    /// The session this window is in, if any: what to say and the button.
    fn session(&self) -> Option<(String, &'static str)> {
        // "Viewer connected: \"NAME\" from 1.2.3.4 (encrypted)" -> NAME
        let who =
            |line: &str| -> String { line.split('"').nth(1).unwrap_or("A device").to_string() };
        if self.viewing.is_some() {
            Some((
                format!("Viewing \u{201C}{}\u{201D}", self.viewing_name),
                "Disconnect",
            ))
        } else if self.viewer_on {
            Some((
                format!(
                    "\u{201C}{}\u{201D} is viewing this PC",
                    who(&self.share_status)
                ),
                "Stop sharing",
            ))
        } else {
            let s = self.svc_status.as_ref().filter(|s| s.connected)?;
            Some((
                format!("\u{201C}{}\u{201D} is viewing this PC", who(&s.line)),
                "Stop sharing",
            ))
        }
    }

    /// A bar at the top for the whole session, with the way out.
    fn session_bar(&mut self, ui: &mut egui::Ui, p: &Pal) {
        let Some((title, button)) = self.session() else {
            return;
        };
        let mut end = false;
        egui::Frame::new()
            .fill(p.ok.gamma_multiply(0.12))
            .stroke(Stroke::new(1.0, p.ok.gamma_multiply(0.5)))
            .corner_radius(CornerRadius::same(14))
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(vec2(10.0, 36.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 5.0, p.ok);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(
                            RichText::new(truncate(&title, 34))
                                .font(semibold(15.0))
                                .color(p.strong),
                        );
                        ui.label(
                            RichText::new("Encrypted \u{00B7} one session at a time")
                                .size(12.5)
                                .color(p.weak),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let b = egui::Button::new(
                            RichText::new(button)
                                .font(semibold(13.5))
                                .color(Color32::WHITE),
                        )
                        .fill(p.danger)
                        .stroke(Stroke::NONE)
                        .corner_radius(CornerRadius::same(8))
                        .min_size(vec2(110.0, 32.0));
                        if ui.add(b).clicked() {
                            end = true;
                        }
                    });
                });
            });
        if end {
            self.end_session();
        }
    }

    fn share_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: &Pal) {
        let svc_installed = self.svc != super::service::State::NotInstalled;
        let svc_on = self.svc == super::service::State::Running && !self.svc_off;
        let on = svc_on || self.sharing.is_some() || self.demo.as_deref() == Some("share");
        ui.horizontal(|ui| {
            let (tile, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
            let col = if on { p.ok } else { p.weak };
            ui.painter().rect_filled(tile, 11, col.gamma_multiply(0.14));
            device_glyph(ui.painter(), tile.center(), false, col);
            ui.add_space(4.0);
            // The text gets what's left beside the switch, and is cut off
            // neatly rather than running under it.
            let text_w = (ui.available_width() - 62.0).max(80.0);
            ui.allocate_ui_with_layout(vec2(text_w, 46.0), Layout::top_down(Align::Min), |ui| {
                ui.set_max_width(text_w);
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(
                        RichText::new("Allow connections")
                            .font(semibold(16.0))
                            .color(p.strong),
                    );
                    if svc_installed {
                        egui::Frame::new()
                            .fill((if svc_on { p.ok } else { p.weak }).gamma_multiply(0.15))
                            .corner_radius(CornerRadius::same(8))
                            .inner_margin(Margin::symmetric(7, 1))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new("Service")
                                        .size(11.5)
                                        .color(if svc_on { p.ok } else { p.weak }),
                                );
                            });
                    }
                });
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let (dot, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 4.0, if on { p.ok } else { p.off });
                    // "Viewer connected: "NAME" from ..." reads as "NAME is viewing".
                    let short = |line: &str| -> String {
                        if line.starts_with("Viewer connected") {
                            let who = line.split('"').nth(1).unwrap_or("A device");
                            format!("\u{201C}{who}\u{201D} is viewing")
                        } else {
                            line.to_string()
                        }
                    };
                    // Whether others may connect (the door), not who is
                    // connected: that's the session bar at the top. Only
                    // warnings about sharing show here.
                    let warning = ["Input blocked", "Wrong code", "Too many", "Refused", "Did not allow", "Sharing stopped"]
                        .iter()
                        .any(|w| self.share_status.starts_with(w));
                    let text = if warning {
                        short(&self.share_status)
                    } else if svc_on {
                        "Open at all times, even when locked".to_string()
                    } else if on {
                        "Open to your paired devices".to_string()
                    } else {
                        "Closed \u{00B7} nobody can connect".to_string()
                    };
                    ui.add(egui::Label::new(RichText::new(text).size(13.0).color(p.weak)).truncate());
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if svc_installed {
                    // The switch turns the service's sharing on and off.
                    if toggle(ui, svc_on, p)
                        .on_hover_text("Open: your paired devices can connect at any time, including the lock screen and UAC prompts (the Glass service). Closed: nobody can (it stays closed after a restart).")
                        .clicked()
                    {
                        self.toggle_share();
                    }
                } else {
                    let r = toggle(ui, on, p);
                    let blocked = !on && self.viewing.is_some();
                    let r = if blocked {
                        r.on_hover_text("Not while you\u{2019}re viewing another device: one session at a time.")
                    } else {
                        r
                    };
                    if r.clicked() && self.demo.is_none() && !blocked {
                        self.toggle_share();
                    }
                }
            });
        });
        let pin = if svc_on {
            self.svc_status.as_ref().and_then(|s| s.pin.clone())
        } else {
            self.pin.clone()
        };
        if let Some((digits, who)) = &pin {
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(format!(
                        "Pairing code for \u{201C}{}\u{201D}",
                        truncate(who, 28)
                    ))
                    .font(semibold(14.0))
                    .color(p.strong),
                );
                ui.label(
                    RichText::new("Type it on that device. Pair only your own devices.")
                        .size(12.5)
                        .color(p.weak),
                );
            });
            ui.add_space(6.0);
            let (tw, th, gap, mid) = (38.0, 50.0, 7.0, 14.0);
            let n = digits.chars().count() as f32;
            let total = n * tw + (n - 1.0) * gap + mid - gap;
            let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), th), Sense::hover());
            let mut x = row.center().x - total / 2.0;
            for (i, ch) in digits.chars().enumerate() {
                if i == 3 {
                    x += mid - gap;
                }
                let r = Rect::from_min_size(pos2(x, row.top()), vec2(tw, th));
                ui.painter().rect_filled(r, 9, p.track);
                ui.painter().text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    ch,
                    semibold(26.0),
                    p.strong,
                );
                x += tw + gap;
            }
            ui.add_space(2.0);
        }
        if self.remote {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(
                    RichText::new(ic('\u{E7BA}', "\u{26A0}"))
                        .font(icon_font(12.0))
                        .color(p.warn),
                );
                if svc_on {
                    ui.label(
                        RichText::new("You\u{2019}re using this PC through Remote Desktop. Viewers see its own screen, which stays locked until you sign in there.")
                            .size(12.5)
                            .color(p.weak),
                    );
                } else {
                    ui.label(
                        RichText::new("You\u{2019}re using this PC through Remote Desktop: Glass can\u{2019}t show this session.")
                            .size(12.5)
                            .color(p.weak),
                    );
                    let link = ui.add(
                        egui::Label::new(
                            RichText::new("Move it to the PC\u{2019}s screen")
                                .size(12.5)
                                .color(p.accent),
                        )
                        .sense(Sense::click()),
                    );
                    if link
                        .on_hover_text("Runs tscon as administrator: Remote Desktop disconnects and this session carries on, unlocked, on the PC\u{2019}s own screen, where Glass can show it.")
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .clicked()
                    {
                        super::service::move_session_to_console();
                    }
                }
            });
        }
        if on && !svc_on && !self.elevated {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(
                    RichText::new(ic('\u{E7BA}', "\u{26A0}"))
                        .font(icon_font(12.0))
                        .color(p.warn),
                );
                ui.label(
                    RichText::new(
                        "Admin windows (Task Manager, installers) ignore remote control.",
                    )
                    .size(12.5)
                    .color(p.weak),
                );
                let link = ui.add(
                    egui::Label::new(
                        RichText::new("Restart as administrator")
                            .size(12.5)
                            .color(p.accent),
                    )
                    .sense(Sense::click()),
                );
                if link.on_hover_cursor(CursorIcon::PointingHand).clicked()
                    && restart_elevated(true)
                {
                    self.quit(ctx);
                }
            });
        }
    }
}

impl eframe::App for GlassApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(m) = self.rx.try_recv() {
            self.handle(ctx, m);
        }
        // Closing the window while sharing keeps Glass in the tray.
        if ctx.input(|i| i.viewport().close_requested()) && self.sharing.is_some() && !self.quitting
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        if self.last_scan.elapsed() > Duration::from_millis(1500) {
            self.scan();
            if self.demo.as_deref() != Some("service") {
                self.svc = super::service::state();
                self.svc_off = super::service::sharing_off();
            }
            self.svc_status = if self.demo.as_deref() == Some("service") {
                self.svc_status.clone()
            } else if self.svc == super::service::State::Running {
                super::service::Status::read()
            } else {
                None
            };
        }
        ctx.request_repaint_after(Duration::from_millis(1500));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let p = pal(ui.visuals().dark_mode);
        if self.icon.is_none() {
            self.icon = Some(ctx.load_texture(
                "glass-icon",
                egui::ColorImage::from_rgba_unmultiplied([64, 64], &icon_rgba(64)),
                egui::TextureOptions::LINEAR,
            ));
        }

        egui::Frame::new()
            .fill(p.bg)
            .inner_margin(Margin {
                left: 22,
                right: 22,
                top: 18,
                bottom: 18,
            })
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                self.header(ui, &ctx, &p);
                ui.add_space(16.0);
                if self.session().is_some() {
                    self.session_bar(ui, &p);
                    ui.add_space(12.0);
                }

                // Devices.
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("CONNECT TO")
                            .font(semibold(12.0))
                            .color(p.weak),
                    );
                    if !self.machines.is_empty() {
                        ui.label(
                            RichText::new(self.machines.len().to_string())
                                .size(12.0)
                                .color(p.weak),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, &ic('\u{E72C}', "\u{21BB}"), "Look again", &p).clicked()
                        {
                            let _ = browse::start();
                            self.scan();
                        }
                    });
                });
                let list_h = (ui.available_height() - self.share_h - 64.0).max(90.0);
                let mut go = None;
                egui::ScrollArea::vertical()
                    .max_height(list_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if self.machines.is_empty() {
                            self.empty_state(ui, &p);
                        }
                        for m in &self.machines {
                            if self.machine_row(ui, m, &p) {
                                go = Some((m.address.clone(), m.port));
                            }
                        }
                    });
                if let Some((h, port)) = go {
                    if self.viewing.as_deref() == Some(h.as_str()) {
                        disconnect_viewer();
                    } else {
                        self.connect(h, port);
                    }
                }

                // A typed address, tucked away.
                // The link stays and toggles it (same as the Mac); Esc closes it too.
                // Off during a session (one at a time), and the box closes.
                ui.add_space(2.0);
                let live = self.session().is_some();
                if live {
                    self.show_address = false;
                }
                let link = ui.add(
                    egui::Label::new(
                        RichText::new(if self.show_address {
                            "\u{2212}  Connect by address"
                        } else {
                            "+  Connect by address"
                        })
                        .size(13.5)
                        .color(if live {
                            p.weak.gamma_multiply(0.6)
                        } else {
                            p.accent
                        }),
                    )
                    .sense(if live { Sense::hover() } else { Sense::click() }),
                );
                if !live && link.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    self.show_address = !self.show_address;
                }
                if self.show_address && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.show_address = false;
                }
                if self.show_address {
                    ui.horizontal(|ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.address)
                                .hint_text("Name or address, e.g. 192.168.1.20")
                                .margin(Margin::symmetric(10, 8))
                                .desired_width(ui.available_width() - 106.0),
                        );
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui
                            .add_enabled(
                                self.connecting.is_none() && self.session().is_none(),
                                accent_button("Connect", &p),
                            )
                            .clicked()
                            || enter
                        {
                            self.connect_typed();
                        }
                    });
                }
                if let Some((n, at)) = &self.notice {
                    if at.elapsed() < Duration::from_secs(10) {
                        ui.label(RichText::new(n).size(13.0).color(p.weak));
                    }
                }

                // Sharing, at the bottom.
                let space = ui.available_height() - self.share_h - 2.0;
                if space > 0.0 {
                    ui.add_space(space);
                }
                let r = card(ui, &p, |ui| self.share_card(ui, &ctx, &p));
                self.share_h = r.height();
            });

        self.dialogs(&ctx, &p);

        // GLASS_DEBUG_SHOT=FILE.bmp: save the window after 3 s and quit (checks).
        // egui's own capture: screen copies can't see an OpenGL window.
        if let Some(t) = self.shot_at {
            if t.elapsed() > Duration::from_secs(3) {
                self.shot_at = None;
                let path = std::env::var_os("GLASS_DEBUG_SHOT").unwrap_or_default();
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                    egui::ScreenshotCallback::new(move |img| {
                        let (w, h) = (img.size[0] as u32, img.size[1] as u32);
                        let mut b = Vec::with_capacity(54 + img.pixels.len() * 4);
                        b.extend_from_slice(b"BM");
                        b.extend_from_slice(&(54 + w * h * 4).to_le_bytes());
                        b.extend_from_slice(&[0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0]);
                        b.extend_from_slice(&w.to_le_bytes());
                        b.extend_from_slice(&(-(h as i32)).to_le_bytes());
                        b.extend_from_slice(&[1, 0, 32, 0]);
                        b.extend_from_slice(&[0; 24]);
                        for p in &img.pixels {
                            b.extend_from_slice(&[p.b(), p.g(), p.r(), 255]);
                        }
                        let _ = std::fs::write(&path, b);
                        tray_send(Msg::TrayQuit);
                    }),
                ));
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
    }
}

impl GlassApp {
    fn dialogs(&mut self, ctx: &egui::Context, p: &Pal) {
        let mut close = false;
        // GLASS_DEBUG_DEMO=report presses Copy itself (clipboard check).
        let auto_copy = self.demo.as_deref() == Some("report");
        match &mut self.dialog {
            Dialog::None => {}
            Dialog::Pin {
                pairing,
                host,
                port,
                pin,
                name,
            } => {
                let mut submit = None;
                modal("pin", p).show(ctx, |ui| {
                    ui.set_width(360.0);
                    ui.label(RichText::new("Enter pairing code").font(semibold(20.0)).color(p.strong));
                    ui.label(
                        RichText::new(format!("\u{201C}{name}\u{201D} is showing a 6-digit code."))
                            .color(p.text),
                    );
                    ui.add_space(8.0);
                    ui.vertical_centered(|ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(pin)
                                .hint_text("000 000")
                                .font(semibold(28.0))
                                .align(Align2::CENTER_CENTER)
                                .char_limit(7)
                                .margin(Margin::symmetric(12, 10))
                                .desired_width(220.0),
                        );
                        r.request_focus();
                    });
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Pair only your own devices: a paired host sees what you type while you view it.")
                            .size(12.5)
                            .color(p.weak),
                    );
                    ui.add_space(8.0);
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(accent_button("Pair", p)).clicked() || enter {
                            submit = Some(true);
                        }
                        if ui.add(plain_button("Cancel", p)).clicked() || esc {
                            submit = Some(false);
                        }
                    });
                });
                if let Some(ok) = submit {
                    let digits: String = pin.chars().filter(char::is_ascii_digit).collect();
                    match (ok && !digits.is_empty(), pairing.take()) {
                        (true, Some(pr)) => {
                            let (wake, device, s, h, pt) = (
                                self.wake.clone(),
                                self.device.clone(),
                                self.settings.clone(),
                                host.clone(),
                                *port,
                            );
                            std::thread::spawn(move || {
                                connect_worker(wake, device, h, pt, Some((pr, digits)), s)
                            });
                        }
                        _ => self.connecting = None,
                    }
                    close = true;
                }
            }
            Dialog::Report(text, copied) => {
                modal("report", p).show(ctx, |ui| {
                    ui.set_width((ctx.content_rect().width() - 60.0).clamp(300.0, 720.0));
                    ui.label(
                        RichText::new("Session results")
                            .font(semibold(20.0))
                            .color(p.strong),
                    );
                    ui.label(
                        RichText::new("Copy these and paste them back to Claude.").color(p.weak),
                    );
                    ui.add_space(6.0);
                    egui::Frame::new()
                        .fill(p.track)
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            egui::ScrollArea::both().max_height(320.0).show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(text.as_str()).monospace().size(12.0),
                                    )
                                    .extend()
                                    .selectable(true),
                                );
                            });
                        });
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(plain_button("Close", p)).clicked() {
                            close = true;
                        }
                        let label = if *copied {
                            "Copied \u{2713}"
                        } else {
                            "Copy results"
                        };
                        if ui
                            .add(accent_button(label, p).min_size(vec2(130.0, 34.0)))
                            .clicked()
                            || (auto_copy && !*copied)
                        {
                            ctx.copy_text(text.clone());
                            *copied = true;
                        }
                    });
                });
            }
            Dialog::Message(title, text) => {
                modal("msg", p).show(ctx, |ui| {
                    ui.set_width(360.0);
                    ui.label(
                        RichText::new(title.as_str())
                            .font(semibold(20.0))
                            .color(p.strong),
                    );
                    ui.label(RichText::new(text.as_str()).color(p.text));
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(accent_button("OK", p)).clicked()
                            || ui.input(|i| {
                                i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape)
                            })
                        {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Settings => {
                let mut s = self.settings.clone();
                let peers: Vec<Peer> = self.device.peers();
                let mut forget = None;
                // A panel under the gear (like the Mac's popover), not a
                // modal: a click outside it or Esc closes it.
                let anchor = self.gear_rect;
                let at = anchor.map_or(pos2(ctx.content_rect().right() - 16.0, 64.0), |r| {
                    pos2(r.right(), r.bottom() + 6.0)
                });
                let shown = egui::Area::new(egui::Id::unique("settings-panel"))
                    .order(egui::Order::Foreground)
                    .pivot(Align2::RIGHT_TOP)
                    .fixed_pos(at)
                    .constrain(true)
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(p.card)
                            .stroke(Stroke::new(1.0, p.border))
                            .corner_radius(CornerRadius::same(14))
                            .inner_margin(Margin::same(18))
                            .shadow(egui::Shadow {
                                offset: [0, 10],
                                blur: 30,
                                spread: 0,
                                color: Color32::from_black_alpha(70),
                            })
                            .show(ui, |ui| {
                    // Scrolls when it's taller than the window.
                    let max_h = (ctx.content_rect().bottom() - at.y - 60.0).max(240.0);
                    // A scroll bar with its own lane, not floating over the text.
                    ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                    egui::ScrollArea::vertical().max_height(max_h).min_scrolled_height(max_h).show(ui, |ui| {
                    ui.set_width(380.0);
                    // A visible way out (Esc and a click outside work too).
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Settings").font(semibold(18.0)).color(p.strong));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if icon_button(ui, &ic('\u{E711}', "\u{2715}"), "Close", p).clicked() {
                                close = true;
                            }
                        });
                    });
                    ui.add_space(4.0);

                    section(ui, "Appearance", p);
                    segmented(
                        ui,
                        &mut s.theme,
                        &[
                            (ThemePreference::System, "System"),
                            (ThemePreference::Light, "Light"),
                            (ThemePreference::Dark, "Dark"),
                        ],
                        p,
                    );
                    ui.add_space(10.0);

                    section(ui, "When viewing, start in", p);
                    // Radio rows, not a drop-down: a pop-up inside the panel
                    // would count as a click outside it.
                    for m in [MODE_FIT, MODE_FIT_MORE_SPACE, MODE_FIT_LARGER, MODE_HOST_SCREEN] {
                        ui.radio_value(&mut s.view, m, RichText::new(viewer::mode_name(m)).color(p.text));
                    }
                    ui.add_space(4.0);
                    section(ui, "Colour", p);
                    segmented(
                        ui,
                        &mut s.full_colour,
                        &[(false, "Fast \u{00B7} 4:2:0"), (true, "Full colour \u{00B7} 4:4:4")],
                        p,
                    );
                    ui.label(
                        RichText::new("Fast has the lowest lag. Full colour is exact but adds some lag. Both can be switched while viewing, from the handle at the top of the viewer.")
                            .size(12.5)
                            .color(p.weak),
                    );
                    ui.add_space(10.0);

                    section(ui, "Unattended access", p);
                    let svc = self.svc;
                    ui.label(
                        RichText::new(match svc {
                            super::service::State::Running if !self.svc_off => "Installed and on: the Glass service shares this PC at all times, including the lock screen, sign-in and UAC prompts. The Share this PC switch turns it off and on.",
                            super::service::State::Running | super::service::State::Stopped => "Installed, sharing off: nobody can connect until the Share this PC switch turns it on again.",
                            super::service::State::NotInstalled => "Install the Glass service to use this PC from far away: it shares at all times, including the lock screen, sign-in and UAC prompts. Windows asks for administrator rights.",
                        })
                        .size(12.5)
                        .color(p.weak),
                    );
                    ui.horizontal(|ui| {
                        match svc {
                            super::service::State::NotInstalled => {
                                if ui.add(accent_button("Install service \u{2026}", p)).clicked() {
                                    super::service::run_elevated("install");
                                }
                            }
                            _ => {
                                if ui.add(plain_button("Update to this build \u{2026}", p)).clicked() {
                                    super::service::run_elevated("install");
                                }
                                if ui.add(plain_button("Remove service \u{2026}", p)).clicked() {
                                    super::service::run_elevated("uninstall");
                                }
                            }
                        }
                    });
                    ui.add_space(10.0);

                    section(ui, "Paired devices", p);
                    if peers.is_empty() {
                        ui.label(RichText::new("None yet.").color(p.weak));
                    }
                    for peer in &peers {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.label(RichText::new(truncate(&peer.name, 32)).color(p.strong));
                                let mut roles = Vec::new();
                                if peer.roles & ROLE_VIEWER != 0 {
                                    roles.push("can view this PC");
                                }
                                if peer.roles & ROLE_HOST != 0 {
                                    roles.push("this PC can view it");
                                }
                                ui.label(RichText::new(roles.join(" \u{00B7} ")).size(12.5).color(p.weak));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let f = ui.add(
                                    egui::Label::new(RichText::new("Forget").color(p.danger)).sense(Sense::click()),
                                );
                                if f.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                                    forget = Some(peer.fingerprint);
                                }
                            });
                        });
                    }
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!(
                            "This PC \u{201C}{}\u{201D} \u{00B7} id {} \u{00B7} protocol v{}",
                            self.device.name,
                            fingerprint_short(&self.device.fingerprint()),
                            PROTOCOL_VERSION
                        ))
                        .size(12.0)
                        .color(p.weak),
                    );
                    });
                            });
                    })
                    .response
                    .rect;
                let outside = ctx.input(|i| {
                    i.pointer.any_pressed()
                        && i.pointer.interact_pos().is_some_and(|q| {
                            !shown.contains(q) && !anchor.is_some_and(|a| a.contains(q))
                        })
                });
                if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                if s != self.settings {
                    if s.theme != self.settings.theme {
                        ctx.set_theme(s.theme);
                    }
                    self.settings = s;
                    self.settings.save();
                }
                if let Some(fp) = forget {
                    let _ = self.device.forget(&fp);
                    self.scan();
                }
            }
        }
        if close {
            self.dialog = Dialog::None;
        }
    }
}

/// Ends the session being viewed: closes the viewer window (as its own
/// Disconnect does); the worker then reports the results.
fn disconnect_viewer() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};
    unsafe {
        if let Ok(h) = FindWindowW(windows::core::w!("GlassViewer"), None) {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Starts an elevated copy of Glass (Windows asks). True if it started.
fn restart_elevated(share: bool) -> bool {
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let e: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(e.as_ptr()),
            if share { w!("--share") } else { w!("") },
            None,
            SW_SHOWNORMAL,
        )
    };
    r.0 as isize > 32
}

//! The Glass window on Windows (M3d): `Glass.exe`. Built with egui for a
//! modern look that follows Windows' light/dark theme (or a chosen one).
//!
//! - Machines with Glass on the network as cards (Mac/PC, paired or not),
//!   each with Connect; or connect to a typed address.
//! - Share this PC, with the pairing PIN shown large.
//! - Pairing PIN entry, paired devices with Forget, settings, session results
//!   with Copy, and a tray icon (closing while sharing keeps Glass running).
//!
//! The command line (`glass-cli host`, `glass-cli connect`, ...) stays for
//! tests and CI.
//!
//! Threads: egui's on the main thread; connecting, pairing and each viewer
//! session on workers (the viewer opens its own window); sharing runs
//! `host_with` on its own thread. Workers report back on a channel and wake
//! the UI with `request_repaint`.

use super::{browse, viewer, Res};
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, ThemePreference};
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

/// A machine card.
#[derive(Clone)]
struct Machine {
    name: String,
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
    Report(String),
    Paired,
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
    status: String,
    connecting: bool,
    sharing: Option<Arc<AtomicBool>>,
    share_status: String,
    pin: Option<(String, String)>,
    dialog: Dialog,
    quitting: bool,
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
            .with_inner_size([640.0, 700.0])
            .with_min_inner_size([500.0, 540.0])
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
            let mut app = GlassApp {
                status: format!(
                    "This PC: \u{201C}{}\u{201D}  \u{00B7}  id {}",
                    device.name,
                    fingerprint_short(&device.fingerprint())
                ),
                device,
                wake,
                rx,
                settings,
                machines: Vec::new(),
                last_scan: Instant::now() - Duration::from_secs(10),
                address,
                connecting: false,
                sharing: None,
                share_status: String::new(),
                pin: None,
                dialog: Dialog::None,
                quitting: false,
                _tray: tray,
                shot_at: std::env::var_os("GLASS_DEBUG_SHOT").map(|_| Instant::now()),
            };
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

fn style(ctx: &egui::Context) {
    // Windows' own UI font (Segoe UI Variable on 11, Segoe UI on 10) when present.
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    if let Some(bytes) = ["SegUIVar.ttf", "segoeui.ttf"]
        .iter()
        .find_map(|f| std::fs::read(format!(r"{windir}\Fonts\{f}")).ok())
    {
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("segoe".into(), Arc::new(egui::FontData::from_owned(bytes)));
        if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            f.insert(0, "segoe".into());
        }
        ctx.set_fonts(fonts);
    }
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(10.0, 8.0);
        s.spacing.button_padding = egui::vec2(14.0, 6.0);
        s.spacing.interact_size.y = 30.0;
        for f in s.text_styles.values_mut() {
            f.size *= 1.1;
        }
        let r = CornerRadius::same(8);
        s.visuals.widgets.noninteractive.corner_radius = r;
        s.visuals.widgets.inactive.corner_radius = r;
        s.visuals.widgets.hovered.corner_radius = r;
        s.visuals.widgets.active.corner_radius = r;
        s.visuals.widgets.open.corner_radius = r;
        s.visuals.window_corner_radius = CornerRadius::same(14);
    });
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

impl GlassApp {
    fn scan(&mut self) {
        let mine = fingerprint_hex(&self.device.fingerprint());
        let peers = self.device.peers();
        let me = PROTOCOL_VERSION.to_string();
        self.machines = browse::list()
            .into_iter()
            .filter_map(|f| {
                let fp = f.fp.clone().unwrap_or_default().to_lowercase();
                if !fp.is_empty() && mine.starts_with(&fp) {
                    return None; // this PC itself
                }
                Some(Machine {
                    paired: !fp.is_empty()
                        && peers.iter().any(|p| {
                            p.roles & ROLE_HOST != 0
                                && fingerprint_hex(&p.fingerprint).starts_with(&fp)
                        }),
                    version_ok: f.version.as_deref().is_none_or(|v| v == me),
                    kind: match f.os.as_deref() {
                        Some("mac") => "Mac",
                        Some("windows") => "PC",
                        _ => "Glass",
                    },
                    name: f.name,
                    address: f.address,
                    port: f.port,
                })
            })
            .collect();
        self.last_scan = Instant::now();
    }

    fn connect(&mut self, host: String, port: u16) {
        if self.connecting {
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
        self.connecting = true;
        self.status = format!("Connecting to {host} \u{2026}");
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
            self.status = "Type the other machine's address (it shows it when sharing).".into();
            return;
        }
        let (h, p) = match text.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().unwrap()),
            _ => (text, DEFAULT_PORT),
        };
        self.connect(h, p);
    }

    fn toggle_share(&mut self) {
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
                self.connecting = false;
                self.status = e.clone();
                self.dialog = Dialog::Message("Couldn't connect".into(), e);
                show(ctx);
            }
            Msg::Report(r) => {
                self.connecting = false;
                self.status = "Session ended. Results are saved next to Glass.exe.".into();
                self.dialog = Dialog::Report(r);
                show(ctx);
            }
            Msg::Status(s) => self.status = s,
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
            }
            Msg::HostStopped(why) => {
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
            HostEvent::PairingPin {
                device, pin, addr, ..
            } => {
                let (a, b) = pin.split_at(3);
                self.pin = Some((
                    format!("{a} {b}"),
                    format!(
                        "\u{201C}{device}\u{201D} ({}) wants to pair. Type this PIN there. Pair only your own devices.",
                        addr.ip()
                    ),
                ));
                show(ctx);
            }
            HostEvent::Paired { device } => {
                self.pin = None;
                self.share_status = format!(
                    "Paired with \u{201C}{device}\u{201D}. It can now view and control this PC."
                );
            }
            HostEvent::WrongPin { device, left } => {
                self.share_status = format!(
                    "Wrong PIN from \u{201C}{device}\u{201D}: {left} more tries with this PIN."
                )
            }
            HostEvent::PairingLocked => {
                self.pin = None;
                self.share_status =
                    "Too many wrong PINs: pairing is off until sharing restarts.".into();
            }
            HostEvent::Refused { fingerprint, addr } => {
                self.share_status = format!(
                    "Refused a device that isn't paired ({}, id {fingerprint}).",
                    addr.ip()
                )
            }
            HostEvent::Denied { device } => {
                self.share_status = format!("Did not allow \u{201C}{device}\u{201D}.")
            }
            HostEvent::Loop { device } => {
                self.share_status = format!(
                    "Refused \u{201C}{device}\u{201D}: this PC is viewing it, so it would loop."
                )
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
                    "That PIN wasn't right (or it expired). Connect again for a new PIN.".into()
                }
                e => format!("{host}: {e}"),
            }));
        }
    }
    let prep = match viewer::prepare(s.view, s.full_colour) {
        Ok(p) => p,
        Err(e) => return wake.send(Msg::Failed(format!("Can't start the viewer: {e}"))),
    };
    match Client::connect_with(&device, &host, port, prep.hello) {
        Ok(c) => {
            wake.send(Msg::Status(format!(
                "Viewing \u{201C}{}\u{201D}.",
                c.peer().name
            )));
            let opts = viewer::Options {
                windowed: false,
                seconds: None,
                rule: None,
                full_colour: s.full_colour,
            };
            match viewer::run(c, prep, opts, &crate::banner()) {
                Ok((report, _)) => {
                    save_report("results-viewer.txt", &report);
                    wake.send(Msg::Report(report));
                }
                Err(e) => wake.send(Msg::Failed(format!("The viewer stopped: {e}"))),
            }
        }
        Err(ConnectError::NotPaired) => match pair_begin(&device, &host, port) {
            Ok(p) => wake.send(Msg::AskPin(p, host, port)),
            Err(e) => wake.send(Msg::Failed(format!("{host}: {e}"))),
        },
        Err(e) => wake.send(Msg::Failed(format!("{host}: {e}"))),
    }
}

/// Theme-aware colours for tags.
struct Palette {
    accent: Color32,
    on_accent: Color32,
    strong: Color32,
    weak: Color32,
    ok: (Color32, Color32),
    warn: (Color32, Color32),
    kind: (Color32, Color32),
}

impl Palette {
    fn of(v: &egui::Visuals) -> Self {
        let dark = v.dark_mode;
        Palette {
            accent: if dark {
                Color32::from_rgb(62, 127, 200)
            } else {
                Color32::from_rgb(23, 92, 170)
            },
            on_accent: Color32::WHITE,
            strong: v.strong_text_color(),
            weak: v.weak_text_color(),
            ok: if dark {
                (
                    Color32::from_rgb(28, 70, 44),
                    Color32::from_rgb(150, 230, 170),
                )
            } else {
                (
                    Color32::from_rgb(214, 242, 222),
                    Color32::from_rgb(20, 100, 50),
                )
            },
            warn: if dark {
                (
                    Color32::from_rgb(80, 64, 20),
                    Color32::from_rgb(250, 215, 120),
                )
            } else {
                (
                    Color32::from_rgb(253, 240, 200),
                    Color32::from_rgb(130, 90, 0),
                )
            },
            kind: if dark {
                (Color32::from_gray(64), Color32::from_gray(225))
            } else {
                (Color32::from_gray(226), Color32::from_gray(50))
            },
        }
    }

    fn primary(&self, text: &str, w: f32) -> egui::Button<'static> {
        egui::Button::new(
            RichText::new(text.to_string())
                .strong()
                .color(self.on_accent),
        )
        .fill(self.accent)
        .min_size(egui::vec2(w, 34.0))
    }
}

/// A small rounded tag ("Mac", "✓ paired").
fn tag(ui: &mut egui::Ui, text: &str, (fill, fg): (Color32, Color32)) {
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(13.0).color(fg));
        });
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let v = ui.visuals().clone();
    let fill = if v.dark_mode {
        Color32::from_gray(38)
    } else {
        Color32::WHITE
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(v.widgets.noninteractive.bg_stroke)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
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
        }
        ctx.request_repaint_after(Duration::from_millis(1500));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let pal = Palette::of(ui.visuals());
        let bg = if ui.visuals().dark_mode {
            Color32::from_gray(24)
        } else {
            Color32::from_gray(243)
        };

        egui::Frame::new().fill(bg).inner_margin(Margin::same(20)).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            // Header: title, theme, settings.
            ui.horizontal(|ui| {
                ui.label(RichText::new("Glass").size(28.0).strong().color(pal.strong));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.dialog = Dialog::Settings;
                    }
                    let before = self.settings.theme;
                    egui::ComboBox::from_id_salt("theme")
                        .selected_text(match self.settings.theme {
                            ThemePreference::System => "Theme: System",
                            ThemePreference::Light => "Theme: Light",
                            ThemePreference::Dark => "Theme: Dark",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.settings.theme, ThemePreference::System, "System");
                            ui.selectable_value(&mut self.settings.theme, ThemePreference::Light, "Light");
                            ui.selectable_value(&mut self.settings.theme, ThemePreference::Dark, "Dark");
                        });
                    if self.settings.theme != before {
                        ctx.set_theme(self.settings.theme);
                        self.settings.save();
                    }
                });
            });
            ui.label(RichText::new(&self.status).color(pal.weak));
            ui.add_space(4.0);

            // Sharing.
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Share this PC").size(18.0).strong().color(pal.strong));
                        let s = if !self.share_status.is_empty() {
                            self.share_status.as_str()
                        } else {
                            "Let another Mac or PC view and control this PC."
                        };
                        ui.label(RichText::new(s).color(pal.weak));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let clicked = if self.sharing.is_some() {
                            ui.add(egui::Button::new(RichText::new("Stop sharing").strong()).min_size(egui::vec2(140.0, 34.0)))
                                .clicked()
                        } else {
                            ui.add(pal.primary("Start sharing", 140.0)).clicked()
                        };
                        if clicked {
                            self.toggle_share();
                        }
                    });
                });
                if let Some((pin, text)) = &self.pin {
                    ui.add_space(4.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(format!("PIN  {pin}")).size(40.0).strong().color(pal.strong));
                        ui.label(RichText::new(text).color(pal.weak));
                    });
                }
                if self.sharing.is_some() && !super::blocker::self_elevated() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new("Admin windows (Task Manager, installers) ignore remote input unless Glass runs as administrator.")
                                .size(13.0)
                                .color(pal.weak),
                        );
                        if ui.small_button("Restart as administrator").clicked() && restart_elevated(true) {
                            self.quit(&ctx);
                        }
                    });
                }
            });
            ui.add_space(6.0);

            // Machines.
            ui.horizontal(|ui| {
                ui.label(RichText::new("Machines on this network").size(18.0).strong().color(pal.strong));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Paired devices").clicked() {
                        self.dialog = Dialog::Paired;
                    }
                    if ui.button("Refresh").clicked() {
                        let _ = browse::start();
                        self.scan();
                    }
                });
            });
            let list_h = (ui.available_height() - 56.0).max(120.0);
            let mut go = None;
            egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, true]).show(ui, |ui| {
                if self.machines.is_empty() {
                    card(ui, |ui| {
                        ui.label(
                            RichText::new("Looking for Macs and PCs with Glass \u{2026}  Start sharing on the other machine, or type its address below.")
                                .color(pal.weak),
                        );
                    });
                }
                for m in &self.machines {
                    card(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&m.name).size(17.0).strong().color(pal.strong));
                                    tag(ui, m.kind, pal.kind);
                                    if !m.version_ok {
                                        tag(ui, "different Glass version", pal.warn);
                                    } else if m.paired {
                                        tag(ui, "\u{2713} paired", pal.ok);
                                    } else {
                                        tag(ui, "needs PIN", pal.warn);
                                    }
                                });
                                ui.label(RichText::new(&m.address).color(pal.weak));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add_enabled(!self.connecting && m.version_ok, pal.primary("Connect", 110.0)).clicked() {
                                    go = Some((m.address.clone(), m.port));
                                }
                            });
                        });
                    });
                }
            });
            if let Some((h, p)) = go {
                self.connect(h, p);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Or an address:");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.address)
                        .hint_text("e.g. 192.168.1.20")
                        .desired_width(220.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.add_enabled(!self.connecting, egui::Button::new("Connect")).clicked() || enter {
                    self.connect_typed();
                }
            });
        });

        self.dialogs(&ctx);

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
    fn dialogs(&mut self, ctx: &egui::Context) {
        let mut close = false;
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
                egui::Modal::new(egui::Id::unique("pin")).show(ctx, |ui| {
                    ui.set_width(380.0);
                    ui.heading("Pair with Glass");
                    ui.label(format!("\u{201C}{name}\u{201D} is showing a 6-digit PIN. Type it here."));
                    ui.label(
                        RichText::new("Pair only your own devices: a paired host sees what you type while you view it.")
                            .size(13.0)
                            .weak(),
                    );
                    let r = ui.add(
                        egui::TextEdit::singleline(pin)
                            .hint_text("123 456")
                            .font(egui::TextStyle::Heading)
                            .desired_width(200.0),
                    );
                    r.request_focus();
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        if ui.button("Pair").clicked() || enter {
                            submit = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            submit = Some(false);
                        }
                    });
                });
                if let Some(ok) = submit {
                    let digits: String = pin.chars().filter(char::is_ascii_digit).collect();
                    match (ok && !digits.is_empty(), pairing.take()) {
                        (true, Some(p)) => {
                            self.status = format!("Pairing with \u{201C}{name}\u{201D} \u{2026}");
                            let (wake, device, s, h, pt) = (
                                self.wake.clone(),
                                self.device.clone(),
                                self.settings.clone(),
                                host.clone(),
                                *port,
                            );
                            std::thread::spawn(move || {
                                connect_worker(wake, device, h, pt, Some((p, digits)), s)
                            });
                        }
                        _ => {
                            self.connecting = false;
                            self.status = "Pairing cancelled.".into();
                        }
                    }
                    close = true;
                }
            }
            Dialog::Report(text) => {
                egui::Modal::new(egui::Id::unique("report")).show(ctx, |ui| {
                    ui.set_width((ctx.content_rect().width() - 80.0).clamp(320.0, 760.0));
                    ui.heading("Session results");
                    ui.label(RichText::new("Copy these and paste them back to Claude.").weak());
                    egui::ScrollArea::vertical()
                        .max_height(380.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(text.as_str()).monospace().size(12.0),
                                )
                                .selectable(true),
                            );
                        });
                    ui.horizontal(|ui| {
                        if ui.button("Copy results").clicked() {
                            ctx.copy_text(text.clone());
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Message(title, text) => {
                egui::Modal::new(egui::Id::unique("msg")).show(ctx, |ui| {
                    ui.set_width(400.0);
                    ui.heading(title.as_str());
                    ui.label(text.as_str());
                    if ui.button("OK").clicked() {
                        close = true;
                    }
                });
            }
            Dialog::Paired => {
                let peers: Vec<Peer> = self.device.peers();
                let mut forget = None;
                egui::Modal::new(egui::Id::unique("paired")).show(ctx, |ui| {
                    ui.set_width(480.0);
                    ui.heading("Paired devices");
                    if peers.is_empty() {
                        ui.label(RichText::new("None yet.").weak());
                    }
                    for p in &peers {
                        card(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(&p.name).strong());
                                    let mut roles = Vec::new();
                                    if p.roles & ROLE_VIEWER != 0 {
                                        roles.push("may view this PC");
                                    }
                                    if p.roles & ROLE_HOST != 0 {
                                        roles.push("this PC may view it");
                                    }
                                    ui.label(
                                        RichText::new(format!(
                                            "id {}  \u{00B7}  {}",
                                            fingerprint_short(&p.fingerprint),
                                            roles.join(", ")
                                        ))
                                        .size(13.0)
                                        .weak(),
                                    );
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("Forget").clicked() {
                                            forget = Some(p.fingerprint);
                                        }
                                    },
                                );
                            });
                        });
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
                if let Some(fp) = forget {
                    let _ = self.device.forget(&fp);
                    self.scan();
                }
            }
            Dialog::Settings => {
                let mut s = self.settings.clone();
                egui::Modal::new(egui::Id::unique("settings")).show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.heading("Settings");
                    ui.label(RichText::new("Theme").strong());
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut s.theme, ThemePreference::System, "System");
                        ui.radio_value(&mut s.theme, ThemePreference::Light, "Light");
                        ui.radio_value(&mut s.theme, ThemePreference::Dark, "Dark");
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new("When viewing a Mac, start in").strong());
                    egui::ComboBox::from_id_salt("view")
                        .width(300.0)
                        .selected_text(viewer::mode_name(s.view))
                        .show_ui(ui, |ui| {
                            for m in [MODE_FIT, MODE_FIT_MORE_SPACE, MODE_FIT_LARGER, MODE_HOST_SCREEN] {
                                ui.selectable_value(&mut s.view, m, viewer::mode_name(m));
                            }
                        });
                    ui.add_space(4.0);
                    ui.label(RichText::new("Colour").strong());
                    ui.radio_value(&mut s.full_colour, false, "Fast (4:2:0): lowest lag");
                    ui.radio_value(&mut s.full_colour, true, "Full colour (4:4:4): exact colours, a little more lag");
                    ui.label(
                        RichText::new("Both can be changed while viewing, from the bar at the top edge of the screen.")
                            .size(13.0)
                            .weak(),
                    );
                    if ui.button("Done").clicked() {
                        close = true;
                    }
                });
                if s != self.settings {
                    if s.theme != self.settings.theme {
                        ctx.set_theme(s.theme);
                    }
                    self.settings = s;
                    self.settings.save();
                }
            }
        }
        if close {
            self.dialog = Dialog::None;
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

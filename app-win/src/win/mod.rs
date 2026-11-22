//! Windows-only pieces. Host: D3D11 device, Windows.Graphics.Capture, NVENC,
//! input injection. Viewer (`viewer.rs`): D3D11VA decode, flip-model present.

pub mod app;
mod bar;
mod blocker;
pub mod browse;
mod capture;
mod cuda;
mod cursor;
mod d3d;
mod dda;
mod decode;
mod decode444;
pub mod desktop;
mod display;
pub mod dpapi;
mod encode;
mod inject;
mod mdns;
mod present;
pub mod service;
mod stats_box;
pub mod viewer;

use glass_core::clock;
use glass_core::identity::fingerprint_short;
use glass_core::input::InputAction;
use glass_core::net::{HostEvent, HostListener, HostOptions, HostSession};
use glass_core::packet::OutFrame;
use glass_core::protocol::{CursorShape, StreamInfo, DEFAULT_PORT, FLAG_IDR};
use glass_core::recording::{
    FrameMeta, RecordingHeader, RecordingWriter, CHROMA_420, CHROMA_444, CODEC_HEVC,
};
use glass_core::stats::{IntervalTracker, StatRing, Summary};
use std::error::Error;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

pub type Res<T> = Result<T, Box<dyn Error>>;

pub fn list() -> Res<()> {
    d3d::init_process();
    println!("Monitors:");
    for (i, m) in capture::monitors()?.iter().enumerate() {
        println!(
            "  [{i}] {} {}x{} @ {} Hz{}",
            m.name,
            m.width,
            m.height,
            m.refresh_hz,
            if m.primary { " (primary)" } else { "" }
        );
    }
    println!("GPUs:");
    for a in d3d::adapters()? {
        println!("  {} (vendor {:04x})", a.name, a.vendor_id);
    }
    Ok(())
}

struct Args {
    monitor: usize,
    seconds: f64,
    chroma444: bool,
    mbps: u32,
    port: u16,
    log: Option<String>,
    /// Listen on 127.0.0.1 only (local tests; no firewall prompt).
    local: bool,
    /// Ask on the console before each (paired) viewer connects.
    ask: bool,
    /// A fixed pairing PIN (tests and local checks only).
    pin: Option<String>,
    /// The service's agent: Desktop Duplication following the lock screen
    /// and UAC prompts, input on whichever desktop is showing (M3f).
    desktops: bool,
    /// Switch this screen to the mode that best fits the viewer while it's
    /// connected (`--keep-resolution` turns it off).
    match_res: bool,
    out: String,
}

fn parse(args: &[String]) -> Res<Args> {
    let mut a = Args {
        monitor: 0,
        seconds: 10.0,
        chroma444: false,
        mbps: 80,
        port: DEFAULT_PORT,
        log: None,
        local: false,
        ask: false,
        pin: None,
        desktops: false,
        match_res: true,
        out: String::new(),
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--monitor" => a.monitor = val()?.parse()?,
            "--seconds" => a.seconds = val()?.parse()?,
            "--mbps" => a.mbps = val()?.parse()?,
            "--port" => a.port = val()?.parse()?,
            "--log" => a.log = Some(val()?.to_string()),
            "--local" => a.local = true,
            "--ask" => a.ask = true,
            "--pin" => a.pin = Some(val()?.to_string()),
            "--desktops" => a.desktops = true,
            "--keep-resolution" => a.match_res = false,
            "--chroma" => {
                a.chroma444 = match val()?.as_str() {
                    "444" => true,
                    "420" => false,
                    v => return Err(format!("--chroma must be 420 or 444, got {v}").into()),
                }
            }
            s if !s.starts_with("--") && a.out.is_empty() => a.out = s.to_string(),
            s => return Err(format!("unknown argument {s}").into()),
        }
    }
    Ok(a)
}

fn ms(us: u32) -> f64 {
    us as f64 / 1000.0
}

pub fn fmt_line(name: &str, s: &Summary) -> String {
    format!(
        "  {name:<18} p50 {:>7.2} ms  p99 {:>7.2} ms  max {:>7.2} ms  sd {:>6.2} ms  (n={})",
        ms(s.p50_us),
        ms(s.p99_us),
        ms(s.max_us),
        s.stddev_us / 1000.0,
        s.count
    )
}

fn line(name: &str, s: &Summary) {
    println!("{}", fmt_line(name, s));
}

fn size_line(s: &Summary) -> String {
    format!(
        "  frame size         p50 {:>6.1} KB  p99 {:>6.1} KB  max {:>6.1} KB",
        s.p50_us as f64 / 1e3,
        s.p99_us as f64 / 1e3,
        s.max_us as f64 / 1e3
    )
}

pub fn record(args: &[String]) -> Res<()> {
    let a = parse(args)?;
    if a.out.is_empty() {
        return Err("missing output path".into());
    }
    d3d::init_process();
    let monitors = capture::monitors()?;
    let mon = monitors
        .get(a.monitor)
        .ok_or_else(|| format!("no monitor {} (see `glass list`)", a.monitor))?;
    let gpu = d3d::Gpu::new_nvidia()?;
    println!(
        "Recording monitor [{}] {} {}x{} @ {} Hz on {} | HEVC {} {} Mbps | {} s",
        a.monitor,
        mon.name,
        mon.width,
        mon.height,
        mon.refresh_hz,
        gpu.name,
        if a.chroma444 { "4:4:4" } else { "4:2:0" },
        a.mbps,
        a.seconds
    );

    let mut cap = capture::Capture::start(&gpu, mon, false)?;
    let (w, h) = cap.size();
    let mut enc = encode::Encoder::new(
        &gpu.device,
        &encode::Config {
            width: w,
            height: h,
            fps: mon.refresh_hz,
            bitrate: a.mbps as i64 * 1_000_000,
            chroma444: a.chroma444,
            keep_last: false,
        },
    )?;

    // Preallocate the whole recording so the loop does no I/O or reallocation.
    let est = (a.seconds * a.mbps as f64 * 1e6 / 8.0 * 2.0) as usize + (64 << 20);
    let header = RecordingHeader {
        codec: CODEC_HEVC,
        chroma: if a.chroma444 { CHROMA_444 } else { CHROMA_420 },
        width: w,
        height: h,
        fps: mon.refresh_hz,
    };
    let mut writer = RecordingWriter::new(Vec::with_capacity(est), &header)?;

    let mut encode_ring = StatRing::new(); // frame arrival -> packet out
    let mut lead_ring = StatRing::new(); // frame arrival -> compositor (present-target) timestamp
    let mut size_ring = StatRing::new(); // bytes per frame
    let mut intervals = IntervalTracker::default(); // between captured frames
    let mut frame_id = 0u32;
    let mut skipped = 0u32;
    let mut idr_bytes = 0usize;
    let mut total_bytes = 0u64;

    let t0 = clock::now_us();
    let end = t0 + (a.seconds * 1e6) as u64;
    while clock::now_us() < end {
        cap.wait(50);
        while let Some(frame) = cap.next_frame()? {
            let arrival = clock::now_us();
            if frame.size != (w, h) {
                skipped += 1; // LATER(M7): handle resolution changes.
                continue;
            }
            // SystemRelativeTime turns out to be the *target present* time and
            // is usually ahead of arrival, so the arrival time is the capture
            // timestamp; the compositor time is kept only for pacing stats.
            lead_ring.push((frame.ts_us as i64 - arrival as i64).max(0) as u32);
            intervals.tick(frame.ts_us);
            let force_idr = frame_id == 0;
            let mut wrote: Res<()> = Ok(());
            enc.encode(
                &frame.texture,
                arrival as i64,
                force_idr,
                |data, key, pts| {
                    let out = clock::now_us();
                    let encode_us = out.saturating_sub(arrival) as u32;
                    encode_ring.push(encode_us);
                    size_ring.push(data.len() as u32);
                    total_bytes += data.len() as u64;
                    if key && idr_bytes == 0 {
                        idr_bytes = data.len();
                    }
                    let meta = FrameMeta {
                        frame_id,
                        flags: if key { FLAG_IDR } else { 0 },
                        capture_ts_us: pts as u64,
                        encode_us,
                        len: data.len() as u32,
                    };
                    if let Err(e) = writer.write_frame(&meta, data) {
                        wrote = Err(e.into());
                    }
                },
            )?;
            wrote?;
            frame_id += 1;
        }
    }
    enc.flush(|data, key, pts| {
        let meta = FrameMeta {
            frame_id,
            flags: if key { FLAG_IDR } else { 0 },
            capture_ts_us: pts as u64,
            encode_us: 0,
            len: data.len() as u32,
        };
        let _ = writer.write_frame(&meta, data);
    })?;
    let elapsed = (clock::now_us() - t0) as f64 / 1e6;
    let buf = writer.into_inner();
    std::fs::write(&a.out, &buf)?;

    println!(
        "Wrote {} ({:.1} MB): {} frames in {:.1} s ({:.1} fps captured), {} skipped",
        a.out,
        buf.len() as f64 / 1e6,
        frame_id,
        elapsed,
        frame_id as f64 / elapsed,
        skipped
    );
    println!(
        "  bitrate {:.1} Mbps avg | first IDR {:.0} KB",
        total_bytes as f64 * 8.0 / elapsed / 1e6,
        idr_bytes as f64 / 1e3
    );
    line("encode", &encode_ring.summary());
    line("WGC ts lead", &lead_ring.summary());
    line("capture interval", &intervals.intervals.summary());
    println!("{}", size_line(&size_ring.summary()));
    Ok(())
}

/// This PC's LAN address as the OS would route it (no packet is sent).
fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // TEST-NET-1: only used to pick a route
    s.local_addr().ok().map(|a| a.ip())
}

/// Per-session host counters and timing rings.
struct HostRun {
    frame_id: u32,
    encode: StatRing, // capture arrival -> encoded packet out
    send: StatRing,   // packetise + send_to for one frame
    size: StatRing,   // bytes per frame
    intervals: IntervalTracker,
    idrs: u32,
    idle_idrs: u32,
    /// What blocked input during the session, and for how long (µs).
    blocked_by: Vec<String>,
    blocked_us: u64,
    not_sent: u32,
    skipped: u32,
    error: Option<String>,
}

impl HostRun {
    fn new() -> Self {
        Self {
            frame_id: 0,
            encode: StatRing::new(),
            send: StatRing::new(),
            size: StatRing::new(),
            intervals: IntervalTracker::default(),
            idrs: 0,
            idle_idrs: 0,
            blocked_by: Vec::new(),
            blocked_us: 0,
            not_sent: 0,
            skipped: 0,
            error: None,
        }
    }

    /// Encoder output -> network. Called from inside the encode callback.
    /// `input_seq`: input applied when the frame was captured.
    fn on_packet(
        &mut self,
        session: &mut HostSession,
        capture_us: u64,
        input_seq: u32,
        data: &[u8],
        key: bool,
    ) {
        let out = clock::now_us();
        let encode_us = out.saturating_sub(capture_us) as u32;
        let f = OutFrame {
            frame_id: self.frame_id,
            flags: if key { FLAG_IDR } else { 0 },
            capture_ts_us: capture_us,
            encode_us,
            input_seq,
        };
        self.frame_id = self.frame_id.wrapping_add(1);
        match session.send_frame(&f, data) {
            Ok(true) => {
                self.encode.push(encode_us);
                self.send.push(clock::now_us().saturating_sub(out) as u32);
                self.size.push(data.len() as u32);
                self.idrs += key as u32;
            }
            Ok(false) => self.not_sent += 1,
            Err(e) => self.error = Some(format!("send failed: {e}")),
        }
    }
}

/// `glass host`: waits for a viewer, streams the monitor live until it leaves,
/// prints the session's numbers, and waits for the next viewer.
/// Clicking inside a console with QuickEdit on starts a selection, and Windows
/// then pauses the process at its next console write (freezing the host).
fn disable_quick_edit() {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE, ENABLE_EXTENDED_FLAGS,
        ENABLE_QUICK_EDIT_MODE, STD_INPUT_HANDLE,
    };
    unsafe {
        let Ok(h) = GetStdHandle(STD_INPUT_HANDLE) else {
            return;
        };
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(h, &mut mode).is_ok() {
            let _ = SetConsoleMode(h, (mode & !ENABLE_QUICK_EDIT_MODE) | ENABLE_EXTENDED_FLAGS);
        }
    }
}

pub fn host(args: &[String]) -> Res<()> {
    host_with(
        args,
        HostUi {
            event: Box::new(crate::print_event),
            line: Box::new(|l| println!("{l}")),
            report: Box::new(|_| {}),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            end: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    )
}

/// `glass-cli agent`: what the Glass service runs as SYSTEM in the PC's
/// console session (M3f). Shares until the service stops it, telling the
/// Glass window how it's going through `C:\ProgramData\Glass\status.txt`.
pub fn agent(args: &[String]) -> Res<()> {
    service::agent_env();
    let end = Arc::new(std::sync::atomic::AtomicBool::new(false));
    service::watch_end(end.clone());
    let status = Arc::new(std::sync::Mutex::new(service::Status::default()));
    let (s1, s2, s3) = (status.clone(), status.clone(), status.clone());
    let update = |s: &Arc<std::sync::Mutex<service::Status>>,
                  f: &mut dyn FnMut(&mut service::Status)| {
        let mut g = s.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g);
        g.write();
    };
    let ui = HostUi {
        event: Box::new(move |e| {
            update(&s1, &mut |st| match &e {
                HostEvent::PairingPin { device, pin, .. } => {
                    st.pin = Some((pin.clone(), device.clone()))
                }
                HostEvent::Paired { .. } | HostEvent::PairingLocked => st.pin = None,
                _ => {}
            });
            crate::print_event(e);
        }),
        line: Box::new(move |l| {
            let l = l.trim();
            update(&s2, &mut |st| {
                if l.starts_with("Viewer connected") {
                    st.connected = true;
                    st.pin = None;
                    st.line = l.to_string();
                } else if l.starts_with("Waiting for a viewer") {
                    st.connected = false;
                    st.line = "Waiting for a viewer".to_string();
                }
            });
            println!("{l}");
        }),
        report: Box::new(move |r| {
            update(&s3, &mut |st| st.connected = false);
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(service::data_dir().join("agent-results.txt"))
            {
                let _ = writeln!(f, "{r}");
            }
        }),
        stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        end: end.clone(),
    };
    let mut a = args.to_vec();
    a.push("--desktops".into());
    // A heartbeat, so the window knows the agent is alive while it waits.
    let beat = status.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(20));
        beat.lock().unwrap_or_else(|e| e.into_inner()).write();
    });
    host_with(&a, ui)
}

/// Where a host's messages go: the console (`glass host`) or the app's window.
pub struct HostUi {
    /// Pairing PINs, refusals and the like.
    pub event: Box<dyn FnMut(HostEvent) + Send>,
    /// Status lines.
    pub line: Box<dyn FnMut(&str) + Send>,
    /// A finished session's report.
    pub report: Box<dyn FnMut(&str) + Send>,
    /// Set (then wake the listener with a connection to its port) to stop.
    pub stop: Arc<std::sync::atomic::AtomicBool>,
    /// Set to end the current session only (sharing goes on).
    pub end: Arc<std::sync::atomic::AtomicBool>,
}

/// Shares this PC until `ui.stop` is set (the window's "Share this PC").
pub fn host_with(args: &[String], ui: HostUi) -> Res<()> {
    let HostUi {
        event,
        line,
        report: mut on_report,
        stop,
        end,
    } = ui;
    let line = std::sync::Mutex::new(line);
    macro_rules! say {
        ($($t:tt)*) => { (line.lock().unwrap_or_else(|e| e.into_inner()))(&format!($($t)*)) };
    }
    let a = parse(args)?;
    d3d::init_process();
    disable_quick_edit();
    let monitors = capture::monitors()?;
    let mon = monitors
        .get(a.monitor)
        .ok_or_else(|| format!("no monitor {} (see `glass list`)", a.monitor))?;
    let gpu = d3d::Gpu::new_nvidia()?;
    let device = crate::device()?;
    let ask: Option<glass_core::net::AskFn> = if a.ask {
        Some(Box::new(|p| {
            use std::io::BufRead;
            println!("\nAllow \"{}\" to view and control this PC? [y/N]", p.name);
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            matches!(line.trim(), "y" | "Y" | "yes")
        }))
    } else {
        None
    };
    let opts = HostOptions {
        fixed_pin: a.pin.clone(),
        ask,
        events: event,
        host_os: glass_core::protocol::HOST_OS_WINDOWS,
        // 4:4:4 switchable mid-session; view modes need a virtual display (M7).
        host_caps: glass_core::protocol::HOST_CAP_444,
    };
    let mut listener = if a.local {
        HostListener::bind_on(std::net::Ipv4Addr::LOCALHOST, a.port, device.clone(), opts)?
    } else {
        HostListener::bind(a.port, device.clone(), opts)?
    };
    let mut head = String::new();
    let _ = writeln!(
        head,
        "{} | host: monitor [{}] {} {}x{} @ {} Hz on {} | HEVC {} {} Mbps | port {}",
        crate::banner(),
        a.monitor,
        mon.name,
        mon.width,
        mon.height,
        mon.refresh_hz,
        gpu.name,
        if a.chroma444 { "4:4:4" } else { "4:2:0" },
        a.mbps,
        listener.port()
    );
    say!("{}", head.trim_end());
    let lan = lan_ip();
    let ip = lan.map_or("(unknown)".to_string(), |ip| ip.to_string());
    let id = fingerprint_short(&device.fingerprint());
    say!(
        "This PC: \"{}\", id {id}. Only paired devices can connect; everything is encrypted.",
        device.name
    );
    say!("On the Mac, open Glass and pick this PC (or type {ip}).");
    let paired = device
        .peers()
        .iter()
        .filter(|p| p.roles & glass_core::identity::ROLE_VIEWER != 0)
        .count();
    if paired == 0 {
        say!("No device is paired yet: connecting from the Mac shows a PIN here to type there.");
    } else {
        say!("{paired} paired device(s) (`glass devices` lists them, `glass forget NAME` removes one).");
    }
    if a.ask {
        say!("You'll be asked before each connection (--ask).");
    }
    say!("(Allow Glass through Windows Firewall on private networks if asked.)");
    if blocker::self_elevated() {
        say!("Running as administrator: the Mac can control admin windows too (not UAC prompts or the lock screen).");
    } else {
        say!("Not running as administrator: Windows blocks the Mac's input while an admin window (e.g. Task Manager) is in front.");
        say!("(Start host.cmd and answer Yes when Windows asks, or right-click it > Run as administrator.)");
    }
    if !a.local {
        let v4 = match lan {
            Some(std::net::IpAddr::V4(v4)) => Some(v4),
            _ => None,
        };
        if let Err(e) = mdns::advertise(&device.name, listener.port(), v4, &id) {
            say!("(Not listed on the network: {e}. Type the address on the Mac instead.)");
        }
    }

    let port = listener.port();
    while !stop.load(Ordering::Relaxed) {
        let mut cap = capture::Capture::start(&gpu, mon, a.desktops)?;
        let (mut w, mut h) = cap.size();
        let info = StreamInfo {
            codec: CODEC_HEVC,
            chroma: if a.chroma444 { CHROMA_444 } else { CHROMA_420 },
            width: w as u16,
            height: h as u16,
            fps: mon.refresh_hz as u16,
            mode: glass_core::protocol::MODE_HOST_SCREEN, // LATER(M7): IddCx virtual display
        };
        say!("\nWaiting for a viewer on port {} ...", listener.port());
        let mut injector = inject::Injector {
            mapper: inject::Mapper::for_monitor(inject::Rect {
                left: mon.left,
                top: mon.top,
                width: mon.width as i32,
                height: mon.height as i32,
            }),
            failed: 0,
            pointer: (0, 0),
        };
        // Windows drops input into admin windows and the secure desktop while
        // still reporting success, so count what arrived during a known block.
        let blocked = Arc::new(AtomicU64::new(0));
        let blocking = Arc::new(AtomicU8::new(0));
        let (b, bl) = (blocked.clone(), blocking.clone());
        let follow = a.desktops;
        let mut follower = desktop::Follower::new();
        // A new monitor rectangle after the resolution changed for a viewer.
        let remap: Arc<std::sync::Mutex<Option<inject::Rect>>> =
            Arc::new(std::sync::Mutex::new(None));
        let remap_in = remap.clone();
        let sink = Box::new(move |action| {
            if let Ok(mut g) = remap_in.try_lock() {
                if let Some(r) = g.take() {
                    injector.mapper = inject::Mapper::for_monitor(r);
                }
            }
            // The agent: put input on whichever desktop is showing (the lock
            // screen, a UAC prompt) once the capture thread has moved there.
            if follow {
                follower.check();
            }
            // Without admin rights Windows drops every injected event while an
            // admin window has focus, so a click can't move focus away. Try
            // bringing the clicked window forward instead (best effort).
            if bl.load(Ordering::Relaxed) == glass_core::protocol::NOTICE_INPUT_BLOCKED_ELEVATED {
                if let InputAction::Button { down: true, .. } = action {
                    blocker::try_focus_at(injector.pointer);
                }
            }
            let ok = injector.inject(&action);
            if !ok || bl.load(Ordering::Relaxed) != 0 {
                b.fetch_add(1, Ordering::Relaxed);
            }
        });
        let accepted = listener.accept(info, sink);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut session = match accepted {
            Ok(Some(s)) => s,
            Ok(None) => continue, // pairing or refusal, already reported
            Err(e) => {
                say!("A connection attempt failed: {e}");
                continue;
            }
        };
        if mdns::registered() == Some(false) {
            say!("(Listing on the network failed; type the address on the Mac.)");
        }
        say!(
            "Viewer connected: \"{}\" from {} (encrypted)",
            session.peer_name,
            session.peer_tcp.ip()
        );
        // Match the viewer's resolution for the session, where this screen
        // offers a better mode (M3f).
        let mut fps = mon.refresh_hz;
        let mut switched = None;
        if a.match_res {
            let v = session.viewer;
            if let Some(cur) = display::current(&mon.name) {
                let best = display::best(
                    &display::modes(&mon.name),
                    v.display_w as u32,
                    v.display_h as u32,
                    cur,
                );
                if let Some(m) = best {
                    if display::switch(&mon.name, m) {
                        switched = Some(cur);
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        let now = capture::monitors()?
                            .into_iter()
                            .find(|x| x.name == mon.name);
                        if let Some(nm) = now {
                            drop(cap);
                            cap = capture::Capture::start(&gpu, &nm, a.desktops)?;
                            (w, h) = cap.size();
                            fps = nm.refresh_hz;
                            *remap.lock().unwrap_or_else(|e| e.into_inner()) = Some(inject::Rect {
                                left: nm.left,
                                top: nm.top,
                                width: nm.width as i32,
                                height: nm.height as i32,
                            });
                            session.send_stream_change(StreamInfo {
                                width: w as u16,
                                height: h as u16,
                                fps: fps as u16,
                                ..info
                            })?;
                            say!(
                                "Matched the viewer: this screen is {}x{} for now (was {}x{}).",
                                w,
                                h,
                                cur.width,
                                cur.height
                            );
                        }
                    }
                }
            }
        }
        let (mut report, blocked_by, blocked_us) = run_session(
            &gpu,
            &mut cap,
            &mut session,
            &a,
            (w, h),
            fps,
            &blocking,
            &stop,
            &end,
        )?;
        end.store(false, Ordering::Relaxed);
        let counters = session.counters();
        let ic = counters.input;
        drop(session); // releases anything the viewer still held
        if let Some(orig) = switched {
            display::restore(&mon.name);
            let _ = writeln!(
                report,
                "  resolution: matched the viewer at {}x{} (this screen is {}x{}; put back)",
                w, h, orig.width, orig.height
            );
            say!("Screen back to {}x{}.", orig.width, orig.height);
        }
        let _ = writeln!(
            report,
            "  input: {} packets | {} keys, {} clicks, {} scrolls, {} moves | repaired {}",
            ic.packets, ic.keys, ic.clicks, ic.scrolls, ic.moves, ic.repaired
        );
        let _ = writeln!(
            report,
            "  rejected UDP datagrams (failed decryption or replays): {}",
            counters.rejected_datagrams
        );
        let _ = writeln!(
            report,
            "  input blocked by Windows: {} actions over {:.1} s{}",
            blocked.load(Ordering::Relaxed),
            blocked_us as f64 / 1e6,
            if blocked_by.is_empty() {
                String::new()
            } else {
                format!(" ({})", blocked_by.join(", "))
            }
        );
        let report = format!("{head}{report}");
        say!("{report}");
        on_report(&report);
        if let Some(path) = &a.log {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(f, "{report}")?;
            say!("(appended to {path})");
        }
    }
    mdns::unadvertise();
    let _ = port;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    gpu: &d3d::Gpu,
    cap: &mut capture::Capture,
    session: &mut HostSession,
    a: &Args,
    (w, h): (u32, u32),
    fps: u32,
    blocking: &AtomicU8,
    stop: &std::sync::atomic::AtomicBool,
    end: &std::sync::atomic::AtomicBool,
) -> Res<(String, Vec<String>, u64)> {
    // Keep the newest frame captured while waiting: a still screen sends
    // nothing new, so it's the viewer's first picture (m3f-test13: nothing
    // showed until something changed on the host). The service's capture
    // starts again for a fresh full frame.
    cap.refresh();
    let mut first = None;
    while let Some(f) = cap.next_frame()? {
        first = Some(f);
    }
    let make = |chroma444: bool| {
        encode::Encoder::new(
            &gpu.device,
            &encode::Config {
                width: w,
                height: h,
                fps: fps.max(1),
                bitrate: a.mbps as i64 * 1_000_000,
                chroma444,
                keep_last: true,
            },
        )
    };
    let info = |chroma444: bool| StreamInfo {
        codec: CODEC_HEVC,
        chroma: if chroma444 { CHROMA_444 } else { CHROMA_420 },
        width: w as u16,
        height: h as u16,
        fps: fps as u16,
        mode: glass_core::protocol::MODE_HOST_SCREEN,
    };
    // Colour: what the viewer asked for if it can decode 4:4:4 (M3d);
    // otherwise 4:2:0. `--chroma` only sets what Welcome announced.
    let v = session.viewer;
    let can_444 = v.caps & glass_core::protocol::CAP_HEVC_444 != 0;
    let mut chroma444 = can_444 && v.caps & glass_core::protocol::CAP_PREFER_420 == 0;
    if chroma444 != a.chroma444 {
        session.send_stream_change(info(chroma444))?;
    }
    let mut enc = make(chroma444)?;
    let mut run = HostRun::new();
    let mut pointer = cursor::CursorWatch::new();
    let mut watch = blocker::BlockerWatch::new();
    let mut blocked_since = None;
    let t0 = clock::now_us();
    let mut pending_idr = true;
    if let Some(frame) = first.take().filter(|f| f.size == (w, h)) {
        // Encoded now and kept as the last frame; the keyframe the viewer
        // asks for once it's ready resends it (pending_idr stays set).
        let arrival = clock::now_us();
        let input_seq = session.input_seq();
        enc.encode(&frame.texture, arrival as i64, true, |data, key, _| {
            run.on_packet(session, arrival, input_seq, data, key)
        })?;
    }
    while session.connected()
        && run.error.is_none()
        && !stop.load(Ordering::Relaxed)
        && !end.load(Ordering::Relaxed)
    {
        cap.wait(5);
        pending_idr |= session.take_keyframe_request();
        // The viewer switched colour (4:2:0 <-> 4:4:4); view modes need a
        // virtual display, which a PC host doesn't have yet (M7).
        if let Some((_mode, allow)) = session.take_mode_request() {
            let want = allow && can_444;
            if want != chroma444 {
                match make(want) {
                    Ok(e) => {
                        enc = e;
                        chroma444 = want;
                        session.send_stream_change(info(chroma444))?;
                        pending_idr = true;
                        println!(
                            "Viewer switched to {}.",
                            if want { "full colour (4:4:4)" } else { "4:2:0" }
                        );
                    }
                    Err(e) => println!("Couldn't switch colour: {e}"),
                }
            }
        }
        let now = clock::now_us();
        // The agent runs as SYSTEM on whichever desktop is showing: nothing
        // blocks it (the lock screen and UAC prompts are what it's for).
        let polled = if a.desktops {
            None
        } else {
            watch.poll(now, 250_000)
        };
        if let Some(b) = polled {
            blocking.store(b.code(), Ordering::Relaxed);
            let _ = session.send_notice(b.code());
            if let Some(since) = blocked_since.take() {
                run.blocked_us += now - since;
            }
            match &b {
                blocker::Blocker::None => println!("Input works again."),
                blocker::Blocker::Elevated(name) => {
                    println!("Input blocked: {name} runs as administrator and is in front. Run host.cmd as administrator to control it.");
                    if !run.blocked_by.contains(name) {
                        run.blocked_by.push(name.clone());
                    }
                }
                blocker::Blocker::SecureDesktop => {
                    println!("Input blocked: a UAC prompt or the lock screen is showing (answer it on the PC).");
                    let s = "UAC prompt / lock screen".to_string();
                    if !run.blocked_by.contains(&s) {
                        run.blocked_by.push(s);
                    }
                }
            }
            if b != blocker::Blocker::None {
                blocked_since = Some(now);
            }
        }
        match pointer.poll() {
            cursor::Change::Shape(s) => {
                let _ = session.send_cursor(&CursorShape {
                    serial: run.frame_id,
                    width: s.width,
                    height: s.height,
                    hot_x: s.hot_x,
                    hot_y: s.hot_y,
                    rgba: &s.rgba,
                });
                let _ = session.send_cursor_visible(true);
            }
            cursor::Change::Visible(v) => {
                let _ = session.send_cursor_visible(v);
            }
            cursor::Change::None => {}
        }
        let mut got = false;
        while let Some(frame) = cap.next_frame()? {
            let arrival = clock::now_us();
            if frame.size != (w, h) {
                run.skipped += 1; // LATER(M7): handle resolution changes.
                continue;
            }
            got = true;
            run.intervals.tick(arrival);
            let idr = std::mem::take(&mut pending_idr);
            let input_seq = session.input_seq();
            enc.encode(&frame.texture, arrival as i64, idr, |data, key, _| {
                run.on_packet(session, arrival, input_seq, data, key)
            })?;
        }
        if !got && pending_idr && session.streaming() && enc.has_last() {
            // Idle screen: resend the last frame as an IDR so the viewer recovers.
            let now = clock::now_us();
            pending_idr = false;
            run.idle_idrs += 1;
            let input_seq = session.input_seq();
            enc.encode_last_idr(now as i64, |data, key, _| {
                run.on_packet(session, now, input_seq, data, key)
            })?;
        }
    }
    let secs = (clock::now_us() - t0) as f64 / 1e6;
    let c = session.counters();
    let mut r = String::new();
    let _ = writeln!(
        r,
        "Session with {} ended after {:.1} s ({})",
        session.peer_tcp,
        secs,
        run.error.as_deref().unwrap_or("viewer left")
    );
    let _ = writeln!(
        r,
        "  frames sent {} ({:.1} fps), IDRs {} (idle re-sends {}), viewer keyframe requests {}, not sent (viewer not ready) {}, skipped {}",
        c.frames_sent,
        c.frames_sent as f64 / secs,
        run.idrs,
        run.idle_idrs,
        c.keyframe_requests,
        run.not_sent,
        run.skipped
    );
    let _ = writeln!(
        r,
        "  bitrate {:.1} Mbps avg | {} packets",
        c.bytes_sent as f64 * 8.0 / secs / 1e6,
        c.packets_sent
    );
    let _ = writeln!(r, "{}", fmt_line("encode", &run.encode.summary()));
    let _ = writeln!(r, "{}", fmt_line("send (packetise)", &run.send.summary()));
    let _ = writeln!(
        r,
        "{}",
        fmt_line("capture interval", &run.intervals.intervals.summary())
    );
    let _ = writeln!(r, "{}", size_line(&run.size.summary()));
    if a.desktops {
        let _ = writeln!(
            r,
            "  desktop switches (lock screen, sign-in, UAC) followed: {}",
            cap.desktop_switches()
        );
    }
    if let Some(since) = blocked_since {
        run.blocked_us += clock::now_us() - since;
    }
    Ok((r, run.blocked_by, run.blocked_us))
}

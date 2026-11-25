//! Windows-only pieces. Host: D3D11 device, Windows.Graphics.Capture, NVENC,
//! input injection. Viewer (`viewer.rs`): D3D11VA decode, flip-model present.

pub mod app;
mod bar;
mod blocker;
mod broker;
pub mod browse;
mod capture;
mod clip;
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
mod helper;
mod inject;
mod mdns;
mod netif;
mod nvenc;
#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unused_imports,
    clippy::all
)]
mod nvenc_sys;
mod pipe;
mod play;
mod present;
mod scene;
pub mod service;
mod slog;
mod sound;
mod stats_box;
pub mod vdisplay;
mod vfiles;
pub mod viewer;
mod vscreen;
pub mod wlan;

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

pub(crate) struct Args {
    pub(crate) monitor: usize,
    pub(crate) seconds: f64,
    pub(crate) chroma444: bool,
    pub(crate) mbps: u32,
    pub(crate) port: u16,
    pub(crate) log: Option<String>,
    /// Listen on 127.0.0.1 only (local tests; no firewall prompt).
    pub(crate) local: bool,
    /// Ask on the console before each (paired) viewer connects.
    pub(crate) ask: bool,
    /// A fixed pairing PIN (tests and local checks only).
    pub(crate) pin: Option<String>,
    /// The service's agent: Desktop Duplication following the lock screen
    /// and UAC prompts, input on whichever desktop is showing (M3f).
    pub(crate) desktops: bool,
    /// Switch this screen to the mode that best fits the viewer while it's
    /// connected (`--keep-resolution` turns it off).
    pub(crate) match_res: bool,
    /// Share the clipboard with the viewer (M5c; `--no-clipboard` turns it off).
    pub(crate) clipboard: bool,
    /// The service's pipes to its helpers (ADR 0017): their name prefix.
    pub(crate) pipes: String,
    pub(crate) out: String,
}

pub(crate) fn parse(args: &[String]) -> Res<Args> {
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
        clipboard: true,
        pipes: "Glass".to_string(),
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
            "--no-clipboard" => a.clipboard = false,
            "--pipes" => a.pipes = val()?.to_string(),
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

/// 1 ms timer resolution while it lives (then back to the default).
struct FineTimer;

impl FineTimer {
    fn new() -> Self {
        // SAFETY: a plain winmm call, undone in Drop.
        unsafe { windows::Win32::Media::timeBeginPeriod(1) };
        Self
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        // SAFETY: matches the timeBeginPeriod in `new`.
        unsafe { windows::Win32::Media::timeEndPeriod(1) };
    }
}

/// Still-picture refinement: this long after the last change (m5-test25:
/// 200 ms sent 122 quality keyframes in 3.5 minutes of scrolling, ~430 KB
/// each across QUIETPC's eero mesh hop; input -> screen went from ~35 to
/// 49 ms p50; 400 ms in m5-test27 felt slow to sharpen; 300 ms in
/// m5-test29 sharpened in ~0.5 s; 200 ms once the quality frame was a
/// correction; 100 ms for SPEC's 150 ms: the scorecard's changes whose first
/// frame wasn't sharp took 219-243 ms at 200 ms)...
const REFINE_AFTER_US: u64 = 100_000;
/// ...after changes worth it (bytes of real changes since the last one)...
const REFINE_MIN_CHANGE: usize = 100_000;
/// ...one high-quality keyframe of the last picture (m5-test22: three
/// passes at the normal per-frame budget never sharpened photos)...
const REFINE_PASSES: u32 = 1;
/// ...and at most one this often (2 s in m5-test27: flicking to the next
/// photo within 2 s of the last waited the rest of the 2 s to sharpen).
const REFINE_EVERY_US: u64 = 500_000;

/// The encoder's bitrate for a viewer's limit: coarse steps (each change
/// costs a keyframe with FFmpeg's NVENC), the largest at or under the limit,
/// never above ours.
const BITRATE_STEPS: [i64; 8] = [80, 60, 40, 30, 20, 15, 10, 8];

fn bitrate_step(limit: i64, ours: i64) -> i64 {
    BITRATE_STEPS
        .iter()
        .map(|m| m * 1_000_000)
        .find(|&b| b <= limit && b <= ours)
        .unwrap_or(8_000_000)
        .min(ours)
}

/// The next bitrate step above `cur` (bits/s), never above ours.
fn step_up(cur: i64, ours: i64) -> i64 {
    BITRATE_STEPS
        .iter()
        .rev()
        .map(|m| m * 1_000_000)
        .find(|&b| b > cur)
        .unwrap_or(ours)
        .min(ours)
}

/// The results' word for `qos::QOS_*`.
pub(crate) fn qos_word(q: u32) -> &'static str {
    match q as u8 {
        glass_core::qos::QOS_MARKED => "yes",
        glass_core::qos::QOS_REFUSED => "no (refused by Windows)",
        _ => "not yet",
    }
}

/// This PC's LAN address: an Ethernet or Wi-Fi adapter's (not a VPN's or
/// a virtual switch's, `netif`), else the one the OS would route through
/// (no packet is sent).
fn lan_ip() -> Option<std::net::IpAddr> {
    if let Some(v4) = netif::lan_ipv4() {
        return Some(std::net::IpAddr::V4(v4));
    }
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // TEST-NET-1: only used to pick a route
    s.local_addr().ok().map(|a| a.ip())
}

/// Per-session host counters and timing rings.
struct HostRun {
    frame_id: u32,
    encode: StatRing, // capture arrival -> encoded packet out
    /// The same, by colour (the current one in `chroma444`), and for the
    /// still-picture quality frames alone: which of them keeps up with
    /// motion (m5-test30: card animations missing in 4:4:4 only).
    encode_444: StatRing,
    encode_420: StatRing,
    refine_encode: StatRing,
    chroma444: bool,
    send: StatRing, // packetise + send_to for one frame
    size: StatRing, // bytes per frame
    intervals: IntervalTracker,
    idrs: u32,
    idle_idrs: u32,
    /// Still-picture refinement passes sent, and their bytes.
    refines: u32,
    /// Each encoder started this session: which, in which colour.
    encoder: String,
    refine_bytes: u64,
    /// What blocked input during the session, and for how long (Âµs).
    blocked_by: Vec<String>,
    blocked_us: u64,
    not_sent: u32,
    skipped: u32,
    /// Frames the frame-rate cap didn't send (a newer one went instead),
    /// and the rate it sends at.
    capped: u32,
    send_hz: u32,
    /// Encode time, a running average (µs), and how often the encoder's
    /// load lowered the frame rate, and to what.
    enc_avg_us: f64,
    /// Encoding a still picture's quality frame (not counted in the average:
    /// it's slow on purpose, and the screen is still).
    in_quality: bool,
    load_lowered: u32,
    load_lowest_hz: u32,
    /// Worst moments (M5): the longest pauses in capture while the picture
    /// was moving (Âµs into the session, gap), and the biggest frames (Âµs,
    /// bytes, encode Âµs, keyframe). Longest / biggest first.
    /// Rate control (M5): the encoder's bitrate now, the lowest set, and
    /// how many changes (0 = none yet: our own setting).
    rate_now: i64,
    rate_lo: i64,
    rate_changes: u32,
    /// The frame-rate lever: the full rate, how often it changed, and how
    /// often it went down to the viewer's refresh; the video rate (bits/s,
    /// smoothed) and when the last frame went.
    full_hz: u32,
    fps_changes: u32,
    fps_lowered: u32,
    avg_bps: f64,
    last_out_us: u64,
    t0: u64,
    last_capture: u64,
    last_gap: u64,
    pauses: Vec<(u64, u64)>,
    big: Vec<(u64, usize, u32, bool)>,
    error: Option<String>,
    /// This PC's Wi-Fi link during the session (M4 step 5).
    wifi: wlan::WifiWatch,
}

impl HostRun {
    /// A pause in capture after frames were coming steadily (a game
    /// freezing, capture stalling), not a still screen.
    fn note_capture(&mut self, now: u64) {
        if self.last_capture > 0 {
            let gap = now.saturating_sub(self.last_capture);
            let moving = self.last_gap < 30_000;
            if moving && gap > 40_000 && now > self.t0 + 2_000_000 {
                let at = now.saturating_sub(self.t0);
                if self.pauses.len() < 5 || gap > self.pauses.last().map_or(0, |p| p.1) {
                    self.pauses.push((at, gap));
                    self.pauses.sort_by_key(|p| std::cmp::Reverse(p.1));
                    self.pauses.truncate(5);
                }
            }
            self.last_gap = gap;
        }
        self.last_capture = now;
    }

    /// "worst moments (M5): â€¦", for the results.
    fn moments_line(&self) -> Option<String> {
        let when = |us: u64| {
            let s = us as f64 / 1e6;
            format!("{}:{:04.1}", (s / 60.0) as u64, s % 60.0)
        };
        let mut parts = Vec::new();
        if !self.pauses.is_empty() {
            let p: Vec<String> = self
                .pauses
                .iter()
                .map(|(at, gap)| format!("{} {:.0} ms", when(*at), *gap as f64 / 1e3))
                .collect();
            parts.push(format!("capture paused while moving: {}", p.join(", ")));
        }
        if !self.big.is_empty() {
            let b: Vec<String> = self
                .big
                .iter()
                .map(|(at, bytes, enc, key)| {
                    format!(
                        "{} {:.0} KB in {:.1} ms{}",
                        when(*at),
                        *bytes as f64 / 1e3,
                        *enc as f64 / 1e3,
                        if *key { " (keyframe)" } else { "" }
                    )
                })
                .collect();
            parts.push(format!("biggest frames: {}", b.join(", ")));
        }
        (!parts.is_empty()).then(|| {
            format!(
                "  worst moments (M5, time into the session): {}",
                parts.join(" | ")
            )
        })
    }

    fn new() -> Self {
        Self {
            wifi: wlan::WifiWatch::start(),
            frame_id: 0,
            encode: StatRing::new(),
            encode_444: StatRing::new(),
            encode_420: StatRing::new(),
            refine_encode: StatRing::new(),
            chroma444: false,
            send: StatRing::new(),
            size: StatRing::new(),
            intervals: IntervalTracker::default(),
            idrs: 0,
            idle_idrs: 0,
            refines: 0,
            encoder: String::new(),
            refine_bytes: 0,
            blocked_by: Vec::new(),
            blocked_us: 0,
            not_sent: 0,
            skipped: 0,
            capped: 0,
            send_hz: 0,
            enc_avg_us: 0.0,
            in_quality: false,
            load_lowered: 0,
            load_lowest_hz: 0,
            rate_now: 0,
            rate_lo: 0,
            rate_changes: 0,
            full_hz: 0,
            fps_changes: 0,
            fps_lowered: 0,
            avg_bps: 0.0,
            last_out_us: 0,
            t0: clock::now_us(),
            last_capture: 0,
            last_gap: u64::MAX,
            pauses: Vec::with_capacity(8),
            big: Vec::with_capacity(8),
            error: None,
        }
    }

    /// Encoder output -> network. Called from inside the encode callback.
    /// `input_seq`: input applied when the frame was captured.
    fn on_packet<V: ViewerLink>(
        &mut self,
        session: &mut V,
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
                if self.last_out_us > 0 {
                    let dt = out.saturating_sub(self.last_out_us).clamp(1_000, 1_000_000) as f64;
                    let inst = data.len() as f64 * 8.0 * 1e6 / dt;
                    self.avg_bps += (inst - self.avg_bps) * 0.05;
                }
                self.last_out_us = out;
                let at = out.saturating_sub(self.t0);
                if self.big.len() < 5 || data.len() > self.big.last().map_or(0, |b| b.1) {
                    self.big.push((at, data.len(), encode_us, key));
                    self.big.sort_by_key(|b| std::cmp::Reverse(b.1));
                    self.big.truncate(5);
                }
                self.encode.push(encode_us);
                if !self.in_quality {
                    self.enc_avg_us += (encode_us as f64 - self.enc_avg_us) * 0.05;
                }
                if self.chroma444 {
                    self.encode_444.push(encode_us);
                } else {
                    self.encode_420.push(encode_us);
                }
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

/// `glass-cli agent`: the Glass service's screen helper (ADR 0017). Runs as
/// SYSTEM in the session on the PC's screen; the service keeps the viewer's
/// connection and starts a new helper when that session changes.
pub fn agent(args: &[String]) -> Res<()> {
    helper::screen(args)
}

/// `glass-cli clip-agent`: the service's clipboard helper, as the signed-in
/// user (ADR 0017).
pub fn clip_agent(args: &[String]) -> Res<()> {
    helper::clipboard(args)
}

/// `glass-cli broker [--local] [--pin N] [--pipes NAME]`: the service's
/// broker in a console, for tests: helpers started by hand (`agent`,
/// `clip-agent` with the same `--pipes`) connect to it as this user.
pub fn broker_cli(args: &[String]) -> Res<()> {
    let me = pipe::current_user_sid().ok_or("couldn't read this user's id")?;
    broker::run(
        args,
        HostUi {
            event: Box::new(crate::print_event),
            line: Box::new(|l| println!("{l}")),
            report: Box::new(|_| {}),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            end: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
        vec![me.clone()],
        vec![me],
        Box::new(|_| true),
    )
}

/// What a host session's capture, encoding and input talk to: the viewer's
/// connection itself (the in-app host), or the broker over a pipe (the
/// service's screen helper, ADR 0017).
pub(crate) trait ViewerLink {
    fn viewer(&self) -> glass_core::net::ViewerHello;
    fn connected(&self) -> bool;
    fn streaming(&self) -> bool;
    fn take_keyframe_request(&self) -> bool;
    fn take_mode_request(&self) -> Option<(u8, bool)>;
    /// The viewer's video limit (bits/s, 0 = none), once each time it
    /// sends one (M5 rate control).
    fn take_video_rate(&self) -> Option<u32>;
    fn send_stream_change(&mut self, info: StreamInfo) -> std::io::Result<()>;
    fn input_seq(&self) -> u32;
    fn send_frame(&mut self, f: &OutFrame, data: &[u8]) -> std::io::Result<bool>;
    fn send_cursor(&mut self, s: &CursorShape) -> std::io::Result<()>;
    fn send_cursor_visible(&mut self, visible: bool) -> std::io::Result<()>;
    fn send_notice(&mut self, code: u8) -> std::io::Result<()>;
    /// Who's viewing, for the results.
    fn peer(&self) -> String;
    /// The connection's own counters (the broker keeps them otherwise).
    fn counters(&self) -> Option<glass_core::net::HostCounters>;
    /// The session, when this process runs the clipboard too (the in-app
    /// host; the service's clipboard runs as the user, in its own helper).
    fn clip_session(&self) -> Option<&HostSession>;
    /// Always tell the viewer the stream's parameters first: a helper that
    /// starts mid-session doesn't know what the last one sent.
    fn announce_stream(&self) -> bool;
}

impl ViewerLink for HostSession {
    fn viewer(&self) -> glass_core::net::ViewerHello {
        self.viewer
    }
    fn connected(&self) -> bool {
        HostSession::connected(self)
    }
    fn streaming(&self) -> bool {
        HostSession::streaming(self)
    }
    fn take_keyframe_request(&self) -> bool {
        HostSession::take_keyframe_request(self)
    }
    fn take_mode_request(&self) -> Option<(u8, bool)> {
        HostSession::take_mode_request(self)
    }
    fn take_video_rate(&self) -> Option<u32> {
        HostSession::take_video_rate(self)
    }
    fn send_stream_change(&mut self, info: StreamInfo) -> std::io::Result<()> {
        HostSession::send_stream_change(self, info)
    }
    fn input_seq(&self) -> u32 {
        HostSession::input_seq(self)
    }
    fn send_frame(&mut self, f: &OutFrame, data: &[u8]) -> std::io::Result<bool> {
        HostSession::send_frame(self, f, data)
    }
    fn send_cursor(&mut self, s: &CursorShape) -> std::io::Result<()> {
        HostSession::send_cursor(self, s)
    }
    fn send_cursor_visible(&mut self, visible: bool) -> std::io::Result<()> {
        HostSession::send_cursor_visible(self, visible)
    }
    fn send_notice(&mut self, code: u8) -> std::io::Result<()> {
        HostSession::send_notice(self, code)
    }
    fn peer(&self) -> String {
        self.peer_tcp.to_string()
    }
    fn counters(&self) -> Option<glass_core::net::HostCounters> {
        Some(HostSession::counters(self))
    }
    fn clip_session(&self) -> Option<&HostSession> {
        Some(self)
    }
    fn announce_stream(&self) -> bool {
        false
    }
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

/// Injects a viewer's input on monitor `mon`. `blocked` counts what Windows
/// dropped while it was known to be blocking (an admin window in front, the
/// secure desktop); `remap` takes the monitor's new rectangle after a
/// resolution switch.
pub(crate) fn input_sink(
    mon: &capture::Monitor,
    follow: bool,
    blocked: Arc<AtomicU64>,
    blocking: Arc<AtomicU8>,
    remap: Arc<std::sync::Mutex<Option<inject::Rect>>>,
) -> glass_core::net::InputSink {
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
    let mut follower = desktop::Follower::new();
    Box::new(move |action| {
        if let Ok(mut g) = remap.try_lock() {
            if let Some(r) = g.take() {
                injector.mapper = inject::Mapper::for_monitor(r);
            }
        }
        // The service's helper: put input on whichever desktop is showing
        // (the lock screen, a UAC prompt) once the capture thread is there.
        if follow {
            follower.check();
        }
        // Without admin rights Windows drops every injected event while an
        // admin window has focus, so a click can't move focus away. Try
        // bringing the clicked window forward instead (best effort).
        if blocking.load(Ordering::Relaxed) == glass_core::protocol::NOTICE_INPUT_BLOCKED_ELEVATED {
            if let InputAction::Button { down: true, .. } = action {
                blocker::try_focus_at(injector.pointer);
            }
        }
        let ok = injector.inject(&action);
        if !ok || blocking.load(Ordering::Relaxed) != 0 {
            blocked.fetch_add(1, Ordering::Relaxed);
        }
    })
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
    let say = |s: &str| (line.lock().unwrap_or_else(|e| e.into_inner()))(s);
    macro_rules! say {
        ($($t:tt)*) => { say(&format!($($t)*)) };
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
        // View modes need the virtual display (each is one at another size).
        host_caps: glass_core::protocol::HOST_CAP_444
            | glass_core::protocol::HOST_CAP_SCORECARD
            | if vdisplay::installed() {
                glass_core::protocol::HOST_CAP_VIEW_MODES
            } else {
                0
            },
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

    while !stop.load(Ordering::Relaxed) {
        let mut cap = capture::Capture::start(&gpu, mon, a.desktops)?;
        let (w, h) = cap.size();
        let info = StreamInfo {
            codec: CODEC_HEVC,
            chroma: if a.chroma444 { CHROMA_444 } else { CHROMA_420 },
            width: w as u16,
            height: h as u16,
            fps: mon.refresh_hz as u16,
            mode: glass_core::protocol::MODE_HOST_SCREEN, // LATER(M7): IddCx virtual display
        };
        say!("\nWaiting for a viewer on port {} ...", listener.port());
        let blocked = Arc::new(AtomicU64::new(0));
        let blocking = Arc::new(AtomicU8::new(0));
        let remap = Arc::new(std::sync::Mutex::new(None));
        let sink = input_sink(
            mon,
            a.desktops,
            blocked.clone(),
            blocking.clone(),
            remap.clone(),
        );
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
        let body = media_session(
            &gpu,
            mon,
            &a,
            &mut cap,
            &mut session,
            &remap,
            &blocking,
            &blocked,
            &stop,
            &end,
            &say,
        )?;
        end.store(false, Ordering::Relaxed);
        drop(session); // releases anything the viewer still held
        let report = format!("{head}{body}");
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
    Ok(())
}

/// Capture on `mon` again after the screens changed: found afresh by name
/// (its handle goes stale when Windows rearranges its screens: m5-test19's
/// fallback failed on it and the service's helper restarted six times),
/// retried for a moment while Windows settles.
pub(crate) fn capture_again(
    gpu: &d3d::Gpu,
    mon: &capture::Monitor,
    desktops: bool,
) -> Res<capture::Capture> {
    let mut last = String::new();
    for _ in 0..12 {
        let now = capture::monitors()?;
        let m = now
            .iter()
            .find(|m| m.name == mon.name)
            .or_else(|| now.iter().find(|m| m.primary))
            .cloned();
        if let Some(m) = m {
            match capture::Capture::start(gpu, &m, desktops) {
                Ok(c) => return Ok(c),
                Err(e) => last = e.to_string(),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err(format!("couldn't capture this PC's screen again: {last}").into())
}

/// `glass-cli nvenc-test`: see `nvenc::self_test`.
pub fn nvenc_test(args: &[String]) -> Res<()> {
    nvenc::self_test(args)
}

pub fn decode444_test(args: &[String]) -> Res<()> {
    decode444::self_test(args)
}

/// Where this machine's session logs are, for a results-only connection
/// (v16): its own logs, the service's last session, and (when this is the
/// service, as SYSTEM) the signed-in users' logs.
pub fn register_results() {
    use glass_core::results::add_dir;
    add_dir(slog::dir());
    add_dir(service::data_dir().join("agent-results.txt"));
    let users = std::path::PathBuf::from(
        std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()) + "\\Users",
    );
    if let Ok(rd) = std::fs::read_dir(users) {
        for e in rd.flatten() {
            let d = e
                .path()
                .join("AppData")
                .join("Local")
                .join("Glass")
                .join("logs");
            if d.is_dir() {
                add_dir(d);
            }
        }
    }
}

/// One viewer's session on this screen: match its resolution where the
/// screen offers a better mode (M3f), stream until it goes, put the screen
/// back. Returns the results (after the header line).
#[allow(clippy::too_many_arguments)]
pub(crate) fn media_session<V: ViewerLink>(
    gpu: &d3d::Gpu,
    mon: &capture::Monitor,
    a: &Args,
    cap: &mut capture::Capture,
    session: &mut V,
    remap: &std::sync::Mutex<Option<inject::Rect>>,
    blocking: &AtomicU8,
    blocked: &AtomicU64,
    stop: &std::sync::atomic::AtomicBool,
    end: &std::sync::atomic::AtomicBool,
    say: &dyn Fn(&str),
) -> Res<String> {
    let (mut w, mut h) = cap.size();
    let mut fps = mon.refresh_hz;
    let mut switched = None;
    // The virtual display, if installed (ADR 0030): the only screen while
    // viewed, at the size the view mode asks for; the PC's own monitor goes
    // dark meanwhile (the user's choice).
    let mut virt_note = String::new();
    // The scorecard (ADR 0033): the test scene at the viewer's size and
    // refresh instead of this screen; nothing here changes.
    let scorecard = session.viewer().caps & glass_core::protocol::CAP_SCORECARD != 0;
    if scorecard {
        let v = session.viewer();
        let (sw, sh) = if v.display_w > 0 && v.display_h > 0 {
            (v.display_w as u32, v.display_h as u32)
        } else {
            (w, h)
        };
        let hz = if v.refresh_hz > 0 {
            v.refresh_hz as u32
        } else {
            fps
        };
        *cap = capture::Capture::Scene(Box::new(scene::SceneSource::new(&gpu.device, sw, sh, hz)?));
        (w, h) = cap.size();
        fps = hz;
        say(&format!(
            "Scorecard: showing the test scene at {w}x{h}, {fps} Hz."
        ));
        virt_note = format!(
            "scorecard: the test scene at {w}x{h} @ {fps} Hz instead of this PC's screen (ADR 0033)"
        );
    }
    // View modes (the viewer's Ctrl+Alt+Shift+R or menu): each one is the
    // virtual display at another size, rebuilt when the viewer switches.
    let modes = vdisplay::installed() && !scorecard;
    let mut mode = session.viewer().mode;
    let mut chroma_pref: Option<bool> = None;
    let (base_w, base_h, base_fps) = (w, h, fps);
    let mut report = String::new();
    let mut blocked_by: Vec<String> = Vec::new();
    let mut blocked_us = 0u64;
    loop {
        (w, h, fps) = (base_w, base_h, base_fps);
        let mut virt: Option<vscreen::Session> = None;
        if modes {
            let v = session.viewer();
            match vscreen::size_for(mode, &v, (mon.width, mon.height)) {
                None => {
                    virt_note = "virtual display: not used (the viewer didn't say its size)".into()
                }
                Some((vw, vh, hz)) => match vscreen::Session::start(vw, vh, hz, gpu, a.desktops) {
                    Ok(vs) => {
                        let found = capture::monitors()?.into_iter().find(|m| m.name == vs.name);
                        match found.map(|nm| capture_again(gpu, &nm, a.desktops).map(|c| (nm, c))) {
                            Some(Ok((nm, c))) => {
                                *cap = c;
                                (w, h) = cap.size();
                                fps = nm.refresh_hz.max(1);
                                *remap.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(inject::Rect {
                                        left: nm.left,
                                        top: nm.top,
                                        width: nm.width as i32,
                                        height: nm.height as i32,
                                    });
                                say(&format!(
                                    "Virtual display: {w}x{h} at {fps} Hz for the viewer ({}); this PC's monitor is off while viewed.",
                                    vscreen::mode_label(mode)
                                ));
                                virt_note = format!(
                                    "virtual display: {w}x{h} @ {fps} Hz, {} (the viewer's {}x{} at {}%), this PC's monitor off while viewed",
                                    vscreen::mode_label(mode),
                                    v.display_w,
                                    v.display_h,
                                    v.scale_pct
                                );
                                virt = Some(vs);
                            }
                            other => {
                                drop(vs);
                                let why = match other {
                                    Some(Err(e)) => e.to_string(),
                                    _ => "its screen wasn't found".into(),
                                };
                                virt_note = format!(
                                    "virtual display: not used ({why}); this PC's own screen instead"
                                );
                                *cap = capture_again(gpu, mon, a.desktops)?;
                            }
                        }
                    }
                    Err(e) => {
                        virt_note =
                            format!("virtual display: not used ({e}); this PC's own screen instead")
                    }
                },
            }
        }
        if a.match_res && virt.is_none() && !scorecard && switched.is_none() {
            let v = session.viewer();
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
                            *cap = capture::Capture::start(gpu, &nm, a.desktops)?;
                            (w, h) = cap.size();
                            fps = nm.refresh_hz;
                            *remap.lock().unwrap_or_else(|e| e.into_inner()) = Some(inject::Rect {
                                left: nm.left,
                                top: nm.top,
                                width: nm.width as i32,
                                height: nm.height as i32,
                            });
                            say(&format!(
                                "Matched the viewer: this screen is {}x{} for now (was {}x{}).",
                                w, h, cur.width, cur.height
                            ));
                        }
                    }
                }
            }
        }
        let session_result = run_session(
            gpu,
            cap,
            session,
            a,
            (w, h),
            fps,
            View {
                mode: if virt.is_some() {
                    mode
                } else {
                    glass_core::protocol::MODE_HOST_SCREEN
                },
                switchable: modes,
                chroma444: chroma_pref,
            },
            switched.is_some() || virt.is_some() || scorecard,
            blocking,
            stop,
            end,
        );
        if let Some(vs) = virt.take() {
            // The PC's own screens back (even if the session failed), and
            // capture on them for whatever comes next.
            drop(vs);
            std::thread::sleep(std::time::Duration::from_millis(500));
            *remap.lock().unwrap_or_else(|e| e.into_inner()) = None;
            if let Ok(c) = capture_again(gpu, mon, a.desktops) {
                *cap = c;
            }
            say("This PC's monitor is back.");
        }
        if scorecard {
            if let Ok(c) = capture_again(gpu, mon, a.desktops) {
                *cap = c;
            }
        }
        let (r, by, us, next) = session_result?;
        report.push_str(&r);
        if !virt_note.is_empty() {
            let _ = writeln!(report, "  {virt_note}");
        }
        for b in by {
            if !blocked_by.contains(&b) {
                blocked_by.push(b);
            }
        }
        blocked_us += us;
        match next {
            Some((m, c)) if session.connected() => {
                say(&format!(
                    "The viewer switched the view to {}.",
                    vscreen::mode_label(m)
                ));
                let _ = writeln!(
                    report,
                    "  view changed to {} (the virtual display made again at its size)\n",
                    vscreen::mode_label(m)
                );
                mode = m;
                chroma_pref = Some(c);
            }
            _ => break,
        }
    }
    if let Some(orig) = switched {
        display::restore(&mon.name);
        let _ = writeln!(
            report,
            "  resolution: matched the viewer at {}x{} (this screen is {}x{}; put back)",
            w, h, orig.width, orig.height
        );
        say(&format!("Screen back to {}x{}.", orig.width, orig.height));
    }
    if let Some(counters) = session.counters() {
        let ic = counters.input;
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
    }
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
    Ok(report)
}

/// The view a run of the session shows: the mode it tells the viewer,
/// whether the viewer can switch (a virtual display), and the colour to
/// keep from the run before (`None`: what the viewer asked for).
struct View {
    mode: u8,
    switchable: bool,
    chroma444: Option<bool>,
}

/// A run's report, what blocked input (names, µs), and the view and colour
/// the viewer switched to, if that's why it ended.
type RunEnd = (String, Vec<String>, u64, Option<(u8, bool)>);

#[allow(clippy::too_many_arguments)]
fn run_session<V: ViewerLink>(
    gpu: &d3d::Gpu,
    cap: &mut capture::Capture,
    session: &mut V,
    a: &Args,
    (w, h): (u32, u32),
    fps: u32,
    view: View,
    resized: bool,
    blocking: &AtomicU8,
    stop: &std::sync::atomic::AtomicBool,
    end: &std::sync::atomic::AtomicBool,
) -> Res<RunEnd> {
    // Keep the newest frame captured while waiting: a still screen sends
    // nothing new, so it's the viewer's first picture (m3f-test13: nothing
    // showed until something changed on the host). The service's capture
    // starts again for a fresh full frame.
    cap.refresh();
    let mut first = None;
    while let Some(f) = cap.next_frame()? {
        first = Some(f);
    }
    // A picture to start with, even from a still screen (m5-test18: a
    // session after the screens changed sent nothing for 35 s).
    for _ in 0..10 {
        if first.is_some() {
            break;
        }
        cap.wait(50);
        while let Some(f) = cap.next_frame()? {
            first = Some(f);
        }
    }
    let make = |chroma444: bool| {
        nvenc::Video::new(
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
        mode: view.mode,
    };
    // Colour: what the viewer asked for if it can decode 4:4:4 (M3d);
    // otherwise 4:2:0. `--chroma` only sets what Welcome announced.
    let v = session.viewer();
    let can_444 = v.caps & glass_core::protocol::CAP_HEVC_444 != 0;
    let mut chroma444 = can_444
        && view
            .chroma444
            .unwrap_or(v.caps & glass_core::protocol::CAP_PREFER_420 == 0);
    let mut next_view: Option<(u8, bool)> = None;
    if chroma444 != a.chroma444 || resized || session.announce_stream() {
        session.send_stream_change(info(chroma444))?;
    }
    let mut enc = make(chroma444)?;
    let mut run = HostRun::new();
    run.chroma444 = chroma444;
    run.encoder = format!(
        "{} {}",
        enc.describe(),
        if chroma444 { "4:4:4" } else { "4:2:0" }
    );
    let mut pointer = cursor::CursorWatch::new();
    let mut watch = blocker::BlockerWatch::new();
    let mut blocked_since = None;
    // The clipboard, both ways (M5c), when this process runs it: a thread
    // of its own, talking to the session's clipboard link.
    let clip_here = a.clipboard && session.clip_session().is_some();
    let _clip = session.clip_session().filter(|_| a.clipboard).map(|s| {
        s.set_file_dir(Some(clip::file_dir(a.desktops, "host")));
        clip::ClipSync::start(s.clip_link(), false)
    });
    // The sound (M4b), when this process hosts the session itself; the
    // speakers stay silent here meanwhile.
    let sound = session
        .clip_session()
        .map(|s| sound::Capture::start(s.audio_out(), true));
    let t0 = clock::now_us();
    let log = session.counters().map(|_| slog::SessionLog::start("host"));
    let mut next_log = t0 + slog::EVERY_US;
    let mut pending_idr = true;
    // The frame-rate cap: twice the viewer's refresh (it says in Hello),
    // never more than this screen's. Exactly its refresh (m5-test1..3)
    // made motion uneven: this PC's clock and the viewer's screen drift
    // against each other, so some refreshes got a fresh frame and some an
    // old one. At twice, the viewer always has a fresh one to pick at its
    // refresh, without the load of everything (165 fps: m4b-test2's full
    // queue). 90 % of the interval.
    let viewer_hz = session.viewer().refresh_hz as u32;
    let full_hz = if viewer_hz == 0 {
        fps
    } else {
        (viewer_hz * 2).min(fps.max(1))
    };
    // Under a viewer's limit, its own refresh (half): the first, free lever.
    let low_hz = if viewer_hz == 0 {
        fps
    } else {
        viewer_hz.min(fps.max(1))
    };
    let mut send_hz = full_hz;
    let mut min_gap = 900_000 / send_hz.max(1) as u64;
    // The encoder's own lever: when a frame takes longer to encode than 80 %
    // of the gap between frames, the next step down (the viewer's refresh,
    // then half, a third); each is a whole number of the viewer's refreshes,
    // so motion stays even instead of frames going late at random. Up again
    // after 1 s with room (m5-test37's scorecard: the RTX 2060 took 18 ms a
    // frame for a 4:4:4 scroll at 165 Hz, and 669 of 1000 refreshes missed).
    let load_steps: Vec<u32> = {
        let mut v = vec![full_hz];
        if viewer_hz > 0 {
            for k in 1..=3 {
                let hz = (viewer_hz / k).max(1);
                if hz < *v.last().unwrap_or(&u32::MAX) {
                    v.push(hz);
                }
            }
        }
        v
    };
    let mut load_level = 0usize;
    let mut load_changed_us = 0u64;
    // Over the line since (one big frame, a page or a card changing, isn't
    // a load: it has to last 0.3 s).
    let mut load_over_since: Option<u64> = None;
    let ours = a.mbps as i64 * 1_000_000;
    let mut limit: Option<i64> = None;
    let mut last_step_us = 0u64;
    let mut last_sent = 0u64;
    let mut held: Option<u64> = None;
    // Still-picture refinement (M7, brought forward): when the screen stops
    // changing, the last picture is encoded again a few times, so detail
    // that a squeezed bitrate left out arrives once there's room (an image
    // opened on a weak link sharpens in a moment instead of staying blocky).
    let mut last_change_us = 0u64;
    let mut refines_left = 0u32;
    let mut last_refine_us = 0u64;
    // What the changes since the last one cost: a clock ticking or a pointer
    // blinking (a few KB) doesn't need a sharp copy; a photo opening does.
    let mut changed_bytes = 0usize;
    run.send_hz = send_hz;
    run.full_hz = full_hz;
    // 1 ms timer resolution for the session: Windows' default is 15.6 ms,
    // so the cap's "send the held frame in 1 ms" took 16 ms and frames went
    // at 29 fps instead of 60 (m5-test1). Per process on Windows 11.
    let _timer = FineTimer::new();
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
        // A frame held by the frame-rate cap goes as soon as its turn
        // comes, so wait no longer than that.
        let wait_ms = match held {
            Some(_) => (min_gap.saturating_sub(clock::now_us().saturating_sub(last_sent)) / 1_000)
                .clamp(1, 5) as u32,
            None => 5,
        };
        cap.wait(wait_ms);
        // The running log (in-app host: if Glass is killed, its numbers so
        // far are still there; m4-test1).
        if let Some(log) = &log {
            let now = clock::now_us();
            if now >= next_log {
                next_log = now + slog::EVERY_US;
                log.write(&format!(
                    "{}\n{}",
                    crate::banner(),
                    host_report(session, &run, a, cap, t0, clip_here, sound.as_ref(), None)
                ));
            }
        }
        pending_idr |= session.take_keyframe_request();
        // Rate control (M5): the viewer's limit, never above ours. FFmpeg's
        // NVENC makes a keyframe at every bitrate change (m5-test10: 68 IDRs
        // for 63 changes, each at the worst moment), so the first lever is
        // the frame rate: twice the viewer's refresh down to its refresh,
        // half the bitrate, no keyframe, nothing visible lost. Only if that
        // isn't enough, the bitrate, in coarse steps, at most one down a
        // second and one up per 5 s.
        if let Some(l) = session.take_video_rate() {
            limit = (l > 0).then_some((l as i64).min(ours));
        }
        {
            let now = clock::now_us();
            let use_bps = run.avg_bps as i64;
            let cur = if run.rate_now == 0 {
                ours
            } else {
                run.rate_now
            };
            let since = now.saturating_sub(last_step_us);
            // Down at once; back up only from the top of the bitrate steps,
            // with room to spare and 5 s calm (m5-test11: jumping from 60 fps
            // at ~10 Mbps straight back to the full rate at 80 overflowed the
            // link 18 times, and each loss cost a keyframe).
            let want_hz = match limit {
                Some(l) if use_bps > 0 && l < use_bps * 9 / 10 => low_hz,
                _ if send_hz == low_hz
                    && (cur < ours
                        || since < 5_000_000
                        || matches!(limit, Some(l) if l < use_bps * 2)) =>
                {
                    low_hz
                }
                _ => full_hz,
            };
            // The encoder's lever (above), checked against the average.
            let gap = |i: usize| 1e6 / load_steps[i] as f64;
            let avg = run.enc_avg_us;
            if avg > 0.0 {
                let over = load_level + 1 < load_steps.len() && avg > gap(load_level) * 0.8;
                if !over {
                    load_over_since = None;
                } else if load_over_since.is_none() {
                    load_over_since = Some(now);
                }
                if over && load_over_since.is_some_and(|t| now.saturating_sub(t) >= 300_000) {
                    load_over_since = None;
                    load_level += 1;
                    load_changed_us = now;
                    run.load_lowered += 1;
                    let hz = load_steps[load_level];
                    run.load_lowest_hz = if run.load_lowest_hz == 0 {
                        hz
                    } else {
                        run.load_lowest_hz.min(hz)
                    };
                } else if load_level > 0
                    && avg < gap(load_level - 1) * 0.6
                    && now.saturating_sub(load_changed_us) >= 1_000_000
                {
                    load_level -= 1;
                    load_changed_us = now;
                }
            }
            let want_hz = want_hz.min(load_steps[load_level]);
            if want_hz != send_hz {
                send_hz = want_hz;
                min_gap = 900_000 / send_hz.max(1) as u64;
                run.fps_changes += 1;
                last_step_us = now;
                if send_hz == low_hz && full_hz != low_hz {
                    run.fps_lowered += 1;
                }
            }
            // At the viewer's refresh, what it still needs from the encoder:
            // down straight to the step under the limit, up one step at a
            // time, only with a quarter to spare.
            let step = match limit {
                _ if send_hz != low_hz => ours,
                None => step_up(cur, ours),
                Some(l) => {
                    let fits = bitrate_step(l, ours);
                    if fits < cur {
                        fits
                    } else {
                        step_up(cur, ours).min(bitrate_step(l * 4 / 5, ours).max(cur))
                    }
                }
            };
            let ok = if step < cur {
                since >= 1_000_000
            } else {
                since >= 2_000_000
            };
            if step != cur && ok {
                last_step_us = now;
                enc.set_bitrate(step);
                run.rate_changes += 1;
                run.rate_now = step;
                run.rate_lo = if run.rate_lo == 0 {
                    step
                } else {
                    run.rate_lo.min(step)
                };
            }
        }
        run.send_hz = send_hz;
        // The viewer switched view (a virtual display at another size: the
        // caller makes it again) or colour (4:2:0 <-> 4:4:4).
        if let Some((mode, allow)) = session.take_mode_request() {
            let want = allow && can_444;
            if view.switchable && mode != view.mode {
                next_view = Some((mode, want));
                break;
            }
            if want != chroma444 {
                match make(want) {
                    Ok(e) => {
                        enc = e;
                        chroma444 = want;
                        run.chroma444 = want;
                        run.encoder = format!(
                            "{}, then {} {}",
                            run.encoder,
                            enc.describe(),
                            if want { "4:4:4" } else { "4:2:0" }
                        );
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
        // The service's helper runs as SYSTEM on whichever desktop is
        // showing: nothing blocks it (the lock screen and UAC prompts are
        // what it's for).
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
        cap.note_input(session.input_seq());
        while let Some(frame) = cap.next_frame()? {
            let arrival = clock::now_us();
            if frame.size != (w, h) {
                run.skipped += 1; // LATER(M7): handle resolution changes.
                continue;
            }
            got = true;
            run.intervals.tick(arrival);
            run.note_capture(arrival);
            // At most one frame per refresh of the viewer's screen (M5's
            // first step, m4b-test2): a 60 Hz Mac sent 165 fps fell behind,
            // showed 37 fps and lost frames to a full queue. The newest is
            // kept and goes when its turn comes.
            if !pending_idr && arrival.saturating_sub(last_sent) < min_gap {
                enc.keep(&frame.texture);
                held = Some(frame.ts_us);
                run.capped += 1;
                continue;
            }
            held = None;
            last_sent = arrival;
            let idr = std::mem::take(&mut pending_idr);
            let input_seq = session.input_seq();
            enc.encode(&frame.texture, arrival as i64, idr, |data, key, _| {
                // Real changes only: a keyframe (the one after a quality
                // keyframe, or one asked for) isn't new content, and
                // counting it re-armed the next quality keyframe at once.
                if !key {
                    changed_bytes += data.len();
                }
                run.on_packet(session, arrival, input_seq, data, key)
            })?;
        }
        if got {
            last_change_us = clock::now_us();
            refines_left = REFINE_PASSES;
        }
        if !got
            && held.is_none()
            && !pending_idr
            && refines_left > 0
            && changed_bytes >= REFINE_MIN_CHANGE
            && session.streaming()
            && enc.has_last()
        {
            let now = clock::now_us();
            if now.saturating_sub(last_change_us) >= REFINE_AFTER_US
                && now.saturating_sub(last_refine_us) >= REFINE_EVERY_US
            {
                refines_left -= 1;
                changed_bytes = 0;
                last_refine_us = now;
                last_sent = now;
                run.refines += 1;
                let input_seq = session.input_seq();
                let mut bytes = 0;
                // One keyframe of the still picture (the encoder's buffer
                // lets it be as big as it needs).
                let qp = nvenc::still_qp(limit.unwrap_or(0));
                run.in_quality = true;
                let r = enc.encode_last_quality(now as i64, qp, |data, key, _| {
                    bytes += data.len();
                    run.on_packet(session, now, input_seq, data, key)
                });
                run.in_quality = false;
                r?;
                run.refine_encode
                    .push(clock::now_us().saturating_sub(now) as u32);
                run.refine_bytes += bytes as u64;
            }
        }
        if held.is_some() && clock::now_us().saturating_sub(last_sent) >= min_gap {
            held = None;
            let now = clock::now_us();
            last_sent = now;
            run.capped = run.capped.saturating_sub(1); // it went after all
            let idr = std::mem::take(&mut pending_idr);
            let input_seq = session.input_seq();
            enc.encode_last(now as i64, idr, |data, key, _| {
                run.on_packet(session, now, input_seq, data, key)
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
    let r = host_report(
        session,
        &run,
        a,
        cap,
        t0,
        clip_here,
        sound.as_ref(),
        Some(run.error.as_deref().unwrap_or("viewer left")),
    );
    if let Some(log) = &log {
        log.write(&format!(
            "{}
{r}",
            crate::banner()
        ));
    }
    if let Some(since) = blocked_since {
        run.blocked_us += clock::now_us() - since;
    }
    Ok((r, run.blocked_by, run.blocked_us, next_view))
}

/// A session's results so far: at the end (`ended`: why), or for the
/// running log while it's on.
#[allow(clippy::too_many_arguments)]
fn host_report<V: ViewerLink>(
    session: &V,
    run: &HostRun,
    a: &Args,
    cap: &capture::Capture,
    t0: u64,
    clip_here: bool,
    sound: Option<&sound::Capture>,
    ended: Option<&str>,
) -> String {
    let secs = (clock::now_us() - t0) as f64 / 1e6;
    let mut r = String::new();
    match ended {
        Some(why) => {
            let _ = writeln!(
                r,
                "Session with {} ended after {:.1} s ({why})",
                session.peer(),
                secs
            );
        }
        None => {
            let _ = writeln!(
                r,
                "Session with {}, still on after {:.1} s (written every 10 s)",
                session.peer(),
                secs
            );
        }
    }
    if let Some(c) = session.counters() {
        let _ = writeln!(
            r,
            "  frames sent {} ({:.1} fps), IDRs {} (idle re-sends {}), viewer keyframe requests {}, not sent (viewer not ready) {}, skipped {} | still-picture quality frames {} ({:.0} KB) | encoders: {}",
            c.frames_sent,
            c.frames_sent as f64 / secs,
            run.idrs,
            run.idle_idrs,
            c.keyframe_requests,
            run.not_sent,
            run.skipped,
            run.refines,
            run.refine_bytes as f64 / 1024.0,
            run.encoder
        );
        if let Some(m) = run.moments_line() {
            let _ = writeln!(r, "{m}");
        }
        let _ = writeln!(
            r,
            "  rate control (M5): bitrate {} | {} changes{} | frame rate lowered to the viewer's refresh {} times",
            if run.rate_changes == 0 || run.rate_now == (a.mbps as i64 * 1_000_000) {
                format!(
                    "{:.0} Mbps (no limit from the viewer)",
                    (a.mbps as i64 * 1_000_000) as f64 / 1e6
                )
            } else {
                format!(
                    "{:.1} Mbps now, the viewer's limit",
                    run.rate_now as f64 / 1e6
                )
            },
            run.rate_changes,
            if run.rate_lo > 0 && run.rate_lo < (a.mbps as i64 * 1_000_000) {
                format!(", lowest {:.1} Mbps", run.rate_lo as f64 / 1e6)
            } else {
                String::new()
            },
            run.fps_lowered
        );
        if run.send_hz > 0 {
            let _ = writeln!(
                r,
                "  frame rate (M5): at most {} fps, twice the viewer's screen | {} frames held back (a newer one went instead) | lowered for the encoder's load {} times{}",
                run.full_hz.max(run.send_hz),
                run.capped,
                run.load_lowered,
                if run.load_lowest_hz > 0 {
                    format!(", lowest {} fps", run.load_lowest_hz)
                } else {
                    String::new()
                }
            );
        }
        let _ = writeln!(
            r,
            "  bitrate {:.1} Mbps avg | {} packets",
            c.bytes_sent as f64 * 8.0 / secs / 1e6,
            c.packets_sent
        );
        let _ = writeln!(
            r,
            "  FEC (M4): {} parity packets ({:.1}% of packets) | the viewer's latest reported loss {:.2}% | NACKs answered {} ({} packets resent)",
            c.parity_packets,
            c.parity_packets as f64 * 100.0 / c.packets_sent.max(1) as f64,
            c.loss_ppm as f64 / 1e4,
            c.nacks,
            c.packets_resent
        );
        let _ = writeln!(
            r,
            "  network (M4): video marked for Wi-Fi priority: {} | keep-awakes sent {}",
            qos_word(c.qos),
            c.keepalives
        );
        let _ = writeln!(
            r,
            "  pacing (M5): {} frames spread over {:.1} ms on average (longest {:.1} ms) | {} sent at once for a newer frame",
            c.paced_frames,
            c.pace_us_total as f64 / c.paced_frames.max(1) as f64 / 1e3,
            c.pace_us_max as f64 / 1e3,
            c.hurried_frames
        );
        let _ = writeln!(r, "  {}", run.wifi.line());
        if let Some(snd) = sound {
            let _ = writeln!(
                r,
                "  {} | {} sent, {} resent for the viewer",
                snd.line(),
                c.audio_frames,
                c.audio_resent
            );
        }
    } else {
        let _ = writeln!(
            r,
            "  frames encoded {} ({:.1} fps), IDRs {} (idle re-sends {}), skipped {} | still-picture quality frames {} ({:.0} KB) | encoders: {}",
            run.frame_id,
            run.frame_id as f64 / secs,
            run.idrs,
            run.idle_idrs,
            run.skipped,
            run.refines,
            run.refine_bytes as f64 / 1024.0,
            run.encoder
        );
    }
    let _ = writeln!(r, "{}", fmt_line("encode", &run.encode.summary()));
    for (name, ring) in [
        ("  in 4:4:4", &run.encode_444),
        ("  in 4:2:0", &run.encode_420),
        ("  quality frames", &run.refine_encode),
    ] {
        if ring.summary().total > 0 {
            let _ = writeln!(r, "{}", fmt_line(name, &ring.summary()));
        }
    }
    let _ = writeln!(r, "{}", fmt_line("send (packetise)", &run.send.summary()));
    let _ = writeln!(
        r,
        "{}",
        fmt_line("capture interval", &run.intervals.intervals.summary())
    );
    let _ = writeln!(r, "{}", size_line(&run.size.summary()));
    if clip_here {
        if let Some(s) = session.clip_session() {
            let _ = writeln!(
                r,
                "  clipboard: {}",
                clip::summary(&s.clip_progress(), "viewer")
            );
        }
    } else if !a.clipboard {
        let _ = writeln!(r, "  clipboard: off");
    }
    if a.desktops {
        let _ = writeln!(
            r,
            "  desktop switches (lock screen, sign-in, UAC) followed: {}",
            cap.desktop_switches()
        );
    }
    r
}

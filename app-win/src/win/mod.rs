//! Windows-only host pieces: D3D11 device, Windows.Graphics.Capture, NVENC.

mod capture;
mod d3d;
mod encode;

use glass_core::clock;
use glass_core::net::{HostListener, HostSession};
use glass_core::packet::OutFrame;
use glass_core::protocol::{StreamInfo, DEFAULT_PORT, FLAG_IDR};
use glass_core::recording::{
    FrameMeta, RecordingHeader, RecordingWriter, CHROMA_420, CHROMA_444, CODEC_HEVC,
};
use glass_core::stats::{IntervalTracker, StatRing, Summary};
use std::error::Error;
use std::fmt::Write as _;

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

    let mut cap = capture::Capture::start(&gpu, mon)?;
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
            not_sent: 0,
            skipped: 0,
            error: None,
        }
    }

    /// Encoder output -> network. Called from inside the encode callback.
    fn on_packet(&mut self, session: &mut HostSession, capture_us: u64, data: &[u8], key: bool) {
        let out = clock::now_us();
        let encode_us = out.saturating_sub(capture_us) as u32;
        let f = OutFrame {
            frame_id: self.frame_id,
            flags: if key { FLAG_IDR } else { 0 },
            capture_ts_us: capture_us,
            encode_us,
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
pub fn host(args: &[String]) -> Res<()> {
    let a = parse(args)?;
    d3d::init_process();
    let monitors = capture::monitors()?;
    let mon = monitors
        .get(a.monitor)
        .ok_or_else(|| format!("no monitor {} (see `glass list`)", a.monitor))?;
    let gpu = d3d::Gpu::new_nvidia()?;
    let listener = if a.local {
        HostListener::bind_on(std::net::Ipv4Addr::LOCALHOST, a.port)?
    } else {
        HostListener::bind(a.port)?
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
    print!("{head}");
    let ip = lan_ip().map_or("(unknown)".to_string(), |ip| ip.to_string());
    println!("On the Mac, open Glass and connect to:  {ip}");
    println!("(Allow glass.exe through Windows Firewall on private networks if asked.)");

    loop {
        let mut cap = capture::Capture::start(&gpu, mon)?;
        let (w, h) = cap.size();
        let info = StreamInfo {
            codec: CODEC_HEVC,
            chroma: if a.chroma444 { CHROMA_444 } else { CHROMA_420 },
            width: w as u16,
            height: h as u16,
            fps: mon.refresh_hz as u16,
        };
        println!("\nWaiting for a viewer on port {} ...", listener.port());
        let mut session = match listener.accept(info) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("viewer handshake failed: {e}");
                continue;
            }
        };
        println!("Viewer connected from {}", session.peer_tcp);
        let report = run_session(&gpu, &mut cap, &mut session, &a, (w, h), mon.refresh_hz)?;
        let report = format!("{head}{report}");
        println!("{report}");
        if let Some(path) = &a.log {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(f, "{report}")?;
            println!("(appended to {path})");
        }
    }
}

fn run_session(
    gpu: &d3d::Gpu,
    cap: &mut capture::Capture,
    session: &mut HostSession,
    a: &Args,
    (w, h): (u32, u32),
    fps: u32,
) -> Res<String> {
    while cap.next_frame()?.is_some() {} // drop frames captured while waiting
    let mut enc = encode::Encoder::new(
        &gpu.device,
        &encode::Config {
            width: w,
            height: h,
            fps: fps.max(1),
            bitrate: a.mbps as i64 * 1_000_000,
            chroma444: a.chroma444,
            keep_last: true,
        },
    )?;
    let mut run = HostRun::new();
    let t0 = clock::now_us();
    let mut pending_idr = true;
    while session.connected() && run.error.is_none() {
        cap.wait(5);
        pending_idr |= session.take_keyframe_request();
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
            enc.encode(&frame.texture, arrival as i64, idr, |data, key, _| {
                run.on_packet(session, arrival, data, key)
            })?;
        }
        if !got && pending_idr && session.streaming() && enc.has_last() {
            // Idle screen: resend the last frame as an IDR so the viewer recovers.
            let now = clock::now_us();
            pending_idr = false;
            run.idle_idrs += 1;
            enc.encode_last_idr(now as i64, |data, key, _| {
                run.on_packet(session, now, data, key)
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
    Ok(r)
}

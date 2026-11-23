//! The Glass service's broker (ADR 0017): it keeps the viewer's connection
//! (listener, pairing, TLS and UDP session, network listing) and never
//! restarts for a Windows session switch. The work is done by helpers in
//! the session on screen (`helper.rs`), over pipes:
//! - the **screen helper** sends encoded frames, the pointer and notices,
//!   and gets the viewer's input and requests;
//! - the **clipboard helper**, running as the signed-in user, gets the
//!   viewer's clipboard messages and sends its own, passed through
//!   unchanged.
//!
//! When the screen helper changes mid-session (the user signed in, Remote
//! Desktop took over or gave the screen back), the new one gets Start
//! again and its first frame is a keyframe: the viewer sees a short pause,
//! not a disconnect.

use super::*;
use glass_core::media_link::{
    encode_clip_to_helper, read_message, write_message, ToBroker, ToHelper,
};
use std::fs::File;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the helpers sent, for the broker's loop. Frames keep the buffer
/// they were read into (`at`: where the picture starts), and the buffer goes
/// back to `POOL` afterwards: no allocation per frame.
enum Msg {
    Frame {
        key: bool,
        capture_us: u64,
        encode_us: u32,
        input_seq: u32,
        buf: Vec<u8>,
        at: usize,
    },
    Cursor {
        serial: u32,
        size: (u16, u16),
        hot: (u16, u16),
        rgba: Vec<u8>,
    },
    CursorVisible(bool),
    Notice(u8),
    Stream(StreamInfo),
    Report(String),
    ScreenUp,
    ScreenGone,
}

static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// A writer to a helper; `None` while there's none.
type Out = Mutex<Option<(File, Vec<u8>)>>;

fn send(out: &Out, m: &ToHelper) {
    let mut g = lock(out);
    if let Some((f, buf)) = g.as_mut() {
        m.encode(buf);
        if write_message(f, buf).is_err() {
            *g = None;
        }
    }
}

/// What the broker knows about the screen helper between sessions.
#[derive(Default)]
struct Screen {
    /// Its screen: width, height, refresh (0 = not known yet).
    display: Mutex<(u16, u16, u16)>,
    /// Its latest clipboard results line (from the clipboard helper).
    clip_line: Mutex<String>,
    /// The viewer's connection, for the clipboard helper's messages while a
    /// session is on: written from the clipboard pipe's thread, so file
    /// data never queues with the video.
    clip_to_viewer: Mutex<Option<glass_core::clipboard::FrameWriter>>,
}

fn read_screen(mut rx: File, tx: mpsc::Sender<Msg>, screen: Arc<Screen>) {
    let _ = tx.send(Msg::ScreenUp);
    loop {
        let mut buf = lock(&POOL).pop().unwrap_or_default();
        let Ok(kind) = read_message(&mut rx, &mut buf) else {
            break;
        };
        let msg = match ToBroker::decode(kind, &buf) {
            Some(ToBroker::Frame {
                key,
                capture_us,
                encode_us,
                input_seq,
                data,
            }) => {
                let at = buf.len() - data.len();
                Msg::Frame {
                    key,
                    capture_us,
                    encode_us,
                    input_seq,
                    at,
                    buf,
                }
            }
            Some(ToBroker::Display { width, height, fps }) => {
                *lock(&screen.display) = (width, height, fps);
                continue;
            }
            Some(ToBroker::Cursor(c)) => Msg::Cursor {
                serial: c.serial,
                size: (c.width, c.height),
                hot: (c.hot_x, c.hot_y),
                rgba: c.rgba.to_vec(),
            },
            Some(ToBroker::CursorVisible(v)) => Msg::CursorVisible(v),
            Some(ToBroker::Notice(n)) => Msg::Notice(n),
            Some(ToBroker::StreamChange(i)) => Msg::Stream(i),
            Some(ToBroker::Report(r)) => Msg::Report(r.to_string()),
            _ => continue,
        };
        if tx.send(msg).is_err() {
            return;
        }
    }
    let _ = tx.send(Msg::ScreenGone);
}

fn read_clip(mut rx: File, screen: Arc<Screen>) {
    let mut buf = Vec::new();
    while let Ok(kind) = read_message(&mut rx, &mut buf) {
        match ToBroker::decode(kind, &buf) {
            Some(ToBroker::ClipFrame(f)) => {
                if let Some(to) = lock(&screen.clip_to_viewer).as_ref() {
                    to(f);
                }
            }
            Some(ToBroker::ClipSummary(s)) => *lock(&screen.clip_line) = s.to_string(),
            _ => {}
        }
    }
}

/// Who may be the clipboard helper: a check on the connecting process.
pub type ClipCheck = Box<dyn Fn(u32) -> bool + Send>;

/// Accepts helpers on `name` for ever, keeping `out` pointing at the newest.
fn serve(
    name: String,
    allow: Vec<String>,
    check: ClipCheck,
    out: Arc<Out>,
    reader: impl Fn(File) + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut server = pipe::Server::new(&name, &allow);
        loop {
            match server.accept(&*check) {
                Ok(link) => {
                    *lock(&out) = Some((link.tx, Vec::with_capacity(4096)));
                    reader(link.rx);
                }
                Err(e) => {
                    println!("Helper pipe {name}: {e}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    });
}

/// `glass-cli broker` (local tests) and the service: keeps viewers'
/// sessions; helpers connect on the `--pipes` name. `*_allow`: SIDs (or
/// SDDL aliases) besides SYSTEM allowed on each pipe; `clip_ok`: who may be
/// the clipboard helper.
pub fn run(
    args: &[String],
    ui: HostUi,
    screen_allow: Vec<String>,
    clip_allow: Vec<String>,
    clip_ok: ClipCheck,
) -> Res<()> {
    let HostUi {
        event,
        line,
        report: mut on_report,
        stop,
        end,
    } = ui;
    let line = Mutex::new(line);
    let say = |s: &str| (lock(&line))(s);
    let a = parse(args)?;
    let device = crate::device()?;
    let opts = HostOptions {
        fixed_pin: a.pin.clone(),
        ask: None,
        events: event,
        host_os: glass_core::protocol::HOST_OS_WINDOWS,
        host_caps: glass_core::protocol::HOST_CAP_444,
    };
    let mut listener = if a.local {
        HostListener::bind_on(std::net::Ipv4Addr::LOCALHOST, a.port, device.clone(), opts)?
    } else {
        HostListener::bind(a.port, device.clone(), opts)?
    };
    let port = listener.port();
    let head = format!(
        "{} | host: the Glass service (connection kept across session switches) | HEVC {} Mbps | port {port}\n",
        crate::banner(),
        a.mbps
    );
    say(head.trim_end());
    if !a.local {
        let id = fingerprint_short(&device.fingerprint());
        let v4 = match lan_ip() {
            Some(std::net::IpAddr::V4(v4)) => Some(v4),
            _ => None,
        };
        if let Err(e) = mdns::advertise(&device.name, port, v4, &id) {
            say(&format!("(Not listed on the network: {e}.)"));
        }
    }

    let (tx, rx) = mpsc::channel::<Msg>();
    let screen = Arc::new(Screen::default());
    let screen_out: Arc<Out> = Arc::new(Mutex::new(None));
    let clip_out: Arc<Out> = Arc::new(Mutex::new(None));
    {
        let (tx1, sc) = (tx.clone(), screen.clone());
        serve(
            format!("{}-screen", a.pipes),
            screen_allow,
            Box::new(|_| true), // the DACL: SYSTEM (and the test user) only
            screen_out.clone(),
            move |r| read_screen(r, tx1.clone(), sc.clone()),
        );
        let sc = screen.clone();
        serve(
            format!("{}-clip", a.pipes),
            clip_allow,
            clip_ok,
            clip_out.clone(),
            move |r| read_clip(r, sc.clone()),
        );
    }
    drop(tx);

    while !stop.load(Ordering::Relaxed) {
        // Let a starting screen helper say what's on screen first, so the
        // viewer's first picture is the right size (m5c-test9 showed a
        // placeholder 1280x720 for a moment).
        let until = clock::now_us() + 5_000_000;
        while lock(&screen.display).0 == 0
            && clock::now_us() < until
            && !stop.load(Ordering::Relaxed)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let (w, h, fps) = *lock(&screen.display);
        let info = StreamInfo {
            codec: CODEC_HEVC,
            chroma: CHROMA_420,
            // Not known yet: the helper's first StreamChange says.
            width: if w == 0 { 1280 } else { w },
            height: if h == 0 { 720 } else { h },
            fps: if fps == 0 { 60 } else { fps },
            mode: glass_core::protocol::MODE_HOST_SCREEN,
        };
        say(&format!("\nWaiting for a viewer on port {port} ..."));
        let so = screen_out.clone();
        let sink: glass_core::net::InputSink =
            Box::new(move |action| send(&so, &ToHelper::Input(action)));
        let accepted = listener.accept(info, sink);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut session = match accepted {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(e) => {
                say(&format!("A connection attempt failed: {e}"));
                continue;
            }
        };
        say(&format!(
            "Viewer connected: \"{}\" from {} (encrypted)",
            session.peer_name,
            session.peer_tcp.ip()
        ));
        // Stale messages from before (a helper's last frames) go.
        while let Ok(m) = rx.try_recv() {
            if let Msg::Frame { buf, .. } = m {
                lock(&POOL).push(buf);
            }
        }
        // The viewer's clipboard messages go to the clipboard helper.
        let co = clip_out.clone();
        session.relay_clipboard(Some(Box::new(move |f: &[u8]| {
            let mut g = lock(&co);
            if let Some((file, buf)) = g.as_mut() {
                encode_clip_to_helper(f, buf);
                if write_message(file, buf).is_err() {
                    *g = None;
                }
            }
            true
        })));
        *lock(&screen.clip_to_viewer) = Some(session.clip_sender());
        let start = ToHelper::Start {
            hello: session.viewer,
            peer: session.peer_tcp.to_string(),
        };
        send(&screen_out, &start);
        let t0 = clock::now_us();
        let (mut frame_id, mut idrs, mut not_sent, mut switches) = (0u32, 0u32, 0u32, 0u32);
        let mut send_ring = StatRing::new();
        // Encoded in the helper -> handed to the network here (the pipe hop).
        let mut hop = StatRing::new();
        let mut seq = 0u32;
        let mut helper_report = String::new();
        let mut gap_since: Option<u64> = None;
        let mut gaps = StatRing::new();
        let mut error = None;
        while session.connected() && !stop.load(Ordering::Relaxed) && !end.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_millis(2)) {
                Ok(Msg::Frame {
                    key,
                    capture_us,
                    encode_us,
                    input_seq,
                    buf,
                    at,
                }) => {
                    if let Some(since) = gap_since.take() {
                        gaps.push((clock::now_us() - since) as u32);
                    }
                    let f = OutFrame {
                        frame_id,
                        flags: if key { FLAG_IDR } else { 0 },
                        capture_ts_us: capture_us,
                        encode_us,
                        input_seq,
                    };
                    frame_id = frame_id.wrapping_add(1);
                    let out = clock::now_us();
                    hop.push(out.saturating_sub(capture_us + encode_us as u64) as u32);
                    match session.send_frame(&f, &buf[at..]) {
                        Ok(true) => {
                            send_ring.push((clock::now_us() - out) as u32);
                            idrs += key as u32;
                        }
                        Ok(false) => not_sent += 1,
                        Err(e) => error = Some(format!("send failed: {e}")),
                    }
                    lock(&POOL).push(buf);
                }
                Ok(Msg::Cursor {
                    serial,
                    size,
                    hot,
                    rgba,
                }) => {
                    let _ = session.send_cursor(&CursorShape {
                        serial,
                        width: size.0,
                        height: size.1,
                        hot_x: hot.0,
                        hot_y: hot.1,
                        rgba: &rgba,
                    });
                }
                Ok(Msg::CursorVisible(v)) => {
                    let _ = session.send_cursor_visible(v);
                }
                Ok(Msg::Notice(n)) => {
                    let _ = session.send_notice(n);
                }
                Ok(Msg::Stream(i)) => {
                    let _ = session.send_stream_change(i);
                }
                Ok(Msg::Report(r)) => helper_report = r,
                Ok(Msg::ScreenUp) => {
                    // A new screen helper mid-session: carry on with it.
                    switches += 1;
                    send(&screen_out, &start);
                    send(&screen_out, &ToHelper::InputSeq(seq));
                    say("The screen helper changed (a Windows session switch); the viewer stays connected.");
                }
                Ok(Msg::ScreenGone) => gap_since = Some(clock::now_us()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if error.is_some() {
                break;
            }
            if session.take_keyframe_request() {
                send(&screen_out, &ToHelper::Keyframe);
            }
            if let Some((mode, allow_444)) = session.take_mode_request() {
                send(&screen_out, &ToHelper::SetMode { mode, allow_444 });
            }
            let now = session.input_seq();
            if now != seq {
                seq = now;
                send(&screen_out, &ToHelper::InputSeq(seq));
            }
        }
        end.store(false, Ordering::Relaxed);
        // The helper puts the screen back and sends its part of the results.
        send(&screen_out, &ToHelper::Stop);
        let until = clock::now_us() + 3_000_000;
        while helper_report.is_empty() && clock::now_us() < until {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Msg::Report(r)) => helper_report = r,
                Ok(Msg::Frame { buf, .. }) => lock(&POOL).push(buf),
                _ => {}
            }
        }
        session.relay_clipboard(None);
        *lock(&screen.clip_to_viewer) = None;
        let secs = (clock::now_us() - t0) as f64 / 1e6;
        let c = session.counters();
        let ic = c.input;
        let mut r = String::new();
        let _ = writeln!(
            r,
            "Session with \"{}\" ({}) ended after {:.1} s ({})",
            session.peer_name,
            session.peer_tcp,
            secs,
            error.as_deref().unwrap_or("viewer left")
        );
        let _ = writeln!(
            r,
            "  frames sent {} ({:.1} fps), IDRs {idrs}, viewer keyframe requests {}, not sent (viewer not ready) {not_sent}",
            c.frames_sent,
            c.frames_sent as f64 / secs,
            c.keyframe_requests
        );
        let _ = writeln!(
            r,
            "  bitrate {:.1} Mbps avg | {} packets",
            c.bytes_sent as f64 * 8.0 / secs / 1e6,
            c.packets_sent
        );
        let _ = writeln!(r, "{}", fmt_line("send (packetise)", &send_ring.summary()));
        let _ = writeln!(r, "{}", fmt_line("helper -> service", &hop.summary()));
        let _ = writeln!(
            r,
            "  session switches with the viewer kept connected: {switches}{}",
            if gaps.summary().count > 0 {
                format!(
                    " (picture paused for {})",
                    fmt_line("", &gaps.summary()).trim()
                )
            } else {
                String::new()
            }
        );
        // The screen helper's lines, without its own first line.
        for l in helper_report.lines().skip(1) {
            let _ = writeln!(r, "{l}");
        }
        let _ = writeln!(
            r,
            "  input: {} packets | {} keys, {} clicks, {} scrolls, {} moves | repaired {}",
            ic.packets, ic.keys, ic.clicks, ic.scrolls, ic.moves, ic.repaired
        );
        let _ = writeln!(
            r,
            "  rejected UDP datagrams (failed decryption or replays): {}",
            c.rejected_datagrams
        );
        let clip_line = lock(&screen.clip_line).clone();
        let _ = writeln!(
            r,
            "  clipboard: {}",
            if !a.clipboard {
                "off".to_string()
            } else if clip_line.is_empty() {
                "no clipboard helper (nobody signed in on this PC's screen)".to_string()
            } else {
                clip_line
            }
        );
        drop(session);
        let report = format!("{head}{r}");
        say(&report);
        on_report(&report);
    }
    mdns::unadvertise();
    Ok(())
}

/// Wakes a broker blocked waiting for a viewer (after setting its stop flag).
pub fn wake(port: u16) {
    let _ = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        Duration::from_millis(500),
    );
}

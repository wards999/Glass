//! The Glass service's helpers (ADR 0017). The service (the broker) keeps the
//! viewer's connection; these do the work in the session on the PC's screen
//! and talk to it over pipes (`pipe.rs`, messages in core `media_link`).
//!
//! - `screen` (`glass-cli agent`, SYSTEM): capture, encoding, input, the
//!   pointer, resolution matching. The same session code as the in-app host
//!   (`media_session`), with the broker standing in for the viewer.
//! - `clipboard` (`glass-cli clip-agent`, the signed-in user): the clipboard
//!   link and its thread, exactly as in Glass.exe. The broker passes its
//!   messages to and from the viewer.

use super::*;
use glass_core::media_link::{read_message, write_message, ToBroker, ToHelper, CLIP_TO_HELPER};
use glass_core::net::{InputSink, ViewerHello};
use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Mutex;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the broker last said (filled by the pipe's reader thread).
#[derive(Default)]
struct FromBroker {
    /// A viewer to start for, not yet taken.
    start: Mutex<Option<(ViewerHello, String)>>,
    /// Bumped by every Start: a session ends when a newer one comes.
    gen: AtomicU32,
    stopped: AtomicBool,
    gone: AtomicBool,
    keyframe: AtomicBool,
    mode: Mutex<Option<(u8, bool)>>,
    input_seq: AtomicU32,
    sink: Mutex<Option<InputSink>>,
}

fn read_broker(mut rx: File, st: Arc<FromBroker>) {
    let mut buf = Vec::new();
    while let Ok(kind) = read_message(&mut rx, &mut buf) {
        match ToHelper::decode(kind, &buf) {
            Some(ToHelper::Start { hello, peer }) => {
                *lock(&st.start) = Some((hello, peer));
                st.gen.fetch_add(1, Ordering::SeqCst);
            }
            Some(ToHelper::Stop) => st.stopped.store(true, Ordering::SeqCst),
            Some(ToHelper::Keyframe) => st.keyframe.store(true, Ordering::Relaxed),
            Some(ToHelper::SetMode { mode, allow_444 }) => {
                *lock(&st.mode) = Some((mode, allow_444))
            }
            Some(ToHelper::InputSeq(s)) => st.input_seq.store(s, Ordering::Relaxed),
            Some(ToHelper::Input(a)) => {
                if let Some(sink) = lock(&st.sink).as_mut() {
                    sink(a);
                }
            }
            None => {}
        }
    }
    st.gone.store(true, Ordering::SeqCst);
}

/// The broker, standing in for the viewer (`ViewerLink`).
struct PipeViewer {
    st: Arc<FromBroker>,
    tx: File,
    buf: Vec<u8>,
    hello: ViewerHello,
    peer: String,
    gen: u32,
}

impl PipeViewer {
    fn send(&mut self, m: ToBroker) -> std::io::Result<()> {
        m.encode(&mut self.buf);
        write_message(&mut self.tx, &self.buf)
    }
}

impl ViewerLink for PipeViewer {
    fn viewer(&self) -> ViewerHello {
        self.hello
    }
    fn connected(&self) -> bool {
        !self.st.gone.load(Ordering::SeqCst)
            && !self.st.stopped.load(Ordering::SeqCst)
            && self.st.gen.load(Ordering::SeqCst) == self.gen
    }
    fn streaming(&self) -> bool {
        true // the broker holds frames the viewer isn't ready for
    }
    fn take_keyframe_request(&self) -> bool {
        self.st.keyframe.swap(false, Ordering::Relaxed)
    }
    fn take_mode_request(&self) -> Option<(u8, bool)> {
        lock(&self.st.mode).take()
    }
    fn send_stream_change(&mut self, info: StreamInfo) -> std::io::Result<()> {
        self.send(ToBroker::StreamChange(info))
    }
    fn input_seq(&self) -> u32 {
        self.st.input_seq.load(Ordering::Relaxed)
    }
    fn send_frame(&mut self, f: &OutFrame, data: &[u8]) -> std::io::Result<bool> {
        self.send(ToBroker::Frame {
            key: f.flags & FLAG_IDR != 0,
            capture_us: f.capture_ts_us,
            encode_us: f.encode_us,
            input_seq: f.input_seq,
            data,
        })?;
        Ok(true)
    }
    fn send_cursor(&mut self, s: &CursorShape) -> std::io::Result<()> {
        self.send(ToBroker::Cursor(*s))
    }
    fn send_cursor_visible(&mut self, visible: bool) -> std::io::Result<()> {
        self.send(ToBroker::CursorVisible(visible))
    }
    fn send_notice(&mut self, code: u8) -> std::io::Result<()> {
        self.send(ToBroker::Notice(code))
    }
    fn peer(&self) -> String {
        self.peer.clone()
    }
    fn counters(&self) -> Option<glass_core::net::HostCounters> {
        None // the broker has them
    }
    fn clip_session(&self) -> Option<&HostSession> {
        None // the clipboard helper does it, as the user
    }
    fn announce_stream(&self) -> bool {
        true
    }
}

/// `glass-cli agent [--pipes NAME]`: the screen helper.
pub fn screen(args: &[String]) -> Res<()> {
    let a = parse(args)?;
    d3d::init_process();
    let monitors = capture::monitors()?;
    let mon = monitors
        .get(a.monitor)
        .ok_or_else(|| format!("no monitor {}", a.monitor))?
        .clone();
    let gpu = d3d::Gpu::new_nvidia()?;
    let link = pipe::connect(&format!("{}-screen", a.pipes), 60_000)?;
    let st = Arc::new(FromBroker::default());
    let (rx, s2) = (link.rx, st.clone());
    std::thread::Builder::new()
        .name("glass-broker-in".into())
        .spawn(move || read_broker(rx, s2))?;
    let mut out = PipeViewer {
        st: st.clone(),
        tx: link.tx,
        buf: Vec::with_capacity(1 << 20),
        hello: ViewerHello::default(),
        peer: String::new(),
        gen: 0,
    };
    let never = std::sync::atomic::AtomicBool::new(false);
    loop {
        // What's on screen now, for the broker's Welcome.
        let mut cap = capture::Capture::start(&gpu, &mon, a.desktops)?;
        let (w, h) = cap.size();
        out.send(ToBroker::Display {
            width: w as u16,
            height: h as u16,
            fps: mon.refresh_hz as u16,
        })?;
        // Wait for a viewer.
        let (hello, peer) = loop {
            if st.gone.load(Ordering::SeqCst) {
                return Ok(()); // the service is gone (or restarting us)
            }
            if let Some(s) = lock(&st.start).take() {
                break s;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        st.stopped.store(false, Ordering::SeqCst);
        out.hello = hello;
        out.peer = peer;
        out.gen = st.gen.load(Ordering::SeqCst);
        let blocked = Arc::new(AtomicU64::new(0));
        let blocking = Arc::new(AtomicU8::new(0));
        let remap = Arc::new(Mutex::new(None));
        *lock(&st.sink) = Some(input_sink(
            &mon,
            a.desktops,
            blocked.clone(),
            blocking.clone(),
            remap.clone(),
        ));
        let body = media_session(
            &gpu,
            &mon,
            &a,
            &mut cap,
            &mut out,
            &remap,
            &blocking,
            &blocked,
            &never,
            &never,
            &|l| println!("{l}"),
        );
        *lock(&st.sink) = None;
        match body {
            Ok(report) => {
                let _ = out.send(ToBroker::Report(&report));
            }
            Err(e) => {
                let _ = out.send(ToBroker::Report(&format!("  screen helper stopped: {e}\n")));
                return Err(e);
            }
        }
    }
}

/// `glass-cli clip-agent [--pipes NAME]`: the clipboard helper, as the
/// signed-in user.
pub fn clipboard(args: &[String]) -> Res<()> {
    let a = parse(args)?;
    let link = pipe::connect(&format!("{}-clip", a.pipes), 60_000)?;
    let tx = Arc::new(Mutex::new(link.tx));
    let t2 = tx.clone();
    let writer: glass_core::clipboard::FrameWriter = Box::new(move |f: &[u8]| {
        let mut out = Vec::with_capacity(f.len() + 8);
        ToBroker::ClipFrame(f).encode(&mut out);
        write_message(&mut *lock(&t2), &out).is_ok()
    });
    let clip_link = Arc::new(glass_core::clipboard::ClipLink::new(Some(writer)));
    clip_link.set_file_dir(Some(clip::file_dir(false, "host")));
    let _sync = clip::ClipSync::start(clip_link.clone(), false);
    // Its results line, for the broker's report.
    let (l2, t3) = (clip_link.clone(), tx.clone());
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let line = clip::summary(&l2.progress(), "viewer");
        let mut out = Vec::new();
        ToBroker::ClipSummary(&line).encode(&mut out);
        if write_message(&mut *lock(&t3), &out).is_err() {
            return;
        }
    });
    let mut rx = link.rx;
    let mut buf = Vec::new();
    while let Ok(kind) = read_message(&mut rx, &mut buf) {
        if kind == CLIP_TO_HELPER {
            if let Ok((m, _)) = glass_core::protocol::Message::decode(&buf) {
                clip_link.on_message(&m);
            }
        }
    }
    Ok(()) // the service is gone
}

//! Streams a `.glsr` recording as if it were live: the same host session the
//! Windows capture path uses, fed from a file. Used by CI (no GPU or screen on
//! the runners), the Swift tests, and `glass serve`.
//!
//! Frames are paced by their recorded capture timestamps and re-stamped with
//! the current clock, so the viewer's latency numbers measure the network and
//! client only. A keyframe request restarts from the first frame (an IDR).
//! Input from the viewer is logged (not injected) and a test pointer shape is
//! sent, so the input and cursor paths can be checked without a desktop.

use crate::clock;
use crate::input::{InputAction, InputCounters};
use crate::net::{HostListener, HostSession};
use crate::packet::OutFrame;
use crate::protocol::{CursorShape, StreamInfo};
use crate::recording::{RecordingReader, CODEC_HEVC};
use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Most recent input actions kept for inspection.
const INPUT_LOG_MAX: usize = 10_000;

#[derive(Default)]
struct ReplayInput {
    log: Vec<InputAction>,
    counters: InputCounters,
}

pub struct Replay {
    stop: Arc<AtomicBool>,
    port: u16,
    input: Arc<Mutex<ReplayInput>>,
    thread: Option<JoinHandle<()>>,
}

impl Replay {
    /// Starts serving `data` (a whole `.glsr` file) on `port` (0 = any free
    /// port). Serves one viewer at a time, looping the recording, until dropped.
    pub fn start(data: Vec<u8>, port: u16) -> io::Result<Self> {
        let reader = RecordingReader::new(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
        let h = reader.header;
        if h.codec != CODEC_HEVC || reader.count() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an HEVC recording with frames",
            ));
        }
        let info = StreamInfo {
            codec: h.codec,
            chroma: h.chroma,
            width: h.width as u16,
            height: h.height as u16,
            fps: h.fps as u16,
        };
        // Loopback only when testing on a random port; LAN otherwise.
        let listener = if port == 0 {
            HostListener::bind_on(std::net::Ipv4Addr::LOCALHOST, 0)?
        } else {
            HostListener::bind(port)?
        };
        let port = listener.port();
        let stop = Arc::new(AtomicBool::new(false));
        let input = Arc::new(Mutex::new(ReplayInput::default()));
        let (st, inp) = (stop.clone(), input.clone());
        let thread = std::thread::spawn(move || {
            while !st.load(Ordering::Relaxed) {
                let log = inp.clone();
                let sink = Box::new(move |a: InputAction| {
                    let mut l = log.lock().unwrap_or_else(|e| e.into_inner());
                    if l.log.len() < INPUT_LOG_MAX {
                        l.log.push(a);
                    }
                });
                let Ok(mut session) = listener.accept(info, sink) else {
                    continue;
                };
                if st.load(Ordering::Relaxed) {
                    break;
                }
                serve(&data, &mut session, &st, &inp);
                drop(session); // joins its threads, so release-all actions are logged
            }
        });
        Ok(Self {
            stop,
            port,
            input,
            thread: Some(thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Input actions received so far (all sessions, up to 10 000).
    pub fn input_log(&self) -> Vec<InputAction> {
        self.input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .log
            .clone()
    }

    /// Input counters of the current (or last) session.
    pub fn input_counters(&self) -> InputCounters {
        self.input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counters
    }
}

/// 16x16 test pointer: white square with a black border, hotspot top-left.
fn test_cursor() -> Vec<u8> {
    let mut px = Vec::with_capacity(16 * 16 * 4);
    for y in 0..16 {
        for x in 0..16 {
            let edge = x == 0 || y == 0 || x == 15 || y == 15;
            let v = if edge { 0 } else { 255 };
            px.extend_from_slice(&[v, v, v, 255]);
        }
    }
    px
}

fn serve(data: &[u8], session: &mut HostSession, stop: &AtomicBool, input: &Mutex<ReplayInput>) {
    let mut frame_id = 0u32;
    let live = |s: &HostSession| s.connected() && !stop.load(Ordering::Relaxed);
    // Wait for the viewer's UDP punch so it gets the stream from the IDR.
    while live(session) && !session.streaming() {
        std::thread::sleep(Duration::from_millis(2));
    }
    let px = test_cursor();
    let _ = session.send_cursor(&CursorShape {
        serial: 1,
        width: 16,
        height: 16,
        hot_x: 0,
        hot_y: 0,
        rgba: &px,
    });
    let _ = session.send_cursor_visible(true);
    'outer: while live(session) {
        session.take_keyframe_request();
        let Ok(reader) = RecordingReader::new(data) else {
            return;
        };
        let start = clock::now_us();
        let mut base_ts = None;
        for f in reader {
            let Ok((meta, bytes)) = f else { return };
            let base = *base_ts.get_or_insert(meta.capture_ts_us);
            let due = start + meta.capture_ts_us.saturating_sub(base);
            loop {
                if !live(session) {
                    break 'outer;
                }
                let now = clock::now_us();
                if now >= due {
                    break;
                }
                std::thread::sleep(Duration::from_micros((due - now).min(2_000)));
            }
            let out = OutFrame {
                frame_id,
                flags: meta.flags,
                capture_ts_us: clock::now_us(),
                encode_us: meta.encode_us,
                input_seq: session.input_seq(),
            };
            if session.send_frame(&out, bytes).is_err() {
                break 'outer;
            }
            input.lock().unwrap_or_else(|e| e.into_inner()).counters = session.counters().input;
            frame_id = frame_id.wrapping_add(1);
            if session.take_keyframe_request() {
                continue 'outer; // restart from the IDR at the top
            }
        }
        // Pause between loops, like an idle screen.
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock accept().
        let _ = TcpStream::connect_timeout(
            &([127, 0, 0, 1], self.port).into(),
            Duration::from_millis(500),
        );
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{Client, Wait};
    use crate::protocol::FLAG_IDR;
    use crate::recording::{FrameMeta, RecordingHeader, RecordingWriter, CHROMA_444};

    /// Synthetic recording: frame 0 IDR, sizes crossing many packets.
    fn recording(frames: u32) -> (Vec<u8>, Vec<Vec<u8>>) {
        let h = RecordingHeader {
            codec: CODEC_HEVC,
            chroma: CHROMA_444,
            width: 320,
            height: 192,
            fps: 120,
        };
        let mut w = RecordingWriter::new(Vec::new(), &h).unwrap();
        let mut datas = Vec::new();
        for id in 0..frames {
            let len = if id == 0 {
                150_000
            } else {
                200 + id as usize * 3_001
            };
            let mut d = vec![0, 0, 0, 1, 0x26, 0x01];
            d.extend((0..len).map(|i| (i as u32 ^ id.wrapping_mul(2_654_435_761)) as u8));
            let meta = FrameMeta {
                frame_id: id,
                flags: if id == 0 { FLAG_IDR } else { 0 },
                capture_ts_us: 1_000_000 + id as u64 * 8_333,
                encode_us: 2_000 + id,
                len: d.len() as u32,
            };
            w.write_frame(&meta, &d).unwrap();
            datas.push(d);
        }
        (w.into_inner(), datas)
    }

    fn take(c: &Client, n: usize) -> Vec<(crate::net::LiveFrameMeta, Vec<u8>)> {
        let mut got = Vec::new();
        let deadline = clock::now_us() + 10_000_000;
        while got.len() < n && clock::now_us() < deadline {
            match c.wait_frame(Duration::from_millis(200)) {
                Wait::Frame(m, d) => {
                    got.push((m, d.to_vec()));
                    c.release_frame();
                }
                Wait::Timeout => {}
                Wait::Disconnected => panic!("disconnected"),
            }
        }
        got
    }

    #[test]
    fn loopback_stream_is_bit_exact() {
        let (file, datas) = recording(30);
        let replay = Replay::start(file, 0).unwrap();
        let c = Client::connect("127.0.0.1", replay.port()).unwrap();
        assert_eq!(c.info().chroma, CHROMA_444);
        assert_eq!((c.info().width, c.info().height), (320, 192));
        let got = take(&c, datas.len());
        assert_eq!(got.len(), datas.len(), "stats: {:?}", c.stats());
        for (i, (m, d)) in got.iter().enumerate() {
            assert_eq!(m.frame_id, i as u32);
            assert_eq!(m.encode_us, 2_000 + i as u32);
            assert_eq!(d, &datas[i], "frame {i}");
            assert!(m.last_packet_us >= m.first_packet_us);
            assert!(m.queued_us >= m.last_packet_us);
        }
        assert!(got[0].0.flags & FLAG_IDR != 0);
        let s = c.stats();
        assert_eq!(s.rx.frames_lost, 0);
        assert!(s.connected);

        // Clock sync converges on loopback (same clock both ends).
        let deadline = clock::now_us() + 2_000_000;
        while !c.stats().clock_valid && clock::now_us() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let s = c.stats();
        assert!(s.clock_valid);
        assert!(
            s.clock_offset_us.abs() < 5_000,
            "offset {}",
            s.clock_offset_us
        );
        let local = c.host_to_local(got[5].0.capture_ts_us).unwrap();
        assert!(local <= got[5].0.last_packet_us + 5_000);
    }

    #[test]
    fn keyframe_request_restarts_from_idr() {
        let (file, datas) = recording(240); // 2 s at 120 fps
        let replay = Replay::start(file, 0).unwrap();
        let c = Client::connect("127.0.0.1", replay.port()).unwrap();
        let first = take(&c, 5);
        assert_eq!(first.len(), 5);
        c.request_keyframe();
        // Within the next frames, an IDR with frame 0's content must show up.
        let more = take(&c, 60);
        let idr = more
            .iter()
            .find(|(m, _)| m.flags & FLAG_IDR != 0)
            .expect("no IDR after request");
        assert_eq!(idr.1, datas[0]);
        assert!(c.stats().keyframe_requests >= 1);
    }

    #[test]
    fn second_viewer_after_first_leaves() {
        let (file, datas) = recording(10);
        let replay = Replay::start(file, 0).unwrap();
        for _ in 0..2 {
            let c = Client::connect("127.0.0.1", replay.port()).unwrap();
            let got = take(&c, 3);
            assert_eq!(got.len(), 3);
            assert_eq!(got[0].1, datas[0]);
        }
    }

    #[test]
    fn input_and_cursor_round_trip() {
        use crate::input::{InputAction as A, BUTTON_LEFT};
        let (file, _) = recording(240);
        let replay = Replay::start(file, 0).unwrap();
        let c = Client::connect("127.0.0.1", replay.port()).unwrap();
        assert_eq!(take(&c, 2).len(), 2); // streaming
        c.input_move(100, 200);
        c.input_key(0xE0, true); // LCtrl
        c.input_key(0x06, true); // C
        c.input_key(0x06, false);
        c.input_button(BUTTON_LEFT, true, 300, 400);
        c.input_button(BUTTON_LEFT, false, 300, 400);
        c.input_scroll(120, -40);
        c.input_key(0x04, true); // A, still held when the viewer leaves
                                 // Pointer shape arrives over TCP.
        let mut rgba = Vec::new();
        let deadline = clock::now_us() + 3_000_000;
        let mut info = c.cursor(0, &mut rgba);
        while info.width == 0 && clock::now_us() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            info = c.cursor(0, &mut rgba);
        }
        assert_eq!((info.width, info.height, info.visible), (16, 16, true));
        assert_eq!(rgba.len(), 16 * 16 * 4);
        assert_eq!(&rgba[0..4], &[0, 0, 0, 255]);
        // Frames carry the applied input sequence.
        let mut seq = 0;
        for (m, _) in take(&c, 40) {
            seq = seq.max(m.input_seq);
        }
        assert!(seq > 0, "no input_seq on frames");
        assert!(c.input_sent_at(seq).is_some());
        assert!(c.stats().input_events >= 7);
        drop(c);
        // The session ends: the held A and Ctrl are released.
        let deadline = clock::now_us() + 3_000_000;
        while !replay.input_log().contains(&A::Key {
            hid: 0xE0,
            down: false,
        }) && clock::now_us() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let log = replay.input_log();
        let want = [
            A::MoveTo { x: 100, y: 200 },
            A::Key {
                hid: 0xE0,
                down: true,
            },
            A::Key {
                hid: 0x06,
                down: true,
            },
            A::Key {
                hid: 0x06,
                down: false,
            },
            A::MoveTo { x: 300, y: 400 },
            A::Button {
                button: BUTTON_LEFT,
                down: true,
            },
            A::Button {
                button: BUTTON_LEFT,
                down: false,
            },
            A::Scroll { dy: 120, dx: -40 },
            A::Key {
                hid: 0x04,
                down: true,
            },
            A::Key {
                hid: 0x04,
                down: false,
            },
            A::Key {
                hid: 0xE0,
                down: false,
            },
        ];
        assert_eq!(log, want);
        assert_eq!(replay.input_counters().clicks, 1);
    }

    #[test]
    fn connect_refused_is_reported() {
        // Bind then drop to find a port nobody listens on.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert_eq!(
            Client::connect("127.0.0.1", port).err(),
            Some(crate::net::ConnectError::Connect)
        );
    }
}

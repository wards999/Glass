//! `losssim`: runs a stream of frames through simulated Wi-Fi loss (M4,
//! ADR 0020), using Glass's real packetiser, FEC and reassembler, and reports
//! what survived.
//!
//! Loss models:
//! - random: each packet lost with probability `p`;
//! - burst (Gilbert–Elliott): a good and a bad state, with loss in each and a
//!   chance per packet of switching, so losses come in runs like they do on
//!   Wi-Fi.
//!
//! Packets are also reordered a little (a packet may swap with the next one),
//! as Wi-Fi aggregation does. The FEC follows the loss the receiver reports,
//! exactly as the host does (up fast, down slowly), or protects against a
//! fixed loss with `--assume P`.
//!
//! Each scenario runs twice: FEC alone, and FEC with NACKs (M4 step 2). The
//! NACK run keeps time: packets take [`ONE_WAY_US`] each way, so a NACK and
//! its resends cost a round trip, and NACKs and resends can be lost too.
//! It reports how much later than "on time" the frames were handed out.
//!
//! `losssim` prints a table of scenarios. `losssim --check` exits non-zero
//! unless 5% random loss loses no frame (CLAUDE.md), with FEC alone and
//! with NACKs.

use glass_core::packet::{OutFrame, Packetiser, Reassembler, MAX_PAYLOAD};
use glass_core::protocol::FLAG_IDR;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// One-way network time (µs): a 4 ms round trip, typical Wi-Fi.
const ONE_WAY_US: u64 = 2_000;
/// Gap between a frame's packets on the wire (µs).
const PACKET_GAP_US: u64 = 20;
const FRAME_US: u64 = 16_667;

/// A small fast PRNG (xorshift), seeded so runs repeat.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Uniform in [0, 1).
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Clone, Copy)]
enum Loss {
    Random(f64),
    /// Gilbert–Elliott: (p good->bad, p bad->good, loss in good, loss in bad).
    Burst(f64, f64, f64, f64),
}

impl Loss {
    fn name(self) -> String {
        match self {
            Loss::Random(p) => format!("random {:.0}%", p * 100.0),
            Loss::Burst(g, b, lg, lb) => {
                let bad = g / (g + b);
                let avg = (1.0 - bad) * lg + bad * lb;
                format!("bursts {:.1}% (runs ~{:.0})", avg * 100.0, 1.0 / b)
            }
        }
    }
}

struct Outcome {
    frames: u32,
    lost: u32,
    nacks: u64,
    resent: u64,
    /// How much later than on time frames were handed out (µs): p99, max.
    late_p99: u64,
    late_max: u64,
    recovered: u64,
    data_missing: u64,
    data_expected: u64,
    parity: u64,
    data: u64,
}

enum Event {
    /// A datagram reaches the viewer.
    Packet(Vec<u8>),
    /// A NACK reaches the host.
    Nack(u32, Vec<u16>),
}

/// One run: `frames` frames at 60 fps (a keyframe every 2 s, the rest small
/// P-frames of varying size), with NACKs or FEC alone.
fn run(loss: Loss, frames: u32, fixed_fec: Option<f32>, seed: u64, nack: bool) -> Outcome {
    let mut rng = Rng(seed);
    let mut tx = Packetiser::default();
    let mut rx = if nack {
        Reassembler::new()
    } else {
        Reassembler::without_nack()
    };
    rx.set_rtt(2 * ONE_WAY_US as u32);
    let mut bad_state = false;
    let mut lost_now = |rng: &mut Rng| -> bool {
        match loss {
            Loss::Random(p) => rng.f() < p,
            Loss::Burst(g, b, lg, lb) => {
                bad_state = if bad_state { rng.f() >= b } else { rng.f() < g };
                rng.f() < if bad_state { lb } else { lg }
            }
        }
    };
    let (mut loss_ewma, mut last) = (0.0f32, (0u64, 0u64));
    let (mut parity, mut data) = (0u64, 0u64);
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    // When each frame was "on time": its last packet sent plus one way.
    let mut due: Vec<u64> = Vec::new();
    let mut late: Vec<u64> = Vec::new();
    let mut events: BinaryHeap<Reverse<(u64, u64)>> = BinaryHeap::new();
    let mut bodies: std::collections::HashMap<u64, Event> = Default::default();
    let mut seq = 0u64;
    let mut push_event = |events: &mut BinaryHeap<Reverse<(u64, u64)>>,
                          bodies: &mut std::collections::HashMap<u64, Event>,
                          t: u64,
                          e: Event| {
        seq += 1;
        events.push(Reverse((t, seq)));
        bodies.insert(seq, e);
    };
    for id in 0..frames + 6 {
        let t0 = id as u64 * FRAME_US;
        // Everything up to this frame's send time, the viewer waking every
        // 2 ms while a frame is incomplete.
        let mut wake = None::<u64>;
        loop {
            let next_ev = events.peek().map(|Reverse((t, _))| *t);
            let t = match (next_ev, wake) {
                (Some(e), Some(w)) => e.min(w),
                (Some(e), None) => e,
                (None, Some(w)) => w,
                (None, None) => break,
            };
            if t >= t0 {
                break;
            }
            if next_ev == Some(t) {
                let Reverse((_, k)) = events.pop().unwrap();
                match bodies.remove(&k).unwrap() {
                    Event::Packet(p) => rx.push(&p, t),
                    Event::Nack(fid, ix) => {
                        let mut out = Vec::new();
                        tx.resend(fid, ix.into_iter(), t, |p| {
                            out.push(p.to_vec());
                            Ok(())
                        })
                        .unwrap();
                        for (j, p) in out.into_iter().enumerate() {
                            if !lost_now(&mut rng) {
                                let at = t + ONE_WAY_US + j as u64 * PACKET_GAP_US;
                                push_event(&mut events, &mut bodies, at, Event::Packet(p));
                            }
                        }
                        continue;
                    }
                }
            }
            let mut asked = Vec::new();
            rx.poll(t, |fid, ix| asked.push((fid, ix.to_vec())));
            for (fid, ix) in asked {
                if !lost_now(&mut rng) {
                    push_event(
                        &mut events,
                        &mut bodies,
                        t + ONE_WAY_US,
                        Event::Nack(fid, ix),
                    );
                }
            }
            while let Some(got) = rx.pop(t) {
                let i = got.frame_id as usize;
                assert_eq!(got.data, &payloads[i][..], "frame {i} came back wrong");
                late.push(t.saturating_sub(due[i]));
            }
            wake = rx.pending().then_some(t + 2_000);
        }
        if id >= frames {
            continue; // let the last frames finish
        }
        // Every 15 frames (0.25 s), the receiver's report, as the host sees it.
        if id % 15 == 0 {
            let (e, m) = (
                rx.stats.data_expected.saturating_sub(last.0),
                rx.stats.data_missing.saturating_sub(last.1),
            );
            last = (rx.stats.data_expected, rx.stats.data_missing);
            if e > 0 {
                let now = m as f32 / e as f32;
                let w = if now > loss_ewma { 0.5 } else { 0.1 };
                loss_ewma += (now - loss_ewma) * w;
            }
        }
        tx.fec_loss = fixed_fec.unwrap_or(loss_ewma);
        let idr = id % 120 == 0;
        let packets = if idr {
            150 + (rng.next() % 80) as usize
        } else {
            2 + (rng.next() % 30) as usize
        };
        let len = packets * MAX_PAYLOAD - 20;
        let payload: Vec<u8> = (0..len)
            .map(|i| (i as u32).wrapping_mul(2_654_435_761).wrapping_add(id) as u8)
            .collect();
        let f = OutFrame {
            frame_id: id,
            flags: if idr { FLAG_IDR } else { 0 },
            capture_ts_us: t0,
            encode_us: 5_000,
            input_seq: 0,
        };
        let mut wire: Vec<Vec<u8>> = Vec::new();
        let n = tx
            .send_frame(&f, &payload, t0, |p| {
                wire.push(p.to_vec());
                Ok(())
            })
            .expect("packetise");
        payloads.push(payload);
        data += n as u64;
        parity += wire.len() as u64 - n as u64;
        // A little reordering, as Wi-Fi aggregation does.
        for i in 0..wire.len().saturating_sub(1) {
            if rng.f() < 0.05 {
                wire.swap(i, i + 1);
            }
        }
        due.push(t0 + ONE_WAY_US + wire.len() as u64 * PACKET_GAP_US);
        for (j, p) in wire.into_iter().enumerate() {
            if lost_now(&mut rng) {
                continue;
            }
            let at = t0 + ONE_WAY_US + j as u64 * PACKET_GAP_US;
            push_event(&mut events, &mut bodies, at, Event::Packet(p));
        }
    }
    late.sort_unstable();
    let p99 = late.get(late.len() * 99 / 100).copied().unwrap_or(0);
    Outcome {
        frames,
        lost: rx.stats.frames_lost as u32,
        nacks: rx.stats.nacks,
        resent: rx.stats.resent,
        late_p99: p99,
        late_max: late.last().copied().unwrap_or(0),
        recovered: rx.stats.recovered,
        data_missing: rx.stats.data_missing,
        data_expected: rx.stats.data_expected,
        parity,
        data,
    }
}

/// Audio through the same loss: 10 ms frames, NACKs and resends lost too,
/// a device pulling 5 ms at a time. (frames, concealed, underruns, target).
fn run_audio(loss: Loss, secs: u64, seed: u64) -> (u64, u64, u64, u32) {
    use glass_core::audio::{AudioReceiver, AudioSender, FRAME_LEN};
    let mut rng = Rng(seed);
    let mut bad_state = false;
    let mut lost_now = |rng: &mut Rng| -> bool {
        match loss {
            Loss::Random(p) => rng.f() < p,
            Loss::Burst(g, b, lg, lb) => {
                bad_state = if bad_state { rng.f() >= b } else { rng.f() < g };
                rng.f() < if bad_state { lb } else { lg }
            }
        }
    };
    let mut tx = AudioSender::new().expect("opus");
    let mut rx = AudioReceiver::new().expect("opus");
    rx.set_rtt(2 * ONE_WAY_US as u32);
    let ring = rx.ring();
    let pcm: Vec<f32> = (0..FRAME_LEN)
        .map(|i| ((i / 2) as f32 * 0.06).sin() * 0.3)
        .collect();
    let mut pull = [0f32; FRAME_LEN / 2];
    // (arrival ms, datagram) to the viewer; (arrival ms, first, count) to the host.
    let mut to_viewer: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut to_host: Vec<(u64, u32, u8)> = Vec::new();
    let one_way_ms = ONE_WAY_US / 1_000;
    for ms in 0..secs * 1_000 {
        let now = ms * 1_000;
        if ms % 10 == 0 {
            let d = tx.frame(&pcm, now + 1).expect("encode").to_vec();
            if !lost_now(&mut rng) {
                to_viewer.push((ms + one_way_ms, d));
            }
        }
        let mut i = 0;
        while i < to_host.len() {
            if to_host[i].0 == ms {
                let (_, first, count) = to_host.swap_remove(i);
                for q in first..first + count as u32 {
                    if let Some(d) = tx.resend(q) {
                        let d = d.to_vec();
                        if !lost_now(&mut rng) {
                            to_viewer.push((ms + one_way_ms, d));
                        }
                    }
                }
            } else {
                i += 1;
            }
        }
        let mut i = 0;
        while i < to_viewer.len() {
            if to_viewer[i].0 == ms {
                let (_, d) = to_viewer.swap_remove(i);
                rx.push(&d, now);
            } else {
                i += 1;
            }
        }
        let mut asked = Vec::new();
        rx.tick(now, |first, count| asked.push((first, count)));
        for (first, count) in asked {
            if !lost_now(&mut rng) {
                to_host.push((ms + one_way_ms, first, count));
            }
        }
        if ms > 50 && ms % 5 == 0 {
            ring.read(&mut pull, 1.0);
        }
    }
    let s = rx.stats;
    if std::env::var("AUDIO_DEBUG").is_ok() {
        eprintln!("{s:?}");
    }
    (
        s.frames + s.concealed,
        s.concealed,
        s.underruns,
        s.target_ms,
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
    };
    let fixed = value("--assume").and_then(|v| v.parse::<f32>().ok());
    let frames = value("--frames")
        .and_then(|v| v.parse().ok())
        .unwrap_or(6_000); // 100 s at 60 fps
    let scenarios = [
        Loss::Random(0.0),
        Loss::Random(0.01),
        Loss::Random(0.02),
        Loss::Random(0.05),
        Loss::Random(0.10),
        // Mostly clean, with runs of heavy loss (Wi-Fi interference).
        Loss::Burst(0.005, 0.25, 0.001, 0.5),
        Loss::Burst(0.01, 0.15, 0.002, 0.6),
    ];
    println!(
        "losssim: Glass protocol v{}, {frames} frames at 60 fps, FEC {}",
        glass_core::protocol::PROTOCOL_VERSION,
        fixed.map_or("following the reported loss".to_string(), |r| format!(
            "for a fixed {:.0}% loss",
            r * 100.0
        ))
    );
    println!(
        "{:<26} {:>7} {:>9} {:>9} | {:>9} {:>7} {:>8} {:>16}",
        "loss", "seen", "FEC extra", "lost", "lost", "NACKs", "resent", "late p99 / max"
    );
    println!(
        "{:<26} {:>7} {:>9} {:>9} | {:>9}",
        "", "", "", "FEC only", "with NACK"
    );
    let mut failed = false;
    for (i, &loss) in scenarios.iter().enumerate() {
        let seed = 0x9E37_79B9_7F4A_7C15 ^ i as u64;
        let o = run(loss, frames, fixed, seed, false);
        let n = run(loss, frames, fixed, seed, true);
        let pct = |l: u32| l as f64 * 100.0 / frames as f64;
        println!(
            "{:<26} {:>6.2}% {:>8.1}% {:>4} {:>3.2}% | {:>4} {:>3.2}% {:>7} {:>8} {:>6.1} / {:>4.1} ms",
            loss.name(),
            o.data_missing as f64 * 100.0 / o.data_expected.max(1) as f64,
            o.parity as f64 * 100.0 / o.data as f64,
            o.lost,
            pct(o.lost),
            n.lost,
            pct(n.lost),
            n.nacks,
            n.resent,
            n.late_p99 as f64 / 1e3,
            n.late_max as f64 / 1e3,
        );
        let _ = (
            o.recovered,
            o.frames,
            o.late_p99,
            o.late_max,
            o.nacks,
            o.resent,
        );
        if matches!(loss, Loss::Random(p) if p == 0.05) && (o.lost > 0 || n.lost > 0) {
            failed = true;
        }
    }
    println!();
    println!("Audio (M4b): Opus 10 ms frames, 3 redundant copies, NACK, a 4 ms round trip");
    println!(
        "{:<26} {:>9} {:>11} {:>10} {:>12}",
        "loss", "frames", "concealed", "gaps", "buffer (ms)"
    );
    let secs = (frames as u64 / 60).max(10);
    for (i, &loss) in scenarios.iter().enumerate() {
        let (n, concealed, gaps, target) = run_audio(loss, secs, 0x51ED_2701 ^ i as u64);
        println!(
            "{:<26} {:>9} {:>5} ({:>4.2}%) {:>10} {:>12}",
            loss.name(),
            n,
            concealed,
            concealed as f64 * 100.0 / n.max(1) as f64,
            gaps,
            target
        );
        // The user's rule, never broken or choppy: at 5 % loss no silent
        // gap, and concealment (single frames, smoothed by Opus) under
        // 0.1 %. The buffer is kept small for sync with the picture.
        if matches!(loss, Loss::Random(p) if p == 0.05) && (gaps > 0 || concealed * 1_000 > n) {
            failed = true;
        }
    }
    if check {
        if failed {
            eprintln!("FAIL: video frames lost or audio broken at 5% random loss");
            std::process::exit(1);
        }
        println!("ok: no video frame lost, and no audio gap, at 5% random loss");
    }
}

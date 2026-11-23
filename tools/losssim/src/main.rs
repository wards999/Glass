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
//! `losssim` prints a table of scenarios. `losssim --check` exits non-zero
//! unless 5% random loss loses no frame (CLAUDE.md).

use glass_core::packet::{OutFrame, Packetiser, Reassembler, MAX_PAYLOAD};
use glass_core::protocol::FLAG_IDR;

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
    recovered: u64,
    data_missing: u64,
    data_expected: u64,
    parity: u64,
    data: u64,
}

/// One run: `frames` frames at 60 fps (a keyframe every 2 s, the rest small
/// P-frames of varying size).
fn run(loss: Loss, frames: u32, fixed_fec: Option<f32>, seed: u64) -> Outcome {
    let mut rng = Rng(seed);
    let mut tx = Packetiser::default();
    let mut rx = Reassembler::new();
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
    let mut payload = vec![0u8; 400 * MAX_PAYLOAD];
    for id in 0..frames {
        // Every 15 frames (0.25 s), the receiver's report, as the host sees it.
        if id % 15 == 0 {
            let (e, m) = (
                rx.stats.data_expected - last.0,
                rx.stats.data_missing - last.1,
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
        for (i, b) in payload[..len].iter_mut().enumerate() {
            *b = (i as u32).wrapping_mul(2_654_435_761).wrapping_add(id) as u8;
        }
        let f = OutFrame {
            frame_id: id,
            flags: if idr { FLAG_IDR } else { 0 },
            capture_ts_us: id as u64 * 16_667,
            encode_us: 5_000,
            input_seq: 0,
        };
        let mut wire: Vec<Vec<u8>> = Vec::new();
        let n = tx
            .send_frame(&f, &payload[..len], |p| {
                wire.push(p.to_vec());
                Ok(())
            })
            .expect("packetise");
        data += n as u64;
        parity += wire.len() as u64 - n as u64;
        // A little reordering, as Wi-Fi aggregation does.
        for i in 0..wire.len().saturating_sub(1) {
            if rng.f() < 0.05 {
                wire.swap(i, i + 1);
            }
        }
        for p in wire {
            if lost_now(&mut rng) {
                continue;
            }
            if let Some(got) = rx.push(&p, id as u64 * 16_667) {
                assert_eq!(got.data, &payload[..len], "frame {id} came back wrong");
            }
        }
    }
    Outcome {
        frames,
        lost: rx.stats.frames_lost as u32,
        recovered: rx.stats.recovered,
        data_missing: rx.stats.data_missing,
        data_expected: rx.stats.data_expected,
        parity,
        data,
    }
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
        "{:<26} {:>9} {:>14} {:>10} {:>10}",
        "loss", "seen", "frames lost", "rebuilt", "FEC extra"
    );
    let mut failed = false;
    for (i, &loss) in scenarios.iter().enumerate() {
        let o = run(loss, frames, fixed, 0x9E37_79B9_7F4A_7C15 ^ i as u64);
        println!(
            "{:<26} {:>8.2}% {:>6} ({:>4.2}%) {:>10} {:>9.1}%",
            loss.name(),
            o.data_missing as f64 * 100.0 / o.data_expected.max(1) as f64,
            o.lost,
            o.lost as f64 * 100.0 / o.frames as f64,
            o.recovered,
            o.parity as f64 * 100.0 / o.data as f64,
        );
        if matches!(loss, Loss::Random(p) if p == 0.05) && o.lost > 0 {
            failed = true;
        }
    }
    if check {
        if failed {
            eprintln!("FAIL: frames were lost at 5% random loss");
            std::process::exit(1);
        }
        println!("ok: no frame lost at 5% random loss");
    }
}

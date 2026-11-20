//! `glass probe` (headless viewer: receive, count, time; no decode) and
//! `glass serve` (stream a recording as if live). Core only, so they build and
//! run on any OS; CI uses them for an end-to-end loopback check.

use glass_core::clock;
use glass_core::net::{Client, Wait};
use glass_core::protocol::{DEFAULT_PORT, FLAG_IDR};
use glass_core::recording::RecordingReader;
use glass_core::replay::Replay;
use glass_core::stats::{IntervalTracker, StatRing, Summary};
use std::collections::HashSet;
use std::error::Error;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

type Res<T> = Result<T, Box<dyn Error>>;

fn line(name: &str, s: &Summary) -> String {
    format!(
        "  {name:<18} p50 {:>7.2} ms  p99 {:>7.2} ms  max {:>7.2} ms  sd {:>6.2} ms  (n={})",
        s.p50_us as f64 / 1e3,
        s.p99_us as f64 / 1e3,
        s.max_us as f64 / 1e3,
        s.stddev_us / 1e3,
        s.count
    )
}

fn hash(d: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    d.hash(&mut h);
    h.finish()
}

fn port_arg(v: Option<&String>) -> Res<u16> {
    Ok(v.map(|s| s.parse()).transpose()?.unwrap_or(DEFAULT_PORT))
}

/// `glass serve FILE.glsr [--port P]`
pub fn serve(args: &[String]) -> Res<()> {
    let (mut file, mut port) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = it.next(),
            s if file.is_none() => file = Some(s.to_string()),
            s => return Err(format!("unknown argument {s}").into()),
        }
    }
    let file = file.ok_or("usage: glass serve FILE.glsr [--port P]")?;
    let replay = Replay::start(std::fs::read(&file)?, port_arg(port)?)?;
    println!(
        "{} | serving {file} on port {} (Ctrl+C to stop)",
        crate::banner(),
        replay.port()
    );
    // Report input the viewer sends (logged, not injected).
    let mut last = Default::default();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let c = replay.input_counters();
        if c != last {
            println!(
                "input: {} packets | {} keys, {} clicks, {} scrolls, {} moves | repaired {}",
                c.packets, c.keys, c.clicks, c.scrolls, c.moves, c.repaired
            );
            last = c;
        }
    }
}

/// `glass probe HOST [--port P] [--seconds S] [--verify FILE.glsr] [--min-frames N]`
pub fn probe(args: &[String]) -> Res<()> {
    let (mut host, mut port, mut seconds, mut verify, mut min_frames) =
        (None, None, 10.0f64, None, 0u64);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--port" => port = Some(val()?.clone()),
            "--seconds" => seconds = val()?.parse()?,
            "--verify" => verify = Some(val()?.clone()),
            "--min-frames" => min_frames = val()?.parse()?,
            s if host.is_none() && !s.starts_with("--") => host = Some(s.to_string()),
            s => return Err(format!("unknown argument {s}").into()),
        }
    }
    let host =
        host.ok_or("usage: glass probe HOST [--port P] [--seconds S] [--verify FILE.glsr]")?;
    let port = port_arg(port.as_ref())?;

    // Frames we expect, by content hash (replay restarts from frame 0 on
    // keyframe requests, so ids don't map to file positions).
    let expected: Option<HashSet<u64>> = match &verify {
        Some(f) => {
            let buf = std::fs::read(f)?;
            let r = RecordingReader::new(&buf).map_err(|e| format!("{f}: {e:?}"))?;
            let mut set = HashSet::new();
            for fr in r {
                set.insert(hash(fr.map_err(|e| format!("{f}: {e:?}"))?.1));
            }
            Some(set)
        }
        None => None,
    };

    let c = Client::connect(&host, port).map_err(|e| format!("{host}:{port}: {e}"))?;
    let info = c.info();
    println!(
        "{} | probe {host}:{port}: {}x{} @ {} Hz HEVC {}",
        crate::banner(),
        info.width,
        info.height,
        info.fps,
        if info.chroma == 1 { "4:4:4" } else { "4:2:0" }
    );

    let mut network = StatRing::new(); // encoded on host -> last packet here
    let mut to_rx = StatRing::new(); // capture on host -> last packet here
    let mut spread = StatRing::new(); // first -> last packet of a frame
    let mut pickup = StatRing::new(); // queued -> picked up
    let mut intervals = IntervalTracker::default();
    let (mut frames, mut idrs, mut bytes, mut mismatched) = (0u64, 0u64, 0u64, 0u64);
    let t0 = clock::now_us();
    let end = t0 + (seconds * 1e6) as u64;
    let mut gone = false;
    while clock::now_us() < end {
        match c.wait_frame(Duration::from_millis(100)) {
            Wait::Frame(m, d) => {
                let now = clock::now_us();
                frames += 1;
                bytes += d.len() as u64;
                idrs += (m.flags & FLAG_IDR != 0) as u64;
                intervals.tick(m.last_packet_us);
                spread.push((m.last_packet_us - m.first_packet_us) as u32);
                pickup.push(now.saturating_sub(m.queued_us) as u32);
                if let Some(cap) = c.host_to_local(m.capture_ts_us) {
                    let enc = cap + m.encode_us as u64;
                    network.push(m.last_packet_us.saturating_sub(enc) as u32);
                    to_rx.push(m.last_packet_us.saturating_sub(cap) as u32);
                }
                if let Some(set) = &expected {
                    if !set.contains(&hash(d)) {
                        mismatched += 1;
                    }
                }
                c.release_frame();
            }
            Wait::Timeout => {}
            Wait::Disconnected => {
                gone = true;
                break;
            }
        }
    }
    let secs = (clock::now_us() - t0) as f64 / 1e6;
    let s = c.stats();
    println!(
        "Received {frames} frames in {secs:.1} s ({:.1} fps), {idrs} IDRs, {:.1} Mbps{}",
        frames as f64 / secs,
        bytes as f64 * 8.0 / secs / 1e6,
        if gone { " (host disconnected)" } else { "" }
    );
    println!(
        "  packets {} | lost frames {} | skipped {} | dup {} | late {} | bad {} | queue full {} | keyframe requests {}",
        s.rx.packets,
        s.rx.frames_lost,
        s.rx.frames_skipped,
        s.rx.duplicates,
        s.rx.late,
        s.rx.bad,
        s.queue_full,
        s.keyframe_requests
    );
    println!(
        "  clock sync: {} offset {} µs, best RTT {:.2} ms, last RTT {:.2} ms",
        if s.clock_valid { "ok" } else { "NOT SYNCED" },
        s.clock_offset_us,
        s.clock_rtt_us as f64 / 1e3,
        s.last_rtt_us as f64 / 1e3
    );
    println!("{}", line("network", &network.summary()));
    println!("{}", line("capture->received", &to_rx.summary()));
    println!("{}", line("packet spread", &spread.summary()));
    println!("{}", line("queue pickup", &pickup.summary()));
    println!(
        "{}",
        line("arrival interval", &intervals.intervals.summary())
    );
    if let Some(set) = &expected {
        println!(
            "  verify: {} of {frames} frames match {} ({} distinct frames in file)",
            frames - mismatched,
            verify.as_deref().unwrap_or(""),
            set.len()
        );
        if mismatched > 0 {
            return Err(format!("{mismatched} frames did not match the recording").into());
        }
    }
    if frames < min_frames {
        return Err(format!("received {frames} frames, expected at least {min_frames}").into());
    }
    Ok(())
}

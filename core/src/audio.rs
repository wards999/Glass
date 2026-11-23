//! Audio (M4b, ADR 0025): the host's sound to the viewer, never broken or
//! choppy (the user's rule, ADR 0018).
//!
//! - **Opus** (libopus), 48 kHz stereo, 10 ms frames, restricted low-delay
//!   mode, 192 kbps: music quality, ~5 ms codec delay.
//! - **Redundancy:** each datagram carries the newest frame and the
//!   [`REDUNDANCY`] before it, so a lost datagram, or two in a row, costs
//!   nothing and adds no delay. Longer gaps are asked for again
//!   ([`AudioNack`]); the host keeps [`HISTORY`] frames.
//! - **Playout:** decoded audio goes into a lock-free ring ([`PcmRing`])
//!   that the device's callback reads. [`AudioReceiver::tick`] keeps the
//!   ring at a target depth (adaptive, 20–80 ms), conceals a frame that
//!   really didn't come (Opus PLC, never a click), and follows clock drift
//!   between the two machines by resampling a fraction of a percent, never
//!   by dropping or repeating audio.
//!
//! Datagram (inside the session's seal):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | `"GLAU"` |
//! | 4 | 4 | seq of the newest frame |
//! | 8 | 8 | its capture time (host clock, µs) |
//! | 16 | 1 | n frames, newest first (frame k is seq − k) |
//! | 17 | … | n × (len u16, Opus bytes) |

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one 10 ms frame.
pub const FRAME_SAMPLES: usize = 480;
/// Interleaved samples in one frame.
pub const FRAME_LEN: usize = FRAME_SAMPLES * CHANNELS;
pub const FRAME_US: u64 = 10_000;
pub const BITRATE: i32 = 192_000;
/// Earlier frames repeated in each datagram.
pub const REDUNDANCY: usize = 2;
/// Largest Opus frame kept (bytes): 10 ms at 192 kbps is ~240.
pub const MAX_OPUS: usize = 600;
/// Frames the host keeps for resends (640 ms).
pub const HISTORY: usize = 64;
pub const AUDIO_MAGIC: [u8; 4] = *b"GLAU";
const HEADER: usize = 17;
/// A whole datagram, at most.
pub const MAX_DATAGRAM: usize = HEADER + (REDUNDANCY + 1) * (2 + MAX_OPUS);

/// A received audio datagram.
#[derive(Debug, Clone, Copy)]
pub struct AudioPacket<'a> {
    pub seq: u32,
    pub capture_ts_us: u64,
    n: u8,
    body: &'a [u8],
}

pub fn is_audio(b: &[u8]) -> bool {
    b.len() >= HEADER && b[0..4] == AUDIO_MAGIC
}

impl<'a> AudioPacket<'a> {
    pub fn decode(b: &'a [u8]) -> Option<Self> {
        if !is_audio(b) {
            return None;
        }
        let p = Self {
            seq: u32::from_be_bytes(b[4..8].try_into().ok()?),
            capture_ts_us: u64::from_be_bytes(b[8..16].try_into().ok()?),
            n: b[16],
            body: &b[HEADER..],
        };
        // Every frame must fit.
        (p.frames().count() == p.n as usize).then_some(p)
    }

    /// (seq, Opus bytes), newest first.
    pub fn frames(&self) -> impl Iterator<Item = (u32, &'a [u8])> + 'a {
        let (mut rest, seq, n) = (self.body, self.seq, self.n);
        (0..n as u32).map_while(move |k| {
            let len = u16::from_be_bytes([*rest.first()?, *rest.get(1)?]) as usize;
            let f = rest.get(2..2 + len)?;
            rest = &rest[2 + len..];
            Some((seq.wrapping_sub(k), f))
        })
    }
}

/// Writes a datagram: `frames` newest first, the first being `seq`.
fn encode_packet(seq: u32, ts: u64, frames: &[&[u8]], out: &mut [u8]) -> usize {
    out[0..4].copy_from_slice(&AUDIO_MAGIC);
    out[4..8].copy_from_slice(&seq.to_be_bytes());
    out[8..16].copy_from_slice(&ts.to_be_bytes());
    out[16] = frames.len() as u8;
    let mut at = HEADER;
    for f in frames {
        out[at..at + 2].copy_from_slice(&(f.len() as u16).to_be_bytes());
        out[at + 2..at + 2 + f.len()].copy_from_slice(f);
        at += 2 + f.len();
    }
    at
}

#[derive(Clone, Copy)]
struct Kept {
    seq: u32,
    len: usize,
    data: [u8; MAX_OPUS],
}

impl Kept {
    const EMPTY: Self = Self {
        seq: u32::MAX,
        len: 0,
        data: [0; MAX_OPUS],
    };
}

/// The host's side: encodes 10 ms frames and builds datagrams. No
/// allocation after construction.
pub struct AudioSender {
    enc: opus::Encoder,
    seq: u32,
    kept: Box<[Kept; HISTORY]>,
    out: [u8; MAX_DATAGRAM],
}

impl AudioSender {
    pub fn new() -> Result<Self, String> {
        let e = |e: opus::Error| format!("Opus encoder: {e}");
        let mut enc = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )
        .map_err(e)?;
        enc.set_bitrate(opus::Bitrate::Bits(BITRATE)).map_err(e)?;
        enc.set_vbr(true).map_err(e)?;
        enc.set_vbr_constraint(true).map_err(e)?;
        enc.set_complexity(8).map_err(e)?;
        Ok(Self {
            enc,
            seq: 0,
            kept: Box::new([Kept::EMPTY; HISTORY]),
            out: [0; MAX_DATAGRAM],
        })
    }

    /// Encodes one frame ([`FRAME_LEN`] interleaved samples, −1..1) captured
    /// at `capture_ts_us`, and returns the datagram to seal and send.
    pub fn frame(&mut self, pcm: &[f32], capture_ts_us: u64) -> Result<&[u8], String> {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let slot = &mut self.kept[seq as usize % HISTORY];
        let len = self
            .enc
            .encode_float(&pcm[..FRAME_LEN], &mut slot.data)
            .map_err(|e| format!("Opus encode: {e}"))?;
        slot.seq = seq;
        slot.len = len;
        let mut frames: [&[u8]; REDUNDANCY + 1] = [&[]; REDUNDANCY + 1];
        let mut n = 0;
        for k in 0..=REDUNDANCY as u32 {
            let s = seq.wrapping_sub(k);
            let kept = &self.kept[s as usize % HISTORY];
            if kept.seq != s {
                break; // the stream's first frames
            }
            frames[n] = &kept.data[..kept.len];
            n += 1;
        }
        let len = encode_packet(seq, capture_ts_us, &frames[..n], &mut self.out);
        Ok(&self.out[..len])
    }

    /// A datagram with frame `seq` alone, if still kept (for a NACK).
    pub fn resend(&mut self, seq: u32) -> Option<&[u8]> {
        let kept = &self.kept[seq as usize % HISTORY];
        if kept.seq != seq {
            return None;
        }
        let f: &[u8] = &kept.data[..kept.len];
        let len = encode_packet(seq, 0, &[f], &mut self.out);
        Some(&self.out[..len])
    }
}

/// Decoded audio between the receiver and the device callback: one writer
/// (the receiver), one reader (the device). Lock-free; the callback never
/// waits.
pub struct PcmRing {
    buf: Box<[UnsafeCell<f32>]>,
    /// Samples written and read so far (each only grows).
    head: AtomicUsize,
    tail: AtomicUsize,
    /// The reader found too little while sound was playing.
    underruns: AtomicU64,
    /// Sound is playing (a stream is on), so running dry is a glitch.
    active: AtomicBool,
}

// SAFETY: one writer and one reader, coordinated by head/tail with
// acquire/release ordering; a sample is only read after it was published.
unsafe impl Sync for PcmRing {}
unsafe impl Send for PcmRing {}

impl PcmRing {
    pub fn new(samples: usize) -> Self {
        Self {
            buf: (0..samples).map(|_| UnsafeCell::new(0.0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            underruns: AtomicU64::new(0),
            active: AtomicBool::new(false),
        }
    }

    /// Samples waiting to be played.
    pub fn level(&self) -> usize {
        self.head.load(Ordering::Acquire) - self.tail.load(Ordering::Acquire)
    }

    fn write(&self, data: &[f32]) -> usize {
        let cap = self.buf.len();
        let head = self.head.load(Ordering::Relaxed);
        let free = cap - (head - self.tail.load(Ordering::Acquire));
        let n = data.len().min(free);
        for (i, &v) in data[..n].iter().enumerate() {
            // SAFETY: slots between head and tail+cap belong to the writer.
            unsafe { *self.buf[(head + i) % cap].get() = v };
        }
        self.head.store(head + n, Ordering::Release);
        n
    }

    /// The device's callback: fills `out` (interleaved) at `gain`, with
    /// silence for what isn't there. Never blocks or allocates.
    pub fn read(&self, out: &mut [f32], gain: f32) -> usize {
        let cap = self.buf.len();
        let tail = self.tail.load(Ordering::Relaxed);
        let have = self.head.load(Ordering::Acquire) - tail;
        let n = out.len().min(have);
        for (i, o) in out[..n].iter_mut().enumerate() {
            // SAFETY: slots between tail and head were published by the writer.
            *o = unsafe { *self.buf[(tail + i) % cap].get() } * gain;
        }
        out[n..].fill(0.0);
        self.tail.store(tail + n, Ordering::Release);
        if n < out.len() && self.active.load(Ordering::Relaxed) {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
        n
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }
}

/// Audio NACK, viewer -> host: `"GLNA"`, first missing seq u32, count u8.
pub const NACK_MAGIC: [u8; 4] = *b"GLNA";

pub fn encode_nack(first: u32, count: u8) -> [u8; 9] {
    let mut b = [0u8; 9];
    b[0..4].copy_from_slice(&NACK_MAGIC);
    b[4..8].copy_from_slice(&first.to_be_bytes());
    b[8] = count;
    b
}

pub fn decode_nack(b: &[u8]) -> Option<(u32, u8)> {
    (b.len() == 9 && b[0..4] == NACK_MAGIC)
        .then(|| (u32::from_be_bytes([b[4], b[5], b[6], b[7]]), b[8]))
}

/// What the viewer's audio did (for the results).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct AudioStats {
    /// Frames played from their own datagram, rebuilt from a later
    /// datagram's redundant copy, or resent after a NACK.
    pub frames: u64,
    pub from_redundancy: u64,
    pub from_resend: u64,
    pub nacks: u64,
    /// Frames that never came in time: concealed (Opus PLC, no click).
    pub concealed: u64,
    /// The device found the buffer empty while sound played (an audible gap).
    pub underruns: u64,
    /// Playout buffer: target and current depth (ms), and the drift
    /// correction (parts per million, + = playing faster).
    pub target_ms: u32,
    pub depth_ms: u32,
    pub drift_ppm: i32,
    /// From capture on the host to leaving this machine's buffer (ms,
    /// smoothed), once clocks are synced: the output device adds its own
    /// few ms. Compare with the picture's capture -> screen for A/V sync.
    pub latency_ms: u32,
}

/// The viewer's results line (the same on both platforms).
pub fn describe(s: &AudioStats) -> String {
    if s.frames == 0 && s.concealed == 0 {
        return "sound (M4b): none came".into();
    }
    format!(
        "sound (M4b): {} frames played ({} rebuilt from a later packet, {} resent; {} asks) | concealed {} | gaps {} | buffer {} ms (target {}) | drift {:+} ppm | latency {} ms + the output's",
        s.frames,
        s.from_redundancy,
        s.from_resend,
        s.nacks,
        s.concealed,
        s.underruns,
        s.depth_ms,
        s.target_ms,
        s.drift_ppm,
        s.latency_ms
    )
}

const SLOTS: usize = 64;
const START_TARGET_US: u64 = 30_000;
const MIN_TARGET_US: u64 = 20_000;
const MAX_TARGET_US: u64 = 80_000;
/// Conceal the next frame only when the buffer is this low and it hasn't come.
const LOW_WATER_US: u64 = 5_000;

#[derive(Clone, Copy)]
struct Slot {
    seq: u32,
    len: u16,
    /// How it came: 0 own datagram, 1 redundant copy, 2 resend.
    via: u8,
    /// Capture time on the host (µs; 0 if unknown, e.g. a resend).
    ts: u64,
    data: [u8; MAX_OPUS],
}

/// The viewer's side: frames in, steady audio out. Lives on the receiving
/// thread; the device reads [`Self::ring`].
pub struct AudioReceiver {
    dec: opus::Decoder,
    slots: Box<[Slot; SLOTS]>,
    next: Option<u32>,
    newest: Option<u32>,
    ring: Arc<PcmRing>,
    pcm: [f32; FRAME_LEN],
    out: Vec<f32>,
    target_us: u64,
    /// Smoothed buffer depth (µs) and the resampling ratio from it.
    depth_avg: f64,
    ratio: f64,
    /// Resampler: the position within the current input frame, and the
    /// last sample of the previous frame (for interpolation).
    pos: f64,
    prev: [f32; CHANNELS],
    last_underruns: u64,
    calm_since_us: u64,
    nack_last: Option<(u32, u64)>,
    rtt_us: u32,
    last_push_us: u64,
    /// `host clock - local clock` (µs), once synced.
    clock_offset: Option<i64>,
    latency_avg: f64,
    pub stats: AudioStats,
}

impl AudioReceiver {
    pub fn new() -> Result<Self, String> {
        let dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
            .map_err(|e| format!("Opus decoder: {e}"))?;
        Ok(Self {
            dec,
            slots: Box::new(
                [Slot {
                    seq: u32::MAX,
                    len: 0,
                    via: 0,
                    ts: 0,
                    data: [0; MAX_OPUS],
                }; SLOTS],
            ),
            next: None,
            newest: None,
            // 200 ms of room, far more than the target ever is.
            ring: Arc::new(PcmRing::new(SAMPLE_RATE as usize / 5 * CHANNELS)),
            pcm: [0.0; FRAME_LEN],
            out: Vec::with_capacity(FRAME_LEN * 2),
            target_us: START_TARGET_US,
            depth_avg: 0.0,
            ratio: 1.0,
            pos: 0.0,
            prev: [0.0; CHANNELS],
            last_underruns: 0,
            calm_since_us: 0,
            nack_last: None,
            rtt_us: 0,
            last_push_us: 0,
            clock_offset: None,
            latency_avg: 0.0,
            stats: AudioStats::default(),
        })
    }

    /// For the device's callback.
    pub fn ring(&self) -> Arc<PcmRing> {
        self.ring.clone()
    }

    pub fn set_rtt(&mut self, rtt_us: u32) {
        self.rtt_us = rtt_us;
    }

    /// `host clock - local clock` (µs), for the latency measurement.
    pub fn set_clock_offset(&mut self, offset_us: Option<i64>) {
        self.clock_offset = offset_us;
    }

    /// A datagram from the host (`is_audio`), received at `now_us`.
    pub fn push(&mut self, b: &[u8], now_us: u64) {
        let Some(p) = AudioPacket::decode(b) else {
            return;
        };
        let resent = p.capture_ts_us == 0;
        for (k, (seq, f)) in p.frames().enumerate() {
            if f.len() > MAX_OPUS || self.next.is_some_and(|n| before(seq, n)) {
                continue;
            }
            let slot = &mut self.slots[seq as usize % SLOTS];
            if slot.seq == seq {
                continue; // already here
            }
            slot.seq = seq;
            slot.len = f.len() as u16;
            slot.via = if resent { 2 } else { u8::from(k > 0) };
            slot.ts = if resent {
                0
            } else {
                p.capture_ts_us.saturating_sub(k as u64 * FRAME_US)
            };
            slot.data[..f.len()].copy_from_slice(f);
            if self.newest.is_none_or(|n| before(n, seq)) {
                self.newest = Some(seq);
            }
        }
        if self.next.is_none() {
            self.next = Some(p.seq);
        }
        self.last_push_us = now_us;
        self.ring.active.store(true, Ordering::Relaxed);
    }

    fn us(samples: usize) -> u64 {
        (samples / CHANNELS) as u64 * 1_000_000 / SAMPLE_RATE as u64
    }

    /// Keeps the ring at its target: decodes frames in order, conceals one
    /// that didn't come once the buffer runs low, asks for missing ones.
    /// Call every few ms (and after each `push`).
    pub fn tick(&mut self, now_us: u64, mut nack: impl FnMut(u32, u8)) {
        let Some(mut next) = self.next else {
            return;
        };
        // The host stopped sending (the session is ending, or sound is
        // off): running dry now isn't a glitch.
        if now_us.saturating_sub(self.last_push_us) > 100_000 {
            self.ring.active.store(false, Ordering::Relaxed);
        }
        // Missing frames with newer ones here: ask once a round trip.
        if let Some(newest) = self.newest.filter(|&n| before(next, n)) {
            let missing = (0..newest.wrapping_sub(next).min(SLOTS as u32))
                .map(|k| next.wrapping_add(k))
                .find(|&s| self.slots[s as usize % SLOTS].seq != s);
            if let Some(first) = missing {
                let again = (self.rtt_us as u64 * 3 / 2).max(4_000);
                let due = match self.nack_last {
                    Some((s, t)) => s != first || now_us.saturating_sub(t) >= again,
                    None => true,
                };
                if due {
                    let count = (first..newest)
                        .take_while(|&s| self.slots[s as usize % SLOTS].seq != s)
                        .count()
                        .clamp(1, 32) as u8;
                    nack(first, count);
                    self.stats.nacks += 1;
                    self.nack_last = Some((first, now_us));
                }
            }
        }
        // Adapt the target: up at once after a gap, down slowly when calm.
        let u = self.ring.underruns();
        if u > self.last_underruns {
            self.last_underruns = u;
            self.target_us = (self.target_us + 10_000).min(MAX_TARGET_US);
            self.calm_since_us = now_us;
        } else if now_us.saturating_sub(self.calm_since_us) > 10_000_000 {
            self.target_us = self.target_us.saturating_sub(5_000).max(MIN_TARGET_US);
            self.calm_since_us = now_us;
        }
        loop {
            let level = Self::us(self.ring.level());
            if level >= self.target_us {
                break;
            }
            let slot = &self.slots[next as usize % SLOTS];
            let here = slot.seq == next;
            let newer = self.newest.is_some_and(|n| before(next, n));
            let n = if here {
                let s = *slot;
                match s.via {
                    1 => self.stats.from_redundancy += 1,
                    2 => self.stats.from_resend += 1,
                    _ => {}
                }
                self.stats.frames += 1;
                // Played after what's already in the ring.
                if let (Some(off), true) = (self.clock_offset, s.ts != 0) {
                    let captured = crate::clock::host_to_local(s.ts, off);
                    let lat = (now_us + level).saturating_sub(captured) as f64;
                    self.latency_avg = if self.latency_avg == 0.0 {
                        lat
                    } else {
                        self.latency_avg + (lat - self.latency_avg) * 0.05
                    };
                    self.stats.latency_ms = (self.latency_avg / 1_000.0) as u32;
                }
                self.dec
                    .decode_float(&s.data[..s.len as usize], &mut self.pcm, false)
            } else if newer && level <= LOW_WATER_US {
                // It really didn't come: conceal (Opus fades it in), no
                // click, and make room so the next burst is covered.
                self.stats.concealed += 1;
                self.target_us = (self.target_us + 5_000).min(MAX_TARGET_US);
                self.calm_since_us = now_us;
                self.dec.decode_float(&[], &mut self.pcm, false)
            } else {
                break; // wait for it
            };
            let n = n.unwrap_or(0).min(FRAME_SAMPLES);
            next = next.wrapping_add(1);
            self.next = Some(next);
            self.write_resampled(n, level);
        }
        let depth = Self::us(self.ring.level());
        self.stats.target_ms = (self.target_us / 1_000) as u32;
        self.stats.depth_ms = (depth / 1_000) as u32;
        self.stats.underruns = self.ring.underruns();
        self.stats.drift_ppm = ((self.ratio - 1.0) * -1e6) as i32;
    }

    /// Writes `n` decoded samples per channel, resampled by the drift
    /// correction: a buffer fuller than the target plays a little faster
    /// (fewer samples), an emptier one a little slower, ±0.5 % at most.
    fn write_resampled(&mut self, n: usize, level_us: u64) {
        self.depth_avg += (level_us as f64 - self.depth_avg) * 0.02;
        let err = (self.depth_avg - self.target_us as f64) / 10_000.0;
        self.ratio = 1.0 + err.clamp(-1.0, 1.0) * 0.005; // input samples per output sample
        self.out.clear();
        // Output sample j sits at input position pos + j*ratio, where -1
        // is the previous frame's last sample.
        while self.pos < n as f64 - 1.0 {
            let i = self.pos.floor();
            let t = (self.pos - i) as f32;
            let i = i as isize;
            for c in 0..CHANNELS {
                let a = if i < 0 {
                    self.prev[c]
                } else {
                    self.pcm[i as usize * CHANNELS + c]
                };
                let b = self.pcm[(i + 1) as usize * CHANNELS + c];
                self.out.push(a + (b - a) * t);
            }
            self.pos += self.ratio;
        }
        self.pos -= n as f64;
        if n > 0 {
            self.prev
                .copy_from_slice(&self.pcm[(n - 1) * CHANNELS..n * CHANNELS]);
        }
        self.ring.write(&self.out);
    }
}

/// Wrapping order: `a` before `b`.
fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frame: u32) -> Vec<f32> {
        (0..FRAME_SAMPLES)
            .flat_map(|i| {
                let t = (frame as usize * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn packets_round_trip_with_redundancy() {
        let mut s = AudioSender::new().unwrap();
        let mut last = Vec::new();
        for f in 0..4 {
            last = s.frame(&tone(f), 1_000 + f as u64).unwrap().to_vec();
        }
        let p = AudioPacket::decode(&last).unwrap();
        assert_eq!((p.seq, p.capture_ts_us), (3, 1_003));
        let seqs: Vec<u32> = p.frames().map(|f| f.0).collect();
        assert_eq!(seqs, vec![3, 2, 1]);
        assert!(last.len() < 1_000, "{}", last.len());
        let r = s.resend(1).unwrap().to_vec();
        let p = AudioPacket::decode(&r).unwrap();
        assert_eq!((p.seq, p.capture_ts_us, p.frames().count()), (1, 0, 1));
        assert!(AudioPacket::decode(&last[..20]).is_none());
        assert_eq!(decode_nack(&encode_nack(77, 3)), Some((77, 3)));
    }

    /// Plays `frames` frames through a lossy link and a device reading in
    /// 5 ms pulls at `device_rate` × real time. `lose(seq)` drops the
    /// datagram carrying frame seq first.
    fn play(
        frames: u32,
        lose: impl Fn(u32) -> bool,
        device_rate: f64,
        nack: bool,
    ) -> (AudioStats, Vec<f32>) {
        let mut s = AudioSender::new().unwrap();
        let mut r = AudioReceiver::new().unwrap();
        r.set_rtt(4_000);
        let ring = r.ring();
        let mut heard = Vec::new();
        let mut pull = [0f32; FRAME_LEN / 2];
        let mut asked: Vec<(u64, u32, u8)> = Vec::new();
        let mut device_t = 0.0f64;
        // 1 ms steps.
        for ms in 0..(frames as u64 * 10 + 300) {
            let now = ms * 1_000;
            if ms % 10 == 0 && ms / 10 < frames as u64 {
                let f = (ms / 10) as u32;
                let d = s.frame(&tone(f), now + 1).unwrap().to_vec();
                if !lose(f) {
                    r.push(&d, now);
                }
            }
            // Resends arrive a round trip later.
            for &(t, first, count) in asked.iter() {
                if t == now {
                    for q in first..first + count as u32 {
                        if let Some(d) = s.resend(q) {
                            let d = d.to_vec();
                            r.push(&d, now);
                        }
                    }
                }
            }
            r.tick(now, |first, count| {
                if nack {
                    asked.push((now + 4_000, first, count));
                }
            });
            // The device: a 5 ms pull whenever its clock says so.
            if ms > 40 && ms < frames as u64 * 10 {
                device_t += device_rate;
            }
            if device_t >= 5.0 {
                device_t -= 5.0;
                let before_u = ring.underruns();
                let lvl = ring.level();
                ring.read(&mut pull, 1.0);
                if ring.underruns() > before_u && std::env::var("AUDIO_DEBUG").is_ok() {
                    eprintln!(
                        "underrun at {ms} ms, level was {lvl}, next {:?} newest {:?} target {}",
                        r.next, r.newest, r.target_us
                    );
                }
                heard.extend_from_slice(&pull);
            }
        }
        (r.stats, heard)
    }

    #[test]
    fn a_clean_link_plays_every_frame() {
        let (s, heard) = play(500, |_| false, 1.0, true);
        assert_eq!(
            (s.concealed, s.from_redundancy, s.nacks),
            (0, 0, 0),
            "{s:?}"
        );
        assert!(s.frames >= 490);
        let loud = heard.iter().filter(|v| v.abs() > 0.1).count();
        assert!(loud > heard.len() / 3, "{loud} of {}", heard.len());
    }

    #[test]
    fn redundancy_covers_two_lost_in_a_row() {
        // Every 7th and 8th datagram lost: redundancy rebuilds them all.
        let (s, _) = play(500, |f| f % 50 == 7 || f % 50 == 8, 1.0, false);
        assert_eq!(s.concealed, 0, "{s:?}");
        assert!(s.from_redundancy >= 18, "{s:?}");
    }

    #[test]
    fn five_percent_random_loss_is_never_heard() {
        let mut x = 0x2545_F491u32;
        let lost: Vec<bool> = (0..1_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x % 100 < 5
            })
            .collect();
        let (s, _) = play(1_000, |f| lost[f as usize], 1.0, true);
        assert_eq!((s.concealed, s.underruns), (0, 0), "{s:?}");
    }

    #[test]
    fn a_longer_burst_is_resent() {
        // 30 ms with nothing arriving: redundancy brings back two frames,
        // a resend the third (at most one concealed, never a gap).
        let (s, _) = play(500, |f| (200..203).contains(&f), 1.0, true);
        assert!(s.concealed <= 1 && s.underruns == 0, "{s:?}");
        assert!(s.nacks >= 1 && s.from_redundancy >= 2, "{s:?}");
        // After it the buffer is deeper, so the same burst again costs
        // nothing.
        let (s, _) = play(
            800,
            |f| (200..203).contains(&f) || (600..603).contains(&f),
            1.0,
            true,
        );
        assert!(s.concealed <= 1 && s.underruns == 0, "{s:?}");
    }

    #[test]
    fn without_a_resend_a_long_gap_is_concealed_not_silent() {
        let (s, _) = play(500, |f| (200..210).contains(&f), 1.0, false);
        assert!(s.concealed >= 6, "{s:?}");
    }

    /// The biggest step between neighbouring samples of one channel,
    /// ignoring the silence before the device's first full read.
    fn worst_step(heard: &[f32]) -> f32 {
        let start = heard.iter().position(|v| v.abs() > 0.01).unwrap_or(0) / 2 * 2;
        heard[start..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| c[0])
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn no_clicks_while_following_drift() {
        // A 440 Hz tone at 0.5 moves at most 0.029 a sample; Opus adds a
        // little. A click (a gap, a dropped or repeated stretch) is far more.
        for rate in [1.0, 1.003, 0.997] {
            let (s, heard) = play(1_500, |_| false, rate, true);
            let w = worst_step(&heard);
            assert!(w < 0.06, "{rate}: step {w} {s:?}");
        }
    }

    #[test]
    fn clock_drift_is_followed_without_gaps() {
        // The viewer's device runs 0.3 % fast, then 0.3 % slow.
        for rate in [1.003, 0.997] {
            let (s, _) = play(3_000, |_| false, rate, true);
            assert_eq!((s.concealed, s.underruns), (0, 0), "{rate} {s:?}");
            assert!((15..=45).contains(&s.depth_ms), "{rate} {s:?}");
        }
    }
}

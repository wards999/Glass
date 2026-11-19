//! Per-frame stage timestamps. Fixed-size and allocation-free so the hot path
//! can record into it directly.

/// Pipeline stages, in order. Host stages first, then client stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    Capture = 0,
    Encode = 1,
    Packetise = 2,
    Receive = 3,
    Decode = 4,
    Present = 5,
}

impl Stage {
    pub const COUNT: usize = 6;
}

/// Timestamps (microseconds) for one frame. 0 = not recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameTimestamps {
    pub frame_id: u32,
    t_us: [u64; Stage::COUNT],
}

impl FrameTimestamps {
    pub fn new(frame_id: u32) -> Self {
        Self {
            frame_id,
            t_us: [0; Stage::COUNT],
        }
    }

    pub fn record(&mut self, stage: Stage, ts_us: u64) {
        self.t_us[stage as usize] = ts_us;
    }

    pub fn get(&self, stage: Stage) -> Option<u64> {
        match self.t_us[stage as usize] {
            0 => None,
            t => Some(t),
        }
    }

    /// Microseconds from `from` to `to`, if both were recorded and are ordered.
    /// Cross-machine spans need clock sync (LATER(M2)).
    pub fn span_us(&self, from: Stage, to: Stage) -> Option<u64> {
        self.get(to)?.checked_sub(self.get(from)?)
    }
}

/// Capacity of a [`StatRing`]: about 34 s at 120 fps.
pub const RING_CAP: usize = 4096;

/// Fixed-capacity ring of duration samples (µs). `push` is O(1) and never
/// allocates; `summary` sorts a copy and is meant for reporting, off the hot path.
#[derive(Clone)]
pub struct StatRing {
    buf: [u32; RING_CAP],
    len: usize,
    next: usize,
    total: u64,
}

/// Percentiles in microseconds over the samples currently held.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(C)]
pub struct Summary {
    /// Samples ever pushed (may exceed the ring capacity).
    pub total: u64,
    /// Samples summarised (≤ RING_CAP).
    pub count: u32,
    pub p50_us: u32,
    pub p99_us: u32,
    pub max_us: u32,
    pub mean_us: f64,
    pub stddev_us: f64,
}

impl Default for StatRing {
    fn default() -> Self {
        Self::new()
    }
}

impl StatRing {
    pub const fn new() -> Self {
        Self {
            buf: [0; RING_CAP],
            len: 0,
            next: 0,
            total: 0,
        }
    }

    pub fn push(&mut self, us: u32) {
        self.buf[self.next] = us;
        self.next = (self.next + 1) % RING_CAP;
        self.len = (self.len + 1).min(RING_CAP);
        self.total += 1;
    }

    pub fn summary(&self) -> Summary {
        if self.len == 0 {
            return Summary {
                total: self.total,
                ..Default::default()
            };
        }
        let mut s = self.buf;
        let s = &mut s[..self.len];
        s.sort_unstable();
        let n = s.len();
        // Nearest-rank percentile.
        let pct = |p: f64| s[((p / 100.0 * n as f64).ceil() as usize).clamp(1, n) - 1];
        let mean = s.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
        let var = s.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n as f64;
        Summary {
            total: self.total,
            count: n as u32,
            p50_us: pct(50.0),
            p99_us: pct(99.0),
            max_us: s[n - 1],
            mean_us: mean,
            stddev_us: var.sqrt(),
        }
    }
}

/// Records the interval between successive timestamps (e.g. present times);
/// its summary's stddev/p99 is the frame-interval jitter metric.
#[derive(Clone, Default)]
pub struct IntervalTracker {
    last_us: Option<u64>,
    pub intervals: StatRing,
}

impl IntervalTracker {
    pub fn tick(&mut self, ts_us: u64) {
        if let Some(prev) = self.last_us {
            self.intervals
                .push(ts_us.saturating_sub(prev).min(u32::MAX as u64) as u32);
        }
        self.last_us = Some(ts_us);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_spans() {
        let mut f = FrameTimestamps::new(7);
        f.record(Stage::Capture, 1_000);
        f.record(Stage::Encode, 3_500);
        assert_eq!(f.span_us(Stage::Capture, Stage::Encode), Some(2_500));
        assert_eq!(f.span_us(Stage::Capture, Stage::Present), None);
        assert_eq!(f.span_us(Stage::Encode, Stage::Capture), None);
    }

    #[test]
    fn percentiles() {
        let mut r = StatRing::new();
        assert_eq!(r.summary().count, 0);
        for v in 1..=100 {
            r.push(v);
        }
        let s = r.summary();
        assert_eq!((s.count, s.p50_us, s.p99_us, s.max_us), (100, 50, 99, 100));
        assert!((s.mean_us - 50.5).abs() < 1e-9);
    }

    #[test]
    fn ring_keeps_latest_samples() {
        let mut r = StatRing::new();
        for _ in 0..RING_CAP {
            r.push(1_000_000);
        }
        for _ in 0..RING_CAP {
            r.push(5);
        }
        let s = r.summary();
        assert_eq!(
            (s.total, s.count, s.max_us),
            (2 * RING_CAP as u64, RING_CAP as u32, 5)
        );
    }

    #[test]
    fn interval_jitter() {
        let mut t = IntervalTracker::default();
        for ts in [0, 8_333, 16_666, 25_000, 33_333] {
            t.tick(ts);
        }
        let s = t.intervals.summary();
        assert_eq!(s.count, 4);
        assert!(s.stddev_us < 1.0);
        assert_eq!(s.p50_us, 8_333);
    }
}

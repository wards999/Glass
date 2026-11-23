//! Pacing video packets (M5, SPEC §6b "Pacing and send path"): a frame's
//! sealed datagrams are queued with send times spread over a short window
//! instead of leaving as one burst, which Wi-Fi turns into a queue at the
//! access point and a delay spike for everything after it.
//!
//! The window is short on purpose: a frame is spread over at most half a
//! refresh of the viewer's screen ([`SPREAD_OF_REFRESH`]), and never sent
//! slower than [`MIN_RATE_X`] times the recent average bitrate, so a big
//! frame (a scene change) is late by a few ms at most. The SPEC's 1.5× the
//! bitrate would have spread m5-test6's 335 KB scene change over ~30 ms:
//! the very moment the user notices.
//!
//! **Newest first:** when a new frame is queued, whatever of the previous
//! one is still waiting goes at once (never dropped, never queued behind).
//! Audio, input, resends and keep-alives don't come through here, so they
//! always go first.
//!
//! A fixed ring, set up once: no allocation per frame.

/// Datagrams the queue holds. Fewer than the receiver's replay window
/// (1024), so packets sent out of order with audio and resends are never
/// refused.
pub const SLOTS: usize = 768;
/// The longest a frame is spread over, as a share of the viewer's refresh.
pub const SPREAD_OF_REFRESH: f64 = 0.5;
/// Never slower than this times the recent average bitrate.
pub const MIN_RATE_X: f64 = 4.0;

pub struct PaceQueue {
    buf: Vec<u8>,
    lens: Vec<u16>,
    due: Vec<u64>,
    slot: usize,
    head: usize,
    count: usize,
    /// When the last queued datagram is due (µs).
    tail_due: u64,
}

impl PaceQueue {
    /// Room for [`SLOTS`] datagrams of up to `slot` bytes.
    pub fn new(slot: usize) -> Self {
        Self {
            buf: vec![0; SLOTS * slot],
            lens: vec![0; SLOTS],
            due: vec![0; SLOTS],
            slot,
            head: 0,
            count: 0,
            tail_due: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// A new frame is about to be queued at `now_us`: what's left of the
    /// previous one becomes due at once. Returns how many were hurried.
    pub fn hurry(&mut self, now_us: u64) -> usize {
        for k in 0..self.count {
            let i = (self.head + k) % SLOTS;
            self.due[i] = self.due[i].min(now_us);
        }
        self.tail_due = self.tail_due.min(now_us);
        self.count
    }

    /// Queues one datagram due at `due_us`. False if full (send it now).
    pub fn push(&mut self, d: &[u8], due_us: u64) -> bool {
        if self.count == SLOTS || d.len() > self.slot {
            return false;
        }
        let i = (self.head + self.count) % SLOTS;
        self.buf[i * self.slot..i * self.slot + d.len()].copy_from_slice(d);
        self.lens[i] = d.len() as u16;
        self.due[i] = due_us;
        self.count += 1;
        self.tail_due = self.tail_due.max(due_us);
        true
    }

    /// The next datagram if it's due by `now_us`.
    pub fn front_due(&self, now_us: u64) -> Option<&[u8]> {
        if self.count == 0 || self.due[self.head] > now_us {
            return None;
        }
        let i = self.head;
        Some(&self.buf[i * self.slot..i * self.slot + self.lens[i] as usize])
    }

    /// Drops the front datagram (after sending it).
    pub fn pop(&mut self) {
        if self.count > 0 {
            self.head = (self.head + 1) % SLOTS;
            self.count -= 1;
        }
    }

    /// When the next datagram is due, if any.
    pub fn next_due(&self) -> Option<u64> {
        (self.count > 0).then(|| self.due[self.head])
    }
}

/// A pretend bottleneck for the weak-link test mode (on the viewer):
/// datagrams leave at `mbps`, queue when more comes, and are dropped past
/// `max_queue_us` of queue, as a weak router does. With the mode off, what
/// is still queued drains in order first.
pub struct Bottleneck {
    q: PaceQueue,
    free_at: u64,
    pub dropped: u64,
}

impl Bottleneck {
    pub fn new(slot: usize) -> Self {
        Self {
            q: PaceQueue::new(slot),
            free_at: 0,
            dropped: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    /// Takes a datagram arriving at `now_us` through a link of `mbps` (0:
    /// off, just keeping the order). False if it was dropped.
    pub fn admit(&mut self, d: &[u8], now_us: u64, mbps: u32, max_queue_us: u64) -> bool {
        let start = self.free_at.max(now_us);
        let done = if mbps == 0 {
            start
        } else {
            start + (d.len() as f64 * 8.0 / mbps as f64) as u64
        };
        if done.saturating_sub(now_us) > max_queue_us || !self.q.push(d, done) {
            self.dropped += 1;
            return false;
        }
        self.free_at = done;
        true
    }

    /// The next datagram through by `now_us`, if any; `pop` after it.
    pub fn front_due(&self, now_us: u64) -> Option<&[u8]> {
        self.q.front_due(now_us)
    }

    pub fn pop(&mut self) {
        self.q.pop();
    }
}

/// How fast to send a frame of `bytes` (bytes per µs): over at most
/// [`SPREAD_OF_REFRESH`] of the viewer's refresh interval, and never slower
/// than [`MIN_RATE_X`] × `avg_bytes_per_us` (the recent average).
pub fn rate(bytes: usize, refresh_us: u64, avg_bytes_per_us: f64) -> f64 {
    let window = (refresh_us as f64 * SPREAD_OF_REFRESH).max(1.0);
    (bytes as f64 / window)
        .max(avg_bytes_per_us * MIN_RATE_X)
        .max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreads_a_frame_and_hurries_it_for_a_newer_one() {
        let mut q = PaceQueue::new(1400);
        // 10 datagrams, one every 100 µs from t=1000.
        for k in 0..10u64 {
            assert!(q.push(&[k as u8; 1200], 1_000 + k * 100));
        }
        assert_eq!(q.len(), 10);
        let mut sent = Vec::new();
        let mut now = 1_000;
        while let Some(d) = q.front_due(now) {
            sent.push(d[0]);
            q.pop();
        }
        assert_eq!(sent, vec![0]); // only the first is due yet
        now = 1_250;
        while let Some(d) = q.front_due(now) {
            sent.push(d[0]);
            q.pop();
        }
        assert_eq!(sent, vec![0, 1, 2]);
        // A newer frame: the other 7 go at once.
        assert_eq!(q.hurry(now), 7);
        while let Some(d) = q.front_due(now) {
            sent.push(d[0]);
            q.pop();
        }
        assert_eq!(sent.len(), 10);
        assert!(q.is_empty());
    }

    #[test]
    fn full_means_send_it_now() {
        let mut q = PaceQueue::new(100);
        for _ in 0..SLOTS {
            assert!(q.push(&[1; 100], 0));
        }
        assert!(!q.push(&[1; 100], 0));
        assert!(!PaceQueue::new(100).push(&[1; 101], 0));
    }

    /// The weak-link test mode: 45 Mbps arriving through 15 Mbps.
    #[test]
    fn the_bottleneck_queues_then_drops() {
        let mut b = Bottleneck::new(1400);
        let (mut through, mut worst) = (0usize, 0u64);
        let mut sent_at = std::collections::VecDeque::new();
        // 1 s: a 1200-byte datagram every 213 µs is 45 Mbps.
        let mut t = 0u64;
        while t < 1_000_000 {
            if b.admit(&[0; 1200], t, 15, 150_000) {
                sent_at.push_back(t);
            }
            while b.front_due(t).is_some() {
                b.pop();
                through += 1200;
                worst = worst.max(t - sent_at.pop_front().unwrap());
            }
            t += 213;
        }
        let mbps = through as f64 * 8.0 / 1e6;
        assert!((12.0..=16.0).contains(&mbps), "{mbps}");
        assert!((140_000..=152_000).contains(&worst), "{worst}");
        assert!(b.dropped > 2_000, "{}", b.dropped);
        // Off: what's queued drains in order, then straight through.
        assert!(b.admit(&[1; 10], t, 0, 150_000));
        assert!(!b.is_empty());
    }

    #[test]
    fn big_frames_take_at_most_half_a_refresh() {
        // m5-test6's 335 KB scene change, a 60 Hz viewer, 45 Mbps average.
        let avg = 45e6 / 8.0 / 1e6;
        let r = rate(335_000, 16_667, avg);
        let spread_ms = 335_000.0 / r / 1e3;
        assert!((8.0..=8.5).contains(&spread_ms), "{spread_ms}");
        // A typical 47 KB frame: 4x the average, about 2 ms.
        let typical = 47_000.0 / rate(47_000, 16_667, avg) / 1e3;
        assert!((1.5..=2.5).contains(&typical), "{typical}");
    }
}

//! Monotonic clock in microseconds, plus the NTP-style offset estimator used to
//! map host timestamps onto the client's clock.
//!
//! The time base matches each platform's media timestamps so apps can mix them:
//! - Windows: QueryPerformanceCounter (same base as WGC `SystemRelativeTime`).
//! - macOS: `CLOCK_UPTIME_RAW` = `mach_absolute_time` = `CACurrentMediaTime()`
//!   and `CAMetalDrawable.presentedTime`.
//! - elsewhere (CI only): time since first use.

/// Current time in microseconds on this machine's media clock.
pub fn now_us() -> u64 {
    sys::now_us()
}

#[cfg(windows)]
mod sys {
    use std::sync::OnceLock;

    #[link(name = "kernel32")]
    extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
    }

    fn freq() -> i64 {
        static F: OnceLock<i64> = OnceLock::new();
        *F.get_or_init(|| {
            let mut f = 0;
            unsafe { QueryPerformanceFrequency(&mut f) };
            f.max(1)
        })
    }

    pub fn now_us() -> u64 {
        let mut c = 0;
        unsafe { QueryPerformanceCounter(&mut c) };
        (c as i128 * 1_000_000 / freq() as i128) as u64
    }
}

#[cfg(target_os = "macos")]
mod sys {
    extern "C" {
        fn clock_gettime_nsec_np(clock_id: u32) -> u64;
    }
    const CLOCK_UPTIME_RAW: u32 = 8;

    pub fn now_us() -> u64 {
        unsafe { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) / 1_000 }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod sys {
    use std::sync::OnceLock;
    use std::time::Instant;

    pub fn now_us() -> u64 {
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        // +1 s so a real timestamp is never 0 (0 means "not recorded" in stats).
        EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64 + 1_000_000
    }
}

/// Estimates `host_clock - client_clock` from ping/pong exchanges. Keeps the
/// last [`ClockSync::WINDOW`] samples and trusts the one with the lowest RTT,
/// whose midpoint assumption has the smallest error bound (±RTT/2).
#[derive(Debug, Clone, Default)]
pub struct ClockSync {
    samples: [(u32, i64); Self::WINDOW], // (rtt_us, offset_us)
    len: usize,
    next: usize,
}

impl ClockSync {
    pub const WINDOW: usize = 16;

    /// `t0` = client send, `host` = host receive, `t1` = client receive.
    pub fn add(&mut self, t0: u64, host: u64, t1: u64) {
        if t1 < t0 {
            return;
        }
        let rtt = (t1 - t0).min(u32::MAX as u64) as u32;
        let mid = t0 as i128 + (t1 - t0) as i128 / 2;
        let offset = (host as i128 - mid) as i64;
        self.samples[self.next] = (rtt, offset);
        self.next = (self.next + 1) % Self::WINDOW;
        self.len = (self.len + 1).min(Self::WINDOW);
    }

    fn best(&self) -> Option<(u32, i64)> {
        self.samples[..self.len].iter().copied().min_by_key(|s| s.0)
    }

    /// `host - client` in µs, if any sample exists.
    pub fn offset_us(&self) -> Option<i64> {
        self.best().map(|s| s.1)
    }

    /// RTT of the sample behind the current offset.
    pub fn best_rtt_us(&self) -> Option<u32> {
        self.best().map(|s| s.0)
    }

    /// RTT of the most recent sample.
    pub fn last_rtt_us(&self) -> Option<u32> {
        (self.len > 0).then(|| self.samples[(self.next + Self::WINDOW - 1) % Self::WINDOW].0)
    }
}

/// Converts a host timestamp to the client clock given `offset = host - client`.
pub fn host_to_local(host_us: u64, offset_us: i64) -> u64 {
    (host_us as i128 - offset_us as i128).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_monotonic_and_nonzero() {
        let a = now_us();
        let b = now_us();
        assert!(a > 0 && b >= a);
    }

    #[test]
    fn offset_from_symmetric_exchange() {
        // Host clock is 1_000_000 µs ahead; one-way delay 150 µs each way.
        let mut c = ClockSync::default();
        c.add(10_000, 1_010_150, 10_300);
        assert_eq!(c.offset_us(), Some(1_000_000));
        assert_eq!(c.best_rtt_us(), Some(300));
        assert_eq!(host_to_local(1_020_000, 1_000_000), 20_000);
    }

    #[test]
    fn prefers_lowest_rtt_sample() {
        let mut c = ClockSync::default();
        // Asymmetric slow sample: request delayed 5 ms -> offset off by 2.5 ms.
        c.add(0, 1_005_100, 5_200);
        c.add(10_000, 1_010_100, 10_200); // fast, accurate
        c.add(20_000, 1_024_000, 24_100); // slow again
        assert_eq!(c.offset_us(), Some(1_000_000));
        assert_eq!(c.best_rtt_us(), Some(200));
        assert_eq!(c.last_rtt_us(), Some(4_100));
    }

    #[test]
    fn window_forgets_old_samples() {
        let mut c = ClockSync::default();
        c.add(0, 500, 10); // very fast but will age out
        for i in 0..ClockSync::WINDOW as u64 {
            let t = 1_000 + i * 1_000;
            c.add(t, t + 2_000 + 50, t + 100);
        }
        assert_eq!(c.offset_us(), Some(2_000));
    }

    #[test]
    fn ignores_backwards_samples() {
        let mut c = ClockSync::default();
        c.add(100, 0, 50);
        assert_eq!(c.offset_us(), None);
    }
}

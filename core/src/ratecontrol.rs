//! Video rate control (M5, SPEC §6b "Congestion and bitrate control"),
//! on the viewer: delay-based, like WebRTC's GCC, so the bitrate backs off
//! as soon as a queue starts to build, before anything is lost.
//!
//! Every [`TICK_US`] the viewer gives it the average *first-packet*
//! network time of the frames that arrived (a big frame doesn't stretch
//! it; a queue does), how much was lost or late so far, and how fast video
//! actually arrived. Against a quiet baseline (the lowest tick average of
//! the last 10 s):
//! - more than [`CONGESTED_US`] over for two ticks, or a loss, while the
//!   stream is using the link: **back off** to 85 % of the limit (of what
//!   arrived, if there was none), and hold for [`HOLD_US`];
//! - more than [`WORRIED_US`] over: hold;
//! - otherwise **ramp up** 5 % a tick (about doubling in 1.5 s) until it
//!   stops limiting anything, and then no limit at all (the host's own
//!   setting applies).
//!
//! On a good link it never limits. It acts on a weak or busy one: the far
//! end of the house, a download, a file copy.

/// How often the controller runs (µs).
pub const TICK_US: u64 = 100_000;
/// Average first-packet time this far over the baseline: back off (µs).
pub const CONGESTED_US: u32 = 15_000;
/// This far over: hold (µs).
pub const WORRIED_US: u32 = 6_000;
/// After backing off, no ramp-up for this long (µs).
pub const HOLD_US: u64 = 500_000;
/// A limit the stream isn't using still rises after this long calm (µs)...
pub const IDLE_CALM_US: u64 = 3_000_000;
/// ...by 5 % this often (µs): from 20 Mbps to no limit in about 25 s.
pub const IDLE_STEP_US: u64 = 500_000;
/// With no limit, the stream counts as using the link from twice this
/// (bits/s); with one, from half of it.
pub const BUSY_NO_LIMIT_BPS: f64 = 20e6;
/// Never below this: text and fine detail go soft (bits/s).
pub const MIN_BPS: f64 = 8e6;
/// At or above this it's no limit (bits/s).
pub const MAX_BPS: f64 = 200e6;

#[derive(Debug, Default)]
pub struct VideoRateControl {
    /// The limit (bits/s), or `None`: no limit.
    target: Option<f64>,
    base: u32,
    base_since: u64,
    last_bad: u64,
    hold_until: u64,
    /// Since when nothing worried it (µs), and the last idle rise.
    calm_since: u64,
    idle_up_at: u64,
    /// Ticks in a row over [`CONGESTED_US`].
    congested_ticks: u32,
    last_sent: Option<Option<u32>>,
    last_sent_at: u64,
    pub backoffs: u32,
    pub lowest_bps: u32,
}

impl VideoRateControl {
    /// One tick: `delay_us` is the average first-packet network time of the
    /// frames since the last tick (`None` if none came), `bad` the frames
    /// lost plus late packets so far, `received_bps` how fast video arrived
    /// over the tick. Returns the limit to send to the host when it changed
    /// (`Some(0)`: no limit), or `None` (nothing to send).
    pub fn tick(
        &mut self,
        now_us: u64,
        delay_us: Option<u32>,
        bad: u64,
        received_bps: f64,
    ) -> Option<u32> {
        let lost = bad > self.last_bad;
        self.last_bad = bad;
        let Some(d) = delay_us else {
            return self.report(now_us);
        };
        if self.base == 0 || d < self.base || now_us.saturating_sub(self.base_since) > 10_000_000 {
            self.base = d.max(1);
            self.base_since = now_us;
        }
        let over = d.saturating_sub(self.base);
        // Only a stream that's using the link can be what congests it: a
        // spike while a still picture trickles out is Wi-Fi jitter, and
        // backing off then only spoils the next picture (QUIETPC, m5-test21:
        // 10 backoffs to the 8 Mbps floor on a wired PC and a clean 6 GHz
        // link, photos blocky). And a rise must last two ticks (200 ms).
        let busy = received_bps >= self.target.unwrap_or(BUSY_NO_LIMIT_BPS) * 0.5;
        self.congested_ticks = if over > CONGESTED_US {
            self.congested_ticks + 1
        } else {
            0
        };
        // Not again within the hold: the queue it saw needs a moment to drain.
        if busy && now_us >= self.hold_until && (lost || self.congested_ticks >= 2) {
            // From the limit itself, not from what happened to arrive.
            let t = match self.target {
                Some(t) => t * 0.85,
                None => received_bps * 0.85,
            }
            .max(MIN_BPS);
            self.congested_ticks = 0;
            self.target = Some(t);
            self.hold_until = now_us + HOLD_US;
            self.calm_since = now_us;
            self.backoffs += 1;
            let t = t as u32;
            if self.lowest_bps == 0 || t < self.lowest_bps {
                self.lowest_bps = t;
            }
        } else if over > WORRIED_US {
            self.calm_since = now_us;
        } else if now_us >= self.hold_until {
            // Raise a limit quickly while it's being used (the encoder near
            // it). One that isn't (a still screen, a quiet game) rises too,
            // but slowly and only after a calm spell: m5-test12 sat at a
            // 22.8 Mbps limit for 100 s after the weak link had gone, the
            // host held at the viewer's refresh, because a 60 fps pool table
            // used too little of it to count. Past the top, no limit.
            if let Some(t) = self.target {
                let used = received_bps >= t * 0.5;
                let idle_ok = now_us.saturating_sub(self.calm_since) >= IDLE_CALM_US
                    && now_us.saturating_sub(self.idle_up_at) >= IDLE_STEP_US;
                if used || idle_ok {
                    if !used {
                        self.idle_up_at = now_us;
                    }
                    let up = t * 1.05;
                    self.target = (up < MAX_BPS).then_some(up);
                }
            }
        }
        self.report(now_us)
    }

    /// The current limit (bits/s, 0 = none).
    pub fn limit(&self) -> u32 {
        self.target.map_or(0, |t| t as u32)
    }

    /// What to send: a change of 3 % or more, or the same again each
    /// second (a lost message mustn't leave the host stuck).
    fn report(&mut self, now_us: u64) -> Option<u32> {
        let now = self.target.map(|t| t as u32);
        let changed = match self.last_sent {
            None => now.is_some(),
            Some(prev) => match (prev, now) {
                (Some(a), Some(b)) => (a as f64 - b as f64).abs() >= a as f64 * 0.03,
                (a, b) => a != b,
            },
        };
        let refresh =
            self.last_sent.is_some() && now_us.saturating_sub(self.last_sent_at) >= 1_000_000;
        if changed || refresh {
            self.last_sent = Some(now);
            self.last_sent_at = now_us;
            Some(now.unwrap_or(0))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `ticks` ticks; `delay(i)` and `rate(i, limit)` give each tick's
    /// first-packet time and what arrived under the current limit.
    fn run(
        c: &mut VideoRateControl,
        start: u64,
        ticks: u64,
        delay: impl Fn(u64) -> u32,
        rate: impl Fn(u64, u32) -> f64,
        bad: impl Fn(u64) -> u64,
    ) -> Vec<u32> {
        let mut sent = Vec::new();
        for i in 0..ticks {
            let now = (start + i) * TICK_US;
            if let Some(b) = c.tick(now, Some(delay(i)), bad(i), rate(i, c.limit())) {
                sent.push(b);
            }
        }
        sent
    }

    #[test]
    fn a_good_link_is_never_limited() {
        let mut c = VideoRateControl::default();
        // 3–4 ms first-packet times with jitter, 45 Mbps arriving.
        let sent = run(
            &mut c,
            0,
            300,
            |i| 3_000 + (i % 7) as u32 * 150,
            |_, _| 45e6,
            |_| 0,
        );
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!((c.limit(), c.backoffs), (0, 0));
    }

    #[test]
    fn a_lone_wifi_spike_doesnt_back_off() {
        let mut c = VideoRateControl::default();
        // One tick's average 10 ms over (a 40 ms spike among 10 frames).
        let sent = run(
            &mut c,
            0,
            100,
            |i| if i == 50 { 13_000 } else { 3_000 },
            |_, _| 45e6,
            |_| 0,
        );
        assert!(sent.is_empty(), "{sent:?}");
    }

    #[test]
    fn a_queue_backs_off_at_once_and_recovers_after() {
        let mut c = VideoRateControl::default();
        run(&mut c, 0, 50, |_| 3_000, |_, _| 45e6, |_| 0);
        // The link drops to 20 Mbps: delay climbs while more is sent.
        let sent = run(
            &mut c,
            50,
            5,
            |i| 3_000 + 10_000 * (i as u32 + 1),
            |_, _| 20e6,
            |_| 0,
        );
        assert_eq!(sent[0], 17_000_000, "85 % of what arrived: {sent:?}");
        assert_eq!(c.limit(), 17_000_000);
        // Now within its means the queue drains; once calm it climbs back
        // until it no longer limits anything: it rises while used, so it
        // settles at about twice what the stream uses (90 against 45 Mbps:
        // above the host's own cap, so in effect no limit).
        let sent = run(
            &mut c,
            55,
            300,
            |_| 3_000,
            |_, limit| (limit as f64).clamp(1.0, 45e6),
            |_| 0,
        );
        let last = *sent.last().unwrap();
        assert!(last == 0 || last >= 85_000_000, "{sent:?}");
        assert!(c.backoffs >= 1 && c.lowest_bps == 17_000_000);
    }

    /// m5-test12: after the weak link, a quiet picture using a third of the
    /// limit. It must still come off, slowly.
    #[test]
    fn an_unused_limit_still_goes_once_calm() {
        let mut c = VideoRateControl::default();
        run(&mut c, 0, 50, |_| 3_000, |_, _| 45e6, |_| 0);
        run(&mut c, 50, 2, |_| 30_000, |_, _| 24e6, |_| 0);
        assert_eq!(c.limit(), 20_400_000);
        // 7 Mbps used: not "used", but calm. Nothing for the first 3 s...
        run(&mut c, 51, 30, |_| 3_000, |_, _| 7e6, |_| 0);
        assert!(c.limit() <= 21_500_000, "{}", c.limit());
        // ...then 5 % every 0.5 s: no limit within about 30 s more.
        run(&mut c, 81, 300, |_| 3_000, |_, _| 7e6, |_| 0);
        assert_eq!(c.limit(), 0);
    }

    /// m5-test21: Wi-Fi spikes while a still picture trickles out must
    /// not limit anything.
    #[test]
    fn spikes_on_an_idle_stream_dont_back_off() {
        let mut c = VideoRateControl::default();
        let sent = run(
            &mut c,
            0,
            300,
            |i| if i % 10 < 3 { 55_000 } else { 3_000 },
            |_, _| 3e6,
            |i| i / 50,
        );
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!((c.limit(), c.backoffs), (0, 0));
    }

    /// A lone tick over isn't congestion; two are.
    #[test]
    fn one_tick_over_is_not_enough() {
        let mut c = VideoRateControl::default();
        run(&mut c, 0, 20, |_| 3_000, |_, _| 45e6, |_| 0);
        run(&mut c, 20, 1, |_| 30_000, |_, _| 45e6, |_| 0);
        assert_eq!(c.limit(), 0);
        run(&mut c, 21, 1, |_| 30_000, |_, _| 45e6, |_| 0);
        assert_eq!(c.limit(), 38_250_000);
    }

    /// From the limit, not from the trickle that arrived.
    #[test]
    fn backs_off_from_the_limit() {
        let mut c = VideoRateControl::default();
        run(&mut c, 0, 20, |_| 3_000, |_, _| 45e6, |_| 0);
        run(&mut c, 20, 2, |_| 30_000, |_, _| 40e6, |_| 0);
        assert_eq!(c.limit(), 34_000_000);
        // Busy at 25 Mbps (over half the limit): 85 % of the limit, not
        // of 25.
        run(&mut c, 22, 10, |_| 3_000, |_, _| 25e6, |_| 0);
        let before = c.limit() as f64;
        run(&mut c, 40, 2, |_| 40_000, |_, _| 25e6, |_| 0);
        assert!(
            (c.limit() as f64 - before * 0.85).abs() <= 2.0,
            "{} {before}",
            c.limit()
        );
    }

    #[test]
    fn a_loss_backs_off_and_it_never_goes_below_the_floor() {
        let mut c = VideoRateControl::default();
        run(&mut c, 0, 10, |_| 3_000, |_, _| 45e6, |_| 0);
        // Losses while the stream uses the link: down by 15 % a time (one
        // each hold) to the floor, never below.
        run(&mut c, 10, 80, |_| 3_000, |_, _| 45e6, |i| i);
        assert_eq!(c.limit(), MIN_BPS as u32);
    }

    #[test]
    fn the_same_limit_is_resent_each_second() {
        let mut c = VideoRateControl::default();
        run(&mut c, 0, 10, |_| 3_000, |_, _| 45e6, |_| 0);
        run(&mut c, 10, 2, |_| 30_000, |_, _| 30e6, |_| 0);
        // Held by a worried delay: no change, but a resend every second.
        let sent = run(&mut c, 11, 30, |_| 7_000 + 3_000, |_, _| 25e6, |_| 0);
        assert!((2..=4).contains(&sent.len()), "{sent:?}");
        assert!(sent.iter().all(|&b| b == 25_500_000), "{sent:?}");
    }
}

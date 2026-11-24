//! Wi-Fi delay spikes, and their likely cause (M4 step 5, SPEC §6
//! "Background scans and power save cause periodic spikes").
//!
//! The viewer feeds each frame's *first-packet* network time (independent
//! of the frame's size). A spike is a stretch of frames more than
//! [`SPIKE_OVER_US`] over the typical time. Spikes within
//! [`SPIKE_MERGE_US`] of each other are one event. If the gaps between
//! events mostly agree (allowing for missed ones: 2× and 3× count too), the
//! spikes are periodic, and the period names a likely cause:
//! - about every second: a Mac's AWDL (AirDrop, Handoff, Sidecar) hopping
//!   channel;
//! - about every minute: a background Wi-Fi scan (Windows WLAN AutoConfig;
//!   Macs scan too);
//! - a tenth to half a second: Wi-Fi power save waking on the router's
//!   beacons.
//!
//! Glass only names it; it never changes a setting (SPEC: opt-in only).
//! No allocation after construction.

/// How far over the typical network time a frame must be to count (µs).
pub const SPIKE_OVER_US: u32 = 8_000;
/// Spikes closer than this are one event (µs).
pub const SPIKE_MERGE_US: u64 = 150_000;
const EVENTS: usize = 64;

/// What the spikes look like, for the results (`repr(C)` for the Mac).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct SpikeSummary {
    /// Spike events, and the worst one over the typical time (µs).
    pub count: u32,
    pub max_over_us: u32,
    /// The typical (first-packet) network time (µs).
    pub typical_us: u32,
    /// The period if periodic (ms), else 0.
    pub period_ms: u32,
    /// `CAUSE_*`.
    pub cause: u8,
}

pub const CAUSE_NONE: u8 = 0;
/// Spikes with no regular pattern: interference, distance, other traffic.
pub const CAUSE_IRREGULAR: u8 = 1;
/// About every second: AWDL on a Mac.
pub const CAUSE_AWDL: u8 = 2;
/// About every minute: a background Wi-Fi scan.
pub const CAUSE_SCAN: u8 = 3;
/// A tenth to half a second: Wi-Fi power save.
pub const CAUSE_POWER_SAVE: u8 = 4;
/// Regular, but at none of the known periods.
pub const CAUSE_PERIODIC: u8 = 5;

pub struct SpikeDetector {
    base: Option<f64>,
    in_spike: bool,
    last_spike_us: u64,
    /// Start times of the last events (a ring).
    events: [u64; EVENTS],
    n: usize,
    count: u32,
    max_over: u32,
}

impl Default for SpikeDetector {
    fn default() -> Self {
        Self {
            base: None,
            in_spike: false,
            last_spike_us: 0,
            events: [0; EVENTS],
            n: 0,
            count: 0,
            max_over: 0,
        }
    }
}

impl SpikeDetector {
    /// A frame's first packet arrived at `now_us`, `d_us` after the host
    /// sent it.
    pub fn feed(&mut self, now_us: u64, d_us: u32) {
        let d = d_us as f64;
        let base = *self.base.get_or_insert(d);
        let over = d - base;
        if over > SPIKE_OVER_US as f64 {
            self.max_over = self.max_over.max(over as u32);
            if !self.in_spike && now_us.saturating_sub(self.last_spike_us) > SPIKE_MERGE_US {
                self.count += 1;
                self.events[self.n % EVENTS] = now_us;
                self.n += 1;
            }
            self.in_spike = true;
            self.last_spike_us = now_us;
        } else {
            self.in_spike = false;
            // The typical time follows calm frames only, slowly, and drops
            // at once to a new low (its floor is the real path).
            let w = if d < base { 0.2 } else { 0.02 };
            self.base = Some(base + (d - base) * w);
        }
    }

    pub fn summary(&self) -> SpikeSummary {
        let (period_ms, cause) = self.classify();
        SpikeSummary {
            count: self.count,
            max_over_us: self.max_over,
            typical_us: self.base.unwrap_or(0.0) as u32,
            period_ms,
            cause,
        }
    }

    fn classify(&self) -> (u32, u8) {
        let k = self.n.min(EVENTS);
        if k == 0 {
            return (0, CAUSE_NONE);
        }
        if k < 5 {
            return (0, CAUSE_IRREGULAR);
        }
        // Gaps between consecutive events, oldest first (no allocation).
        let mut gaps = [0u64; EVENTS];
        let first = self.n - k;
        for i in 1..k {
            gaps[i - 1] = self.events[(first + i) % EVENTS] - self.events[(first + i - 1) % EVENTS];
        }
        let gaps = &mut gaps[..k - 1];
        // The period: the shortest gap that most gaps are a multiple of.
        let mut sorted = [0u64; EVENTS];
        sorted[..gaps.len()].copy_from_slice(gaps);
        let sorted = &mut sorted[..gaps.len()];
        sorted.sort_unstable();
        for &p in sorted.iter() {
            if p == 0 {
                continue;
            }
            let fits = gaps
                .iter()
                .filter(|&&g| {
                    let m = (g as f64 / p as f64).round();
                    (1.0..=3.0).contains(&m) && ((g as f64 - m * p as f64).abs() <= 0.15 * p as f64)
                })
                .count();
            if fits * 10 >= gaps.len() * 7 {
                // Refine: the mean of the single-period gaps.
                let (sum, n) = gaps
                    .iter()
                    .filter(|&&g| (g as f64 - p as f64).abs() <= 0.15 * p as f64)
                    .fold((0u64, 0u64), |(s, n), &g| (s + g, n + 1));
                let period = sum.checked_div(n).unwrap_or(p);
                let ms = (period / 1_000) as u32;
                let cause = match ms {
                    700..=1_400 => CAUSE_AWDL,
                    40_000..=80_000 => CAUSE_SCAN,
                    90..=550 => CAUSE_POWER_SAVE,
                    _ => CAUSE_PERIODIC,
                };
                return (ms, cause);
            }
        }
        (0, CAUSE_IRREGULAR)
    }
}

/// The results line's text for a summary (same wording on the Mac, which
/// gets it through `glass_spike_text`).
pub fn describe(s: &SpikeSummary) -> String {
    if s.count == 0 {
        return format!(
            "Wi-Fi spikes (M4): none over {} ms above the typical {:.1} ms",
            SPIKE_OVER_US / 1_000,
            s.typical_us as f64 / 1e3
        );
    }
    let why = match s.cause {
        CAUSE_AWDL => format!(
            "about every {:.1} s: typical of a Mac's AWDL (AirDrop, Handoff, Sidecar) on either machine",
            s.period_ms as f64 / 1e3
        ),
        CAUSE_SCAN => format!(
            "about every {:.0} s: typical of a background Wi-Fi scan (Windows WLAN AutoConfig, or a Mac's)",
            s.period_ms as f64 / 1e3
        ),
        CAUSE_POWER_SAVE => format!(
            "about every {} ms: typical of Wi-Fi power save (the adapter's power setting)",
            s.period_ms
        ),
        CAUSE_PERIODIC => format!("regular, about every {:.2} s: cause unknown", s.period_ms as f64 / 1e3),
        _ => "no regular pattern: interference, distance or other traffic".into(),
    };
    format!(
        "Wi-Fi spikes (M4): {} over {} ms above the typical {:.1} ms (worst {:.0} ms over) | {}",
        s.count,
        SPIKE_OVER_US / 1_000,
        s.typical_us as f64 / 1e3,
        s.max_over_us as f64 / 1e3,
        why
    )
}

/// One look at this machine's Wi-Fi link (fed about once a second by the
/// app: the WLAN API on Windows, CoreWLAN on the Mac). Unknown fields are 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct LinkSample {
    /// Connected over Wi-Fi at all (false: wired, or Wi-Fi off).
    pub wifi: bool,
    /// Signal strength (dBm, negative), and Windows' 0–100 quality.
    pub rssi_dbm: i32,
    pub quality_pct: u32,
    /// The link's transmit rate (Mbps).
    pub rate_mbps: u32,
    pub channel: u32,
    /// The access point (all zero if the OS won't say).
    pub bssid: [u8; 6],
}

/// A session's Wi-Fi link: the latest and worst signal and rate, roams
/// (the access point changed) and channel changes. No allocation.
#[derive(Debug, Default)]
pub struct LinkTrack {
    samples: u32,
    wifi: u32,
    last: LinkSample,
    rssi_min: Option<i32>,
    quality_min: Option<u32>,
    rate_min: Option<u32>,
    roams: u32,
    channel_changes: u32,
}

impl LinkTrack {
    pub fn feed(&mut self, s: &LinkSample) {
        self.samples += 1;
        if !s.wifi {
            return;
        }
        if self.wifi > 0 {
            let known = |b: &[u8; 6]| b.iter().any(|&x| x != 0);
            if known(&s.bssid) && known(&self.last.bssid) && s.bssid != self.last.bssid {
                self.roams += 1;
            }
            if s.channel != 0 && self.last.channel != 0 && s.channel != self.last.channel {
                self.channel_changes += 1;
            }
        }
        self.wifi += 1;
        if s.rssi_dbm != 0 {
            self.rssi_min = Some(self.rssi_min.map_or(s.rssi_dbm, |m| m.min(s.rssi_dbm)));
        }
        if s.quality_pct != 0 {
            self.quality_min = Some(
                self.quality_min
                    .map_or(s.quality_pct, |m| m.min(s.quality_pct)),
            );
        }
        if s.rate_mbps != 0 {
            self.rate_min = Some(self.rate_min.map_or(s.rate_mbps, |m| m.min(s.rate_mbps)));
        }
        self.last = *s;
    }

    /// The results line (the same on both platforms).
    pub fn describe(&self) -> String {
        if self.samples == 0 {
            return "Wi-Fi here: not measured".into();
        }
        if self.wifi == 0 {
            return "Wi-Fi here: none (a cable to the network; a mesh node or Wi-Fi beyond it isn't visible from here)".into();
        }
        let l = &self.last;
        let mut parts = Vec::new();
        if let Some(m) = self.rssi_min {
            parts.push(format!("signal {} dBm (worst {m})", l.rssi_dbm));
        }
        if let Some(m) = self.quality_min {
            parts.push(format!("quality {}% (worst {m}%)", l.quality_pct));
        }
        if let Some(m) = self.rate_min {
            parts.push(format!("rate {} Mbps (lowest {m})", l.rate_mbps));
        }
        if l.channel != 0 {
            parts.push(format!("channel {}", l.channel));
        }
        parts.push(format!(
            "roams {} | channel changes {}",
            self.roams, self.channel_changes
        ));
        format!("Wi-Fi here: {}", parts.join(" | "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 60 fps for `secs`, 3 ms typical with ±0.5 ms jitter, and a 20 ms
    /// spike of 3 frames every `period_us` (some missed if `skip`).
    fn run(secs: u64, period_us: Option<u64>, skip: bool) -> SpikeSummary {
        let mut d = SpikeDetector::default();
        let mut x: u32 = 0x9E37_79B9;
        let mut next = period_us.map(|p| p / 2);
        let mut i = 0;
        let mut t = 0u64;
        while t < secs * 1_000_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let mut v = 3_000 + (x % 1_000);
            if let (Some(n), Some(p)) = (next, period_us) {
                if t >= n {
                    if !(skip && i % 4 == 3) {
                        v += 20_000;
                    }
                    if t >= n + 50_000 {
                        next = Some(n + p);
                        i += 1;
                    }
                }
            }
            d.feed(t, v);
            t += 16_667;
        }
        d.summary()
    }

    #[test]
    fn calm_wifi_has_no_spikes() {
        let s = run(60, None, false);
        assert_eq!((s.count, s.cause), (0, CAUSE_NONE));
        assert!((3_000..4_000).contains(&s.typical_us), "{s:?}");
    }

    #[test]
    fn every_second_is_awdl() {
        let s = run(30, Some(1_000_000), false);
        assert_eq!(s.cause, CAUSE_AWDL, "{s:?}");
        assert!((950..=1_050).contains(&s.period_ms), "{s:?}");
        assert!(s.max_over_us >= 15_000);
    }

    #[test]
    fn missed_spikes_still_show_the_period() {
        let s = run(30, Some(1_000_000), true);
        assert_eq!(s.cause, CAUSE_AWDL, "{s:?}");
    }

    #[test]
    fn every_minute_is_a_scan() {
        let s = run(400, Some(60_000_000), false);
        assert_eq!(s.cause, CAUSE_SCAN, "{s:?}");
    }

    #[test]
    fn a_fifth_of_a_second_is_power_save() {
        let s = run(20, Some(204_800), false);
        assert_eq!(s.cause, CAUSE_POWER_SAVE, "{s:?}");
    }

    #[test]
    fn random_spikes_are_irregular() {
        let mut d = SpikeDetector::default();
        let mut x: u32 = 12345;
        let mut t = 0u64;
        while t < 120_000_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let v = if x.is_multiple_of(200) { 30_000 } else { 3_000 };
            d.feed(t, v);
            t += 16_667;
        }
        let s = d.summary();
        assert!(s.count >= 5, "{s:?}");
        assert_eq!(s.cause, CAUSE_IRREGULAR, "{s:?}");
        assert!(describe(&s).contains("no regular pattern"));
    }

    #[test]
    fn link_track_counts_roams_and_the_worst() {
        let mut t = LinkTrack::default();
        assert_eq!(t.describe(), "Wi-Fi here: not measured");
        let mut s = LinkSample {
            wifi: true,
            rssi_dbm: -55,
            quality_pct: 0,
            rate_mbps: 866,
            channel: 36,
            bssid: [1, 2, 3, 4, 5, 6],
        };
        t.feed(&s);
        s.rssi_dbm = -70;
        s.rate_mbps = 300;
        s.bssid = [1, 2, 3, 4, 5, 7];
        s.channel = 149;
        t.feed(&s);
        s.rssi_dbm = -58;
        s.rate_mbps = 780;
        t.feed(&s);
        let d = t.describe();
        assert!(d.contains("signal -58 dBm (worst -70)"), "{d}");
        assert!(d.contains("rate 780 Mbps (lowest 300)"), "{d}");
        assert!(d.contains("roams 1 | channel changes 1"), "{d}");
        let mut w = LinkTrack::default();
        w.feed(&LinkSample::default());
        assert_eq!(w.describe(), "Wi-Fi here: none (a cable to the network; a mesh node or Wi-Fi beyond it isn't visible from here)");
    }
}

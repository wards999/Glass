//! The scorecard (ADR 0033): a fixed test scene the host streams instead of
//! its screen, and the viewer's scoring of it against SPEC §0's targets.
//! Same content and timing every run, so builds compare by numbers.
//!
//! Timeline (scene time, from when the host made the scene):
//! - warm-up: card 0 and a barcode of the scene's age in ms (a new frame
//!   every refresh), from which the viewer finds scene time 0 on its own
//!   clock (input packets can't start it: heartbeats send them anyway);
//! - [`CARDS`] changes of a text-heavy grey test card, each
//!   held [`CARD_US`]: how long until the viewer's picture matches it to
//!   [`SHARP_DB`] (target: [`SHARP_TARGET_US`]);
//! - [`MOTION_US`] of the card scrolling [`SCROLL_PX`] a frame, one frame per
//!   viewer refresh: refreshes without a new picture, interval spread;
//! - [`LATENCY_US`] of a still card with a marker that changes with every
//!   input: the viewer presses or lets go of F24 every [`INPUT_EVERY_US`]
//!   (an event, which a host counts as input that changes the picture; a
//!   pointer move alone doesn't) and times input -> screen.
//!
//! The card is grey (R = G = B), so its Y is the same in BT.601 and BT.709
//! and the viewer can compare decoded luma with the original exactly.

use crate::stats::StatRing;

pub const WARMUP_US: u64 = 3_500_000;
/// The barcode (warm-up only): 16 bits of the scene's age in ms, one
/// `BAR_CELL` square each, black = 1, top left.
pub const BAR_BITS: u32 = 16;
pub const BAR_CELL: u32 = 16;
pub const CARDS: usize = 6;
pub const CARD_US: u64 = 2_000_000;
pub const MOTION_START_US: u64 = WARMUP_US + CARDS as u64 * CARD_US;
pub const MOTION_US: u64 = 10_000_000;
pub const LATENCY_START_US: u64 = MOTION_START_US + MOTION_US;
pub const LATENCY_US: u64 = 10_000_000;
pub const END_US: u64 = LATENCY_START_US + LATENCY_US;
/// Rows the card moves per frame while scrolling.
pub const SCROLL_PX: u32 = 3;
/// "Visually lossless" (SPEC §0).
pub const SHARP_DB: f64 = 45.0;
pub const SHARP_TARGET_US: u64 = 150_000;
pub const INPUT_EVERY_US: u64 = 100_000;
/// The marker square (top left) that answers input in the delay part.
pub const MARKER: u32 = 64;
/// F24's HID usage: the delay part's key.
pub const DELAY_KEY: u8 = 0x73;
/// Motion and delay samples skip this much at the start of their part.
const SETTLE_US: u64 = 500_000;

/// What the scene shows at scene time `t_us`, for a viewer at `fps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Card 0 during warm-up, then cards 1..=CARDS.
    Still {
        card: usize,
    },
    /// The `frame`th frame of the scroll.
    Motion {
        frame: u64,
    },
    Latency,
    Done,
}

pub fn phase_at(t_us: u64, fps: u32) -> Phase {
    if t_us < WARMUP_US {
        Phase::Still { card: 0 }
    } else if t_us < MOTION_START_US {
        Phase::Still {
            card: 1 + ((t_us - WARMUP_US) / CARD_US) as usize,
        }
    } else if t_us < LATENCY_START_US {
        Phase::Motion {
            frame: (t_us - MOTION_START_US) * fps.max(1) as u64 / 1_000_000,
        }
    } else if t_us < END_US {
        Phase::Latency
    } else {
        Phase::Done
    }
}

/// Scene time of the next change after `t_us` (when a host showing it
/// needs a new frame without input), or `None` when it's over.
pub fn next_change_us(t_us: u64, fps: u32) -> Option<u64> {
    let frame_us = 1_000_000 / fps.max(1) as u64;
    match phase_at(t_us, fps) {
        // The barcode changes every frame.
        Phase::Still { card: 0 } => Some((t_us + frame_us).min(WARMUP_US)),
        Phase::Still { card } => Some(WARMUP_US + card as u64 * CARD_US),
        Phase::Motion { frame } => {
            let next = MOTION_START_US + (frame + 1) * 1_000_000 / fps.max(1) as u64;
            Some(next.min(LATENCY_START_US))
        }
        Phase::Latency => Some(END_US),
        Phase::Done => None,
    }
}

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

const INK: u8 = 24;
const PAPER: u8 = 232;

/// Limited-range (video) Y of grey level `v`.
fn video_y(v: u8) -> u8 {
    (16 + (v as u32 * 219 + 127) / 255) as u8
}

/// The test card: `height` rows that repeat (the scroll wraps), grey.
pub struct Scene {
    pub width: u32,
    pub height: u32,
    gray: Vec<u8>,
    y_of: [u8; 256],
}

impl Scene {
    pub fn new(width: u32, height: u32) -> Self {
        let (w, h) = (width.max(16) as usize, height.max(16) as usize);
        let mut gray = vec![PAPER; w * h];
        for y in 0..h {
            let row = &mut gray[y * w..(y + 1) * w];
            for (x, px) in row.iter_mut().enumerate() {
                *px = Self::pixel(x, y, w, h);
            }
        }
        let mut y_of = [0u8; 256];
        for (v, out) in y_of.iter_mut().enumerate() {
            *out = video_y(v as u8);
        }
        Self {
            width: w as u32,
            height: h as u32,
            gray,
            y_of,
        }
    }

    /// Three quarters small text (6x12 cells, 1-pixel strokes), then fine
    /// line patterns, then a photo-like gradient with grain.
    fn pixel(x: usize, y: usize, w: usize, h: usize) -> u8 {
        let band = x * 8 / w;
        if band < 6 {
            let (cx, cy, gx, gy) = (x / 6, y / 12, x % 6, y % 12);
            if gx == 5 || gy >= 9 {
                return PAPER;
            }
            let cell = splitmix((cx as u64) << 32 | cy as u64);
            // Word gaps, and every 18th line a blank one (paragraphs).
            if cell.is_multiple_of(7) || cy % 18 == 17 {
                return PAPER;
            }
            let bit = gy * 5 + gx;
            let ink = (cell >> bit) & 1 == 1 && (splitmix(cell) >> bit) & 1 == 1;
            if ink {
                INK
            } else {
                PAPER
            }
        } else if band == 6 {
            let q = y * 4 / h;
            let on = match q {
                0 => (x + y).is_multiple_of(2),
                1 => x.is_multiple_of(2),
                2 => y.is_multiple_of(2),
                _ => (x + y) % 4 < 2,
            };
            if on {
                INK
            } else {
                PAPER
            }
        } else {
            let base = 40 + ((x * 3 + y * 2) % 512) as i32 * 170 / 512;
            let grain = (splitmix((x as u64) << 32 | y as u64) % 41) as i32 - 20;
            (base + grain).clamp(0, 255) as u8
        }
    }

    /// The card's offset (rows) for a phase: each still card is a different
    /// part of it, the scroll moves through it.
    pub fn offset(&self, phase: Phase) -> u32 {
        match phase {
            Phase::Still { card } => (card as u32 * 389 + (card * card) as u32 * 61) % self.height,
            Phase::Motion { frame } => {
                ((frame % self.height as u64) as u32 * SCROLL_PX) % self.height
            }
            Phase::Latency | Phase::Done => 0,
        }
    }

    /// Grey row `y` of the picture at `offset`.
    pub fn row(&self, offset: u32, y: u32) -> &[u8] {
        let w = self.width as usize;
        let r = ((y + offset) % self.height) as usize;
        &self.gray[r * w..(r + 1) * w]
    }

    /// The card twice over as BGRA (`2 * height` rows, `4 * width` bytes
    /// each): a host copies `height` rows from any offset with one GPU copy.
    pub fn tall_bgra(&self) -> Vec<u8> {
        let w = self.width as usize;
        let mut out = Vec::with_capacity(w * 4 * self.height as usize * 2);
        for _ in 0..2 {
            for y in 0..self.height {
                for &v in self.row(0, y) {
                    out.extend_from_slice(&[v, v, v, 255]);
                }
            }
        }
        out
    }

    /// The barcode's grey squares for `age_ms` (BAR_BITS squares of
    /// BAR_CELL pixels in a row, bit 0 leftmost): `BAR_BITS * BAR_CELL`
    /// wide, `BAR_CELL` tall.
    pub fn barcode_gray(age_ms: u16) -> Vec<u8> {
        let (w, h) = ((BAR_BITS * BAR_CELL) as usize, BAR_CELL as usize);
        let mut out = vec![PAPER; w * h];
        for y in 0..h {
            for x in 0..w {
                if (age_ms >> (x / BAR_CELL as usize)) & 1 == 1 {
                    out[y * w + x] = INK;
                }
            }
        }
        out
    }

    /// Reads the barcode from decoded limited-range luma, if it's there
    /// (every square clearly dark or light).
    pub fn read_barcode(y_plane: &[u8], stride: usize) -> Option<u16> {
        let mut v = 0u16;
        let c = BAR_CELL as usize;
        let (ink, paper) = (video_y(INK) as u32, video_y(PAPER) as u32);
        for bit in 0..BAR_BITS as usize {
            let mut sum = 0u32;
            for y in c / 4..c * 3 / 4 {
                for x in bit * c + c / 4..bit * c + c * 3 / 4 {
                    sum += *y_plane.get(y * stride + x)? as u32;
                }
            }
            let mean = sum / ((c / 2) * (c / 2)) as u32;
            if mean < ink + 30 {
                v |= 1 << bit;
            } else if mean <= paper - 30 {
                return None;
            }
        }
        Some(v)
    }

    /// What a host shows at scene time `t_us` for a viewer at `fps`:
    /// (offset, marker, barcode), or `None` once the scene is over. A host
    /// draws a frame whenever this changes; `marker` is its input parity.
    pub fn content(
        &self,
        t_us: u64,
        fps: u32,
        marker: bool,
    ) -> Option<(u32, Option<bool>, Option<u16>)> {
        let phase = phase_at(t_us, fps);
        if phase == Phase::Done {
            return None;
        }
        Some((
            self.offset(phase),
            (phase == Phase::Latency).then_some(marker),
            (phase == Phase::Still { card: 0 })
                .then_some((t_us / 1000).min(u16::MAX as u64) as u16),
        ))
    }

    /// The marker's grey for the input count's parity.
    pub fn marker_gray(odd: bool) -> u8 {
        if odd {
            INK
        } else {
            PAPER
        }
    }

    /// Limited-range luma of the picture at `offset` (with the marker if
    /// `marker` is set), into `out` rows of `stride` bytes: for hosts whose
    /// encoder takes YCbCr (the Mac).
    pub fn fill_video_y(
        &self,
        offset: u32,
        marker: Option<bool>,
        barcode: Option<u16>,
        out: &mut [u8],
        stride: usize,
    ) {
        let w = self.width as usize;
        let bar = barcode.map(Self::barcode_gray);
        let bw = (BAR_BITS * BAR_CELL) as usize;
        for y in 0..self.height {
            let dst = &mut out[y as usize * stride..y as usize * stride + w];
            for (d, &v) in dst.iter_mut().zip(self.row(offset, y)) {
                *d = self.y_of[v as usize];
            }
            if let Some(odd) = marker {
                if y < MARKER {
                    let m = self.y_of[Self::marker_gray(odd) as usize];
                    dst[..MARKER as usize].fill(m);
                }
            }
            if let Some(bar) = &bar {
                if y < BAR_CELL && bw <= w {
                    let src = &bar[y as usize * bw..(y as usize + 1) * bw];
                    for (d, &v) in dst[..bw].iter_mut().zip(src) {
                        *d = self.y_of[v as usize];
                    }
                }
            }
        }
    }

    /// PSNR (dB) of decoded limited-range luma (`stride` bytes a row) against
    /// the picture at `offset`; 99 if identical.
    pub fn psnr_y(&self, offset: u32, y_plane: &[u8], stride: usize) -> f64 {
        let w = self.width as usize;
        let mut sum: u64 = 0;
        for y in 0..self.height {
            let got = &y_plane[y as usize * stride..y as usize * stride + w];
            for (&g, &v) in got.iter().zip(self.row(offset, y)) {
                let d = g as i32 - self.y_of[v as usize] as i32;
                sum += (d * d) as u64;
            }
        }
        if sum == 0 {
            return 99.0;
        }
        let mse = sum as f64 / (w * self.height as usize) as f64;
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// One still card's result.
#[derive(Debug, Clone, Copy, Default)]
struct Card {
    /// Capture time (viewer clock) of its first frame.
    change_us: u64,
    first_db: f64,
    best_db: f64,
    sharp_after_us: Option<u64>,
}

/// The viewer's side: fed decoded pictures and presents, scores at the end.
pub struct Scorecard {
    pub scene: Scene,
    fps: u32,
    /// Scene time 0 on the viewer's clock: the earliest of capture time
    /// minus the barcode's age (a frame captured later than it was drawn
    /// only overestimates).
    t0: Option<u64>,
    cards: [Card; CARDS + 1],
    /// Capture time -> luma PSNR of decoded still pictures (a few dozen).
    psnr: Vec<(u64, f64)>,
    motion_shown: Vec<u64>,
    latency: StatRing,
    next_input_us: u64,
    flip: bool,
}

impl Scorecard {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            scene: Scene::new(width, height),
            fps: fps.max(1),
            t0: None,
            cards: [Card::default(); CARDS + 1],
            psnr: Vec::with_capacity(256),
            motion_shown: Vec::with_capacity(4096),
            latency: StatRing::new(),
            next_input_us: 0,
            flip: false,
        }
    }

    pub fn is_started(&self) -> bool {
        self.t0.is_some()
    }

    /// Scene time of a frame captured at `capture_us` (viewer clock).
    pub fn scene_time(&self, capture_us: u64) -> Option<u64> {
        self.t0
            .filter(|&t0| capture_us >= t0)
            .map(|t0| capture_us - t0)
    }

    /// Whether to look at this decoded picture: the warm-up (its barcode)
    /// and the still cards.
    pub fn wants_picture(&self, capture_us: u64) -> bool {
        matches!(
            self.scene_time(capture_us).map(|t| phase_at(t, self.fps)),
            None | Some(Phase::Still { .. })
        )
    }

    /// A decoded picture's luma (one `wants_picture` asked for).
    pub fn picture(&mut self, capture_us: u64, y_plane: &[u8], stride: usize) {
        if capture_us == 0 {
            return;
        }
        let warming = self.scene_time(capture_us).is_none_or(|t| t < WARMUP_US);
        if let Some(age) = Scene::read_barcode(y_plane, stride).filter(|_| warming) {
            let est = capture_us.saturating_sub(age as u64 * 1000);
            if self.t0.is_none_or(|t0| est < t0) {
                self.t0 = Some(est);
            }
            return;
        }
        let Some(t) = self.scene_time(capture_us) else {
            return;
        };
        let Phase::Still { card } = phase_at(t, self.fps) else {
            return;
        };
        let db = self
            .scene
            .psnr_y(self.scene.offset(Phase::Still { card }), y_plane, stride);
        let c = &mut self.cards[card];
        if c.change_us == 0 {
            c.change_us = capture_us;
            c.first_db = db;
        }
        c.best_db = c.best_db.max(db);
        if self.psnr.len() < 1024 {
            self.psnr.push((capture_us, db));
        }
    }

    /// A present of the frame captured at `capture_us` reached the screen
    /// at `shown_us`; `input_us`: when its input was sent, if it answered one.
    pub fn presented(&mut self, capture_us: u64, shown_us: u64, input_us: Option<u64>) {
        let Some(t) = self.scene_time(capture_us) else {
            return;
        };
        match phase_at(t, self.fps) {
            Phase::Still { card } if card > 0 => {
                let db = self
                    .psnr
                    .iter()
                    .find(|p| p.0 == capture_us)
                    .map(|p| p.1)
                    .unwrap_or(0.0);
                let c = &mut self.cards[card];
                if c.sharp_after_us.is_none() && c.change_us > 0 && db >= SHARP_DB {
                    c.sharp_after_us = Some(shown_us.saturating_sub(c.change_us));
                }
            }
            Phase::Motion { .. } if t >= MOTION_START_US + SETTLE_US => {
                self.motion_shown.push(shown_us)
            }
            Phase::Latency if t >= LATENCY_START_US + SETTLE_US => {
                if let Some(sent) = input_us {
                    self.latency
                        .push(shown_us.saturating_sub(sent).min(u32::MAX as u64) as u32);
                }
            }
            _ => {}
        }
    }

    /// Viewer clock now: whether to press (`true`) or let go of F24 (the
    /// delay part); nothing on either OS answers it.
    pub fn input_due(&mut self, now_us: u64) -> Option<bool> {
        let t0 = self.t0?;
        let t = now_us.checked_sub(t0)?;
        if !(LATENCY_START_US + 200_000..END_US - 200_000).contains(&t)
            || now_us < self.next_input_us
        {
            return None;
        }
        self.next_input_us = now_us + INPUT_EVERY_US;
        self.flip = !self.flip;
        Some(self.flip)
    }

    /// The scene is over (viewer clock now), with time to show the last.
    pub fn finished(&self, now_us: u64) -> bool {
        self.t0.is_some_and(|t0| now_us >= t0 + END_US + 1_000_000)
    }

    /// The scorecard: one line per target, PASS or FAIL.
    pub fn report(&self, what: &str) -> String {
        let mut out = format!(
            "scorecard (ADR 0033): {what} | test scene {}x{} @ {} Hz | targets: SPEC section 0\n",
            self.scene.width, self.scene.height, self.fps
        );
        if self.t0.is_none() {
            out.push_str("  not run: no test scene came (the host may not support it)\n");
            return out;
        }
        // Sharpness.
        let cards: Vec<&Card> = self.cards[1..].iter().filter(|c| c.change_us > 0).collect();
        let mut sharp: Vec<u64> = cards.iter().filter_map(|c| c.sharp_after_us).collect();
        sharp.sort_unstable();
        let never = cards.len() - sharp.len();
        let worst_best = cards
            .iter()
            .map(|c| c.best_db)
            .fold(f64::INFINITY, f64::min);
        let mut first: Vec<f64> = cards.iter().map(|c| c.first_db).collect();
        first.sort_by(|a, b| a.total_cmp(b));
        let pass = !cards.is_empty() && never == 0 && sharp.iter().all(|&s| s <= SHARP_TARGET_US);
        out.push_str(&format!(
            "  sharpness: {} of {} changes reached {:.0} dB{} | first frame p50 {:.1} dB, best of the worst {:.1} dB (target: {:.0} dB within {} ms) {}\n",
            sharp.len(),
            cards.len(),
            SHARP_DB,
            if sharp.is_empty() {
                String::new()
            } else {
                format!(
                    ", after p50 {:.0} ms, worst {:.0} ms",
                    sharp[sharp.len() / 2] as f64 / 1e3,
                    sharp[sharp.len() - 1] as f64 / 1e3
                )
            },
            first.get(first.len() / 2).copied().unwrap_or(0.0),
            if worst_best.is_finite() { worst_best } else { 0.0 },
            SHARP_DB,
            SHARP_TARGET_US / 1000,
            if pass { "PASS" } else { "FAIL" }
        ));
        // Smoothness.
        let period = 1e6 / self.fps as f64;
        let mut missed = 0u64;
        let mut refreshes = 0u64;
        let mut intervals: Vec<f64> = Vec::with_capacity(self.motion_shown.len());
        for w in self.motion_shown.windows(2) {
            let d = w[1].saturating_sub(w[0]) as f64;
            let n = (d / period).round().max(1.0) as u64;
            refreshes += n;
            missed += n - 1;
            intervals.push(d);
        }
        if intervals.is_empty() {
            out.push_str("  smoothness: no frames shown while scrolling FAIL\n");
        } else {
            let mean = intervals.iter().sum::<f64>() / intervals.len() as f64;
            let sd = (intervals.iter().map(|d| (d - mean).powi(2)).sum::<f64>()
                / intervals.len() as f64)
                .sqrt();
            let mut sorted = intervals.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let p99 = sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)];
            let per_1000 = missed as f64 * 1000.0 / refreshes.max(1) as f64;
            let pass = per_1000 <= 1.0 && sd < 1000.0 && p99 < 1.5 * period;
            out.push_str(&format!(
                "  smoothness: {refreshes} refreshes while scrolling, {missed} without a new picture ({per_1000:.1} per 1000; target <= 1) | interval sd {:.2} ms (target < 1), p99 {:.2} refreshes (target < 1.5) {}\n",
                sd / 1e3,
                p99 / period,
                if pass { "PASS" } else { "FAIL" }
            ));
        }
        // Delay.
        let s = self.latency.summary();
        if s.total == 0 {
            out.push_str("  delay: no answers to input FAIL\n");
        } else {
            let pass = s.p50_us <= 30_000 && s.p99_us <= 60_000;
            out.push_str(&format!(
                "  delay: input -> screen p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms (n={}; target p50 <= 30, p99 <= 60) {}\n",
                s.p50_us as f64 / 1e3,
                s.p99_us as f64 / 1e3,
                s.max_us as f64 / 1e3,
                s.total,
                if pass { "PASS" } else { "FAIL" }
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_covers_each_part_in_order() {
        assert_eq!(phase_at(0, 60), Phase::Still { card: 0 });
        assert_eq!(phase_at(WARMUP_US, 60), Phase::Still { card: 1 });
        assert_eq!(
            phase_at(MOTION_START_US - 1, 60),
            Phase::Still { card: CARDS }
        );
        assert_eq!(
            phase_at(MOTION_START_US + 1_000_000, 60),
            Phase::Motion { frame: 60 }
        );
        assert_eq!(phase_at(LATENCY_START_US, 60), Phase::Latency);
        assert_eq!(phase_at(END_US, 60), Phase::Done);
        assert_eq!(next_change_us(0, 60), Some(16_666));
        assert_eq!(next_change_us(WARMUP_US, 60), Some(WARMUP_US + CARD_US));
        assert_eq!(next_change_us(END_US, 60), None);
    }

    #[test]
    fn exact_luma_scores_99_and_a_level_off_about_48() {
        let s = Scene::new(64, 48);
        let off = s.offset(Phase::Still { card: 2 });
        let mut y = vec![0u8; 64 * 48];
        s.fill_video_y(off, None, None, &mut y, 64);
        assert_eq!(s.psnr_y(off, &y, 64), 99.0);
        for v in y.iter_mut() {
            *v = v.saturating_add(1);
        }
        let db = s.psnr_y(off, &y, 64);
        assert!((47.5..48.5).contains(&db), "{db}");
        // Another card is far off.
        assert!(s.psnr_y(s.offset(Phase::Still { card: 3 }), &y, 64) < 20.0);
    }

    #[test]
    fn scores_a_run() {
        let fps = 100;
        let (w, h) = (320, 48);
        let mut sc = Scorecard::new(w, h, fps);
        let t0 = 1_000_000u64;
        let mut y = vec![0u8; (w * h) as usize];
        // Warm-up frames: one captured 3 ms after it was drawn, one on time.
        let warm = sc.scene.offset(Phase::Still { card: 0 });
        sc.scene
            .fill_video_y(warm, None, Some(1200), &mut y, w as usize);
        sc.picture(t0 + 1_203_000, &y, w as usize);
        sc.scene
            .fill_video_y(warm, None, Some(1500), &mut y, w as usize);
        sc.picture(t0 + 1_500_000, &y, w as usize);
        assert_eq!(sc.scene_time(t0 + 2_000_000), Some(2_000_000));
        // Each card: a blurry first frame, then the exact picture 100 ms on.
        for card in 1..=CARDS {
            let at = t0 + WARMUP_US + (card as u64 - 1) * CARD_US;
            let off = sc.scene.offset(Phase::Still { card });
            sc.scene.fill_video_y(off, None, None, &mut y, w as usize);
            let blurry: Vec<u8> = y.iter().map(|v| v.saturating_add(9)).collect();
            sc.picture(at, &blurry, w as usize);
            sc.presented(at, at + 20_000, None);
            sc.picture(at + 100_000, &y, w as usize);
            sc.presented(at + 100_000, at + 120_000, None);
        }
        // A frame every refresh while scrolling, one refresh missed.
        let mut shown = t0 + MOTION_START_US + SETTLE_US;
        for i in 0..900 {
            sc.presented(shown - 15_000, shown, None);
            shown += if i == 400 { 20_000 } else { 10_000 };
        }
        // Inputs answered in 25 ms.
        for i in 0..80 {
            let at = t0 + LATENCY_START_US + SETTLE_US + i * 100_000;
            sc.presented(at, at + 20_000, Some(at - 5_000));
        }
        let r = sc.report("test");
        assert!(
            r.contains("6 of 6 changes reached 45 dB, after p50 120 ms"),
            "{r}"
        );
        assert!(
            r.contains("sharpness") && r.lines().nth(1).unwrap().ends_with("PASS"),
            "{r}"
        );
        assert!(r.contains("1 without a new picture"), "{r}");
        assert!(r.contains("delay: input -> screen p50 25.0 ms"), "{r}");
    }
}

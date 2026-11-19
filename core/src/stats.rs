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

// LATER(M2): ring buffer of FrameTimestamps + p50/p99 and frame-interval jitter.

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
}

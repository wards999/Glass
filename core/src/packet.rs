//! Packetiser (frame -> MTU-sized UDP datagrams) and reassembler (datagrams ->
//! frames). Both are allocation-free in steady state: buffers are sized once
//! and only grow for an unusually large frame.
//!
//! A frame's logical payload is `FrameInfo` (12 bytes) followed by the
//! Annex-B access unit, cut into chunks of [`MAX_PAYLOAD`] bytes. Every chunk
//! except the last is exactly `MAX_PAYLOAD` long, so chunk `i` lives at
//! offset `i * MAX_PAYLOAD` and the receiver can place packets in any order.
//!
//! FEC (M4, ADR 0020): the data packets are split into interleaved groups
//! (packet `i` in group `i mod G`, at most [`FEC_GROUP_MAX`] a group, so a
//! burst of consecutive losses is spread over the groups). Each group gets
//! `m` Reed–Solomon parity packets (`fec.rs`, the last chunk zero-padded),
//! sent after the data. The receiver rebuilds a group's lost packets as soon
//! as it has as many of its packets as it has data packets.

use crate::fec;
use crate::protocol::{FrameInfo, VideoPacketHeader, FLAG_IDR, FLAG_RESENT, NACK_MAX};
use std::io;

/// Whole video datagram before encryption, header included. Sealed, it's
/// `seal::WIRE_MAX` (1400) bytes: fits a 1500-byte MTU with room for IP/UDP.
pub const MAX_DATAGRAM: usize = crate::seal::WIRE_MAX - crate::seal::OVERHEAD;
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM - VideoPacketHeader::SIZE;
/// At most this many data packets in one FEC group.
pub const FEC_GROUP_MAX: usize = 64;

/// What the host knows about a frame when it sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct OutFrame {
    pub frame_id: u32,
    pub flags: u8,
    pub capture_ts_us: u64,
    pub encode_us: u32,
    /// `FrameInfo::input_seq`.
    pub input_seq: u32,
}

/// Number of data datagrams a frame of `data_len` Annex-B bytes needs.
pub fn packet_count(data_len: usize) -> usize {
    (FrameInfo::SIZE + data_len).div_ceil(MAX_PAYLOAD)
}

/// A group may fail to rebuild about this rarely, at the expected loss.
const FEC_TARGET: f64 = 1e-4;

/// How a frame of `count` data packets is protected when packets are lost
/// with probability `loss` (negative: no FEC): `(groups, parity per
/// group)`. Parity is sized by probability, not as a percentage: the
/// smallest `m` such that a group of `k + m` packets loses more than `m`
/// less often than [`FEC_TARGET`], with a margin on the reported loss. Tiny
/// frames get relatively more (two of three packets lost is likely enough
/// to matter), which costs little because they're tiny. (losssim: sized as a
/// percentage, 5 % random loss still lost 0.3 % of frames.)
pub fn fec_layout(count: usize, loss: f32) -> (u8, u8) {
    if loss < 0.0 || count == 0 {
        return (0, 0);
    }
    let groups = count.div_ceil(FEC_GROUP_MAX);
    let k = count.div_ceil(groups); // the biggest group
    let p = (loss.min(0.15) as f64 * 1.5 + 0.005).min(0.5);
    // Never more than half again (3 for tiny frames), and the loss planned
    // for is capped at 15 %: when loss comes from a full link, more parity
    // only makes it fuller (m4-test1 could reach 100 % extra).
    let cap = k
        .min(fec::MAX_SHARDS - k)
        .min(((k as f64 * 0.5).ceil() as usize).max(3))
        .max(1);
    let m = (1..=cap)
        .find(|&m| binomial_tail(k + m, m, p) < FEC_TARGET)
        .unwrap_or(cap);
    (groups as u8, m as u8)
}

/// P(more than `m` of `n` independent trials succeed), each with chance `p`.
fn binomial_tail(n: usize, m: usize, p: f64) -> f64 {
    let mut term = (1.0 - p).powi(n as i32); // P(X = 0)
    let mut cdf = term;
    for x in 1..=m.min(n) {
        term *= (n - x + 1) as f64 / x as f64 * p / (1.0 - p);
        cdf += term;
    }
    (1.0 - cdf).max(0.0)
}

/// Data packets in group `g` of `groups` for a `count`-packet frame.
pub fn group_size(count: usize, groups: usize, g: usize) -> usize {
    count / groups + usize::from(g < count % groups)
}

/// Sent frames kept for resending (M4 NACK).
const HISTORY: usize = 32;
/// Frames are resent only this long after they were first sent (µs): past
/// that the viewer has given up on them anyway.
pub const RESEND_MAX_AGE_US: u64 = 150_000;

/// A sent frame, kept for resends.
#[derive(Default)]
struct Sent {
    used: bool,
    f: OutFrame,
    count: usize,
    groups: u8,
    m: u8,
    /// FrameInfo plus data (the logical payload's real length).
    total: usize,
    logical: Vec<u8>,
    at_us: u64,
}

/// Cuts frames into datagrams in reusable buffers, adding FEC, and keeps
/// the last [`HISTORY`] frames to resend packets the viewer asks for.
pub struct Packetiser {
    buf: [u8; MAX_DATAGRAM],
    /// The frame's logical payload, zero-padded to whole chunks.
    logical: Vec<u8>,
    parity: Vec<u8>,
    history: Vec<Sent>,
    /// The packet loss to protect against (negative: no FEC); set from the
    /// viewer's reports.
    pub fec_loss: f32,
}

impl Default for Packetiser {
    fn default() -> Self {
        Self {
            buf: [0; MAX_DATAGRAM],
            logical: Vec::with_capacity(1 << 20),
            parity: Vec::with_capacity(1 << 18),
            history: (0..HISTORY).map(|_| Sent::default()).collect(),
            fec_loss: FEC_LOSS_START,
        }
    }
}

fn header(
    f: &OutFrame,
    flags: u8,
    index: usize,
    count: usize,
    groups: u8,
    m: u8,
) -> VideoPacketHeader {
    VideoPacketHeader {
        stream_id: 0,
        flags,
        frame_id: f.frame_id,
        packet_index: index as u16,
        packet_count: count as u16,
        fec_groups: groups,
        fec_parity: m,
        capture_ts_us: f.capture_ts_us,
    }
}

/// The loss to protect against before the viewer has said how lossy the
/// link is.
pub const FEC_LOSS_START: f32 = 0.02;

impl Packetiser {
    /// Calls `send` once per datagram: the data packets in order, then the
    /// parity packets. Returns the number of data packets. The frame is
    /// kept (sent at `now_us`) for [`Self::resend`].
    pub fn send_frame(
        &mut self,
        f: &OutFrame,
        data: &[u8],
        now_us: u64,
        mut send: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<u16> {
        let count = packet_count(data.len());
        let (groups, m) = fec_layout(count, self.fec_loss);
        let total = FrameInfo::SIZE + data.len();
        if count + groups as usize * m as usize > u16::MAX as usize
            || data.len() > u32::MAX as usize
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame too large",
            ));
        }
        // The logical payload, whole chunks (the tail zeroed for FEC).
        self.logical.clear();
        self.logical.extend_from_slice(
            &FrameInfo {
                encode_us: f.encode_us,
                frame_len: data.len() as u32,
                input_seq: f.input_seq,
            }
            .to_bytes(),
        );
        self.logical.extend_from_slice(data);
        self.logical.resize(count * MAX_PAYLOAD, 0);
        let hs = VideoPacketHeader::SIZE;
        let header = |index: usize| header(f, f.flags, index, count, groups, m);
        for i in 0..count {
            let start = i * MAX_PAYLOAD;
            let end = (start + MAX_PAYLOAD).min(total);
            header(i)
                .encode(&mut self.buf)
                .map_err(|_| io::Error::other("header"))?;
            self.buf[hs..hs + end - start].copy_from_slice(&self.logical[start..end]);
            send(&self.buf[..hs + end - start])?;
        }
        if groups > 0 {
            let (g, m) = (groups as usize, m as usize);
            self.parity.clear();
            self.parity.resize(g * m * MAX_PAYLOAD, 0);
            for gi in 0..g {
                let k = group_size(count, g, gi);
                let par = &mut self.parity[gi * m * MAX_PAYLOAD..(gi + 1) * m * MAX_PAYLOAD];
                fec::encode_strided(&self.logical, MAX_PAYLOAD, (gi, g, k), par, m);
            }
            for p in 0..g * m {
                header(count + p)
                    .encode(&mut self.buf)
                    .map_err(|_| io::Error::other("header"))?;
                self.buf[hs..hs + MAX_PAYLOAD]
                    .copy_from_slice(&self.parity[p * MAX_PAYLOAD..(p + 1) * MAX_PAYLOAD]);
                send(&self.buf[..hs + MAX_PAYLOAD])?;
            }
        }
        // Keep it for resends: swap buffers, so nothing is copied or
        // allocated once every slot has grown to its size.
        let h = &mut self.history[f.frame_id as usize % HISTORY];
        h.used = true;
        h.f = *f;
        h.count = count;
        h.groups = groups;
        h.m = m;
        h.total = total;
        h.at_us = now_us;
        std::mem::swap(&mut h.logical, &mut self.logical);
        Ok(count as u16)
    }

    /// Sends data packets `indices` of frame `frame_id` again (every one if
    /// `indices` is empty), flagged [`FLAG_RESENT`]. Returns how many went:
    /// 0 if the frame is unknown or older than [`RESEND_MAX_AGE_US`].
    pub fn resend(
        &mut self,
        frame_id: u32,
        indices: impl Iterator<Item = u16>,
        now_us: u64,
        mut send: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<usize> {
        let h = &self.history[frame_id as usize % HISTORY];
        if !h.used || h.f.frame_id != frame_id || now_us.saturating_sub(h.at_us) > RESEND_MAX_AGE_US
        {
            return Ok(0);
        }
        let hs = VideoPacketHeader::SIZE;
        let mut sent = 0;
        let mut one = |i: usize, buf: &mut [u8; MAX_DATAGRAM]| -> io::Result<()> {
            let start = i * MAX_PAYLOAD;
            let end = (start + MAX_PAYLOAD).min(h.total);
            header(&h.f, h.f.flags | FLAG_RESENT, i, h.count, h.groups, h.m)
                .encode(buf)
                .map_err(|_| io::Error::other("header"))?;
            buf[hs..hs + end - start].copy_from_slice(&h.logical[start..end]);
            sent += 1;
            send(&buf[..hs + end - start])
        };
        let mut any = false;
        for i in indices {
            any = true;
            if (i as usize) < h.count {
                one(i as usize, &mut self.buf)?;
            }
        }
        if !any {
            for i in 0..h.count {
                one(i, &mut self.buf)?;
            }
        }
        Ok(sent)
    }
}

/// A complete frame handed out by [`Reassembler::push`].
#[derive(Debug, Clone, Copy)]
pub struct ReceivedFrame<'a> {
    pub frame_id: u32,
    pub flags: u8,
    pub capture_ts_us: u64,
    pub encode_us: u32,
    pub input_seq: u32,
    /// Local receive time of the first and last datagram of this frame.
    pub first_packet_us: u64,
    pub last_packet_us: u64,
    pub packets: u16,
    pub data: &'a [u8],
}

impl ReceivedFrame<'_> {
    pub fn is_idr(&self) -> bool {
        self.flags & FLAG_IDR != 0
    }
}

/// Receive-side counters. `repr(C)` so the client can hand them over FFI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct ReassemblyStats {
    pub packets: u64,
    pub bytes: u64,
    pub duplicates: u64,
    /// Data packets for frames older than the last delivered one.
    pub late: u64,
    /// Unparseable or inconsistent packets.
    pub bad: u64,
    pub frames: u64,
    /// Frames that never completed (packets were lost beyond what FEC
    /// could rebuild).
    pub frames_lost: u64,
    /// Complete frames discarded while waiting for an IDR after a loss.
    pub frames_skipped: u64,
    /// Data packets rebuilt by FEC, and frames that needed it.
    pub recovered: u64,
    pub frames_recovered: u64,
    /// Data packets in frames that completed or were lost, and how many of
    /// them didn't arrive (rebuilt or not): the loss the host's FEC follows.
    pub data_expected: u64,
    pub data_missing: u64,
    /// NACKs sent, resent packets that came and were used, and frames
    /// completed after a NACK (M4).
    pub nacks: u64,
    pub resent: u64,
    pub frames_nacked: u64,
    /// Hitches: a frame given up, then frames withheld until a keyframe
    /// came. How many, and their total and longest length (µs, from giving
    /// up to the keyframe), M4 step 3.
    pub recoveries: u64,
    pub recovery_us_total: u64,
    pub recovery_us_max: u64,
}

/// Frames in flight at once: those arriving plus, while an older frame is
/// being recovered with a NACK, the complete ones held behind it.
const SLOTS: usize = 16;
/// Initial per-slot buffer; a slot grows (once) for a bigger frame.
const INITIAL_FRAME_CAP: usize = 256 << 10;
const WORDS: usize = (u16::MAX as usize).div_ceil(64);

/// NACK timing (M4, ADR 0021). An incomplete frame is stalled, and asked
/// for, once a newer frame's packets arrive or none of its own came for
/// [`NACK_IDLE_US`]. It's asked for again every round trip and a half (at
/// least [`NACK_RETRY_MIN_US`]), and given up (lost: wait for a keyframe)
/// four round trips plus 10 ms after the first ask, within
/// [`NACK_GIVE_UP_MIN_US`]..[`NACK_GIVE_UP_MAX_US`]. Newer frames wait
/// meanwhile, so a recovered frame costs a few ms of delay instead of a
/// keyframe's hitch.
///
/// The idle wait is 8 ms since pacing (m5-test7): a frame's packets now
/// arrive over several ms, and Wi-Fi delivers in clumps, so 3 ms asked for
/// packets still on their way (1,142 NACKs, 0 frames saved, 36,964
/// duplicates). The host sends frames strictly in order, so a newer frame's
/// packets are the reliable sign; the idle wait only matters for the last
/// frame before a still screen.
pub const NACK_IDLE_US: u64 = 8_000;
pub const NACK_RETRY_MIN_US: u64 = 4_000;
pub const NACK_GIVE_UP_MIN_US: u64 = 20_000;
pub const NACK_GIVE_UP_MAX_US: u64 = 60_000;

struct Slot {
    used: bool,
    /// False for a frame known only to be missing (none of it came yet).
    known: bool,
    /// Every data packet is here (or rebuilt).
    done: bool,
    frame_id: u32,
    flags: u8,
    capture_ts_us: u64,
    count: u16,
    groups: u8,
    parity: u8,
    got: u16,
    /// Data packets that came the first time (not rebuilt, not resent), for
    /// the loss report.
    arrived: u16,
    total: usize,
    first_us: u64,
    last_us: u64,
    /// When it was first and last asked for (0: never).
    nack_first: u64,
    nack_last: u64,
    /// A resent packet helped complete it.
    resent_used: bool,
    /// From FrameInfo, once accepted.
    encode_us: u32,
    input_seq: u32,
    bits: Vec<u64>,
    pbits: Vec<u64>,
    buf: Vec<u8>,
    pbuf: Vec<u8>,
    /// Per FEC group: data and parity packets here.
    gdata: [u16; 256],
    gpar: [u16; 256],
}

impl Slot {
    fn new() -> Self {
        Self {
            used: false,
            known: false,
            done: false,
            frame_id: 0,
            flags: 0,
            capture_ts_us: 0,
            count: 0,
            groups: 0,
            parity: 0,
            got: 0,
            arrived: 0,
            total: 0,
            first_us: 0,
            last_us: 0,
            nack_first: 0,
            nack_last: 0,
            resent_used: false,
            encode_us: 0,
            input_seq: 0,
            bits: vec![0; WORDS],
            pbits: vec![0; WORDS],
            buf: vec![0; INITIAL_FRAME_CAP],
            pbuf: Vec::new(),
            gdata: [0; 256],
            gpar: [0; 256],
        }
    }

    fn has(bits: &[u64], i: usize) -> bool {
        bits[i / 64] & (1 << (i % 64)) != 0
    }

    fn set(bits: &mut [u64], i: usize) {
        bits[i / 64] |= 1 << (i % 64);
    }

    /// Ready for frame `h`'s packets (keeping any NACK state).
    fn init(&mut self, h: &VideoPacketHeader, now_us: u64) {
        let need = h.packet_count as usize * MAX_PAYLOAD;
        if self.buf.len() < need {
            self.buf.resize(need, 0); // rare: only frames larger than any before
        }
        let pneed = h.fec_groups as usize * h.fec_parity as usize * MAX_PAYLOAD;
        if self.pbuf.len() < pneed {
            self.pbuf.resize(pneed, 0);
        }
        let words = (h.packet_count as usize).div_ceil(64);
        self.bits[..words].fill(0);
        let pwords = (h.fec_groups as usize * h.fec_parity as usize).div_ceil(64);
        self.pbits[..pwords].fill(0);
        self.gdata[..h.fec_groups as usize].fill(0);
        self.gpar[..h.fec_groups as usize].fill(0);
        self.used = true;
        self.known = true;
        self.done = false;
        self.frame_id = h.frame_id;
        self.flags = h.flags & !FLAG_RESENT;
        self.capture_ts_us = h.capture_ts_us;
        self.count = h.packet_count;
        self.groups = h.fec_groups;
        self.parity = h.fec_parity;
        self.got = 0;
        self.arrived = 0;
        self.resent_used = false;
        self.total = 0;
        self.first_us = now_us;
        self.last_us = now_us;
    }
}

/// Wrapping frame-id order: true if `a` is before `b`.
fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// Rebuilds frames from datagrams and decides which ones the decoder may see.
///
/// Lost data packets are rebuilt from the frame's FEC parity as soon as a
/// group has enough packets. What FEC can't rebuild, [`Self::poll`] asks
/// the host for again (NACK), just as many packets as each group still
/// needs. Frames are handed out in order by [`Self::pop`]; newer complete
/// frames wait while an older one is being recovered. HEVC P-frames depend
/// on the previous frame, so once a frame is given up the reassembler
/// withholds frames until the next IDR ([`Self::needs_keyframe`] tells the
/// client to ask for one).
pub struct Reassembler {
    slots: Vec<Slot>,
    next_expected: Option<u32>,
    /// The newest frame any packet came for.
    newest: Option<u32>,
    awaiting_idr: bool,
    /// False: no NACKs, an incomplete frame is lost as soon as a newer one
    /// is complete (M4 step 1; losssim compares).
    nack: bool,
    rtt_us: u32,
    nack_buf: Vec<u16>,
    /// When a loss started withholding frames (for `recoveries`).
    lost_at: Option<u64>,
    pub stats: ReassemblyStats,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            slots: (0..SLOTS).map(|_| Slot::new()).collect(),
            next_expected: None,
            newest: None,
            awaiting_idr: true,
            nack: true,
            rtt_us: 0,
            nack_buf: Vec::with_capacity(NACK_MAX),
            lost_at: None,
            stats: ReassemblyStats::default(),
        }
    }

    /// Without NACKs: a frame that FEC can't complete is lost as soon as a
    /// newer one completes.
    pub fn without_nack() -> Self {
        Self {
            nack: false,
            ..Self::new()
        }
    }

    /// The latest round trip to the host (µs), for NACK timing.
    pub fn set_rtt(&mut self, rtt_us: u32) {
        self.rtt_us = rtt_us;
    }

    /// True while frames are being withheld until an IDR arrives.
    pub fn needs_keyframe(&self) -> bool {
        self.awaiting_idr
    }

    /// Withhold frames until the next IDR (e.g. the consumer dropped one).
    pub fn resync(&mut self) {
        self.awaiting_idr = true;
    }

    /// A frame is incomplete: call [`Self::poll`] and [`Self::pop`] again
    /// within a few ms even if no packet comes.
    pub fn pending(&self) -> bool {
        self.slots.iter().any(|s| s.used && !s.done)
    }

    fn retry_us(&self) -> u64 {
        (self.rtt_us as u64 * 3 / 2 + 1_000).max(NACK_RETRY_MIN_US)
    }

    fn give_up_us(&self) -> u64 {
        (self.rtt_us as u64 * 4 + 10_000).clamp(NACK_GIVE_UP_MIN_US, NACK_GIVE_UP_MAX_US)
    }

    /// Feeds one datagram received at `now_us`. Complete frames come out of
    /// [`Self::pop`].
    pub fn push(&mut self, pkt: &[u8], now_us: u64) {
        let Ok(h) = VideoPacketHeader::decode(pkt) else {
            self.stats.bad += 1;
            return;
        };
        let payload = &pkt[VideoPacketHeader::SIZE..];
        self.stats.packets += 1;
        self.stats.bytes += pkt.len() as u64;
        let resent = h.flags & FLAG_RESENT != 0;
        if let Some(exp) = self.next_expected {
            if before(h.frame_id, exp) {
                // Parity for a frame that's done (it came whole): expected.
                // A data packet of a recent frame was only reordered, or
                // resent as well: not lost, and not "late" (that slows file
                // copies down). One of the frame just delivered that FEC
                // rebuilt wasn't lost after all.
                if !h.is_parity() {
                    if h.frame_id == exp.wrapping_sub(1) && !resent {
                        self.stats.data_missing = self.stats.data_missing.saturating_sub(1);
                    } else if exp.wrapping_sub(h.frame_id) <= SLOTS as u32 {
                        self.stats.duplicates += 1;
                    } else {
                        self.stats.late += 1;
                    }
                }
                return;
            }
        }
        let last = h.packet_index + 1 == h.packet_count;
        let size_ok = if h.is_parity() || !last {
            payload.len() == MAX_PAYLOAD
        } else {
            payload.len() <= MAX_PAYLOAD
        };
        if !size_ok || (resent && h.is_parity()) {
            self.stats.bad += 1;
            return;
        }
        if self.newest.is_none_or(|n| before(n, h.frame_id)) {
            self.newest = Some(h.frame_id);
        }

        let si = match self
            .slots
            .iter()
            .position(|s| s.used && s.frame_id == h.frame_id)
        {
            Some(i) => {
                if !self.slots[i].known {
                    self.slots[i].init(&h, now_us);
                }
                i
            }
            None => {
                let Some(i) = self.claim_slot(now_us) else {
                    return;
                };
                // Evicting may have given up on this very frame.
                if self.next_expected.is_some_and(|e| before(h.frame_id, e)) {
                    return;
                }
                let s = &mut self.slots[i];
                s.nack_first = 0;
                s.nack_last = 0;
                s.init(&h, now_us);
                i
            }
        };
        let s = &mut self.slots[si];
        if s.count != h.packet_count || s.groups != h.fec_groups || s.parity != h.fec_parity {
            self.stats.bad += 1;
            return;
        }
        if s.done {
            self.stats.duplicates += 1;
            return;
        }
        let groups = s.groups as usize;
        let group = if h.is_parity() {
            let pi = (h.packet_index - h.packet_count) as usize;
            if Slot::has(&s.pbits, pi) {
                self.stats.duplicates += 1;
                return;
            }
            Slot::set(&mut s.pbits, pi);
            s.pbuf[pi * MAX_PAYLOAD..(pi + 1) * MAX_PAYLOAD].copy_from_slice(payload);
            let g = pi / s.parity as usize;
            s.gpar[g] += 1;
            Some(g)
        } else {
            let i = h.packet_index as usize;
            if Slot::has(&s.bits, i) {
                self.stats.duplicates += 1;
                return;
            }
            Slot::set(&mut s.bits, i);
            let off = i * MAX_PAYLOAD;
            s.buf[off..off + payload.len()].copy_from_slice(payload);
            if last {
                s.total = off + payload.len();
                // The rest of the shard is zero, as it was for the parity.
                s.buf[off + payload.len()..off + MAX_PAYLOAD].fill(0);
            }
            s.got += 1;
            if resent {
                self.stats.resent += 1;
                s.resent_used = true;
            } else {
                s.arrived += 1;
            }
            if groups > 0 {
                s.gdata[i % groups] += 1;
            }
            (groups > 0).then(|| i % groups)
        };
        s.last_us = now_us;
        if let Some(g) = group {
            Self::try_rebuild(s, g, &mut self.stats);
        }
        if s.got >= s.count {
            s.done = true;
        }
    }

    /// Asks for what stalled frames still need: `nack(frame_id, indices)`
    /// (no indices: the whole frame, none of it came). Call after each
    /// packet and every few ms while [`Self::pending`].
    pub fn poll(&mut self, now_us: u64, mut nack: impl FnMut(u32, &[u16])) {
        if !self.nack || self.awaiting_idr {
            return; // nothing worth recovering: a keyframe is needed anyway
        }
        let (Some(exp), Some(newest)) = (self.next_expected, self.newest) else {
            return;
        };
        // Frames that never showed at all, between the next one due and the
        // newest: placeholders, so they're asked for (and timed) too.
        let mut id = exp;
        for _ in 0..SLOTS {
            if !before(id, newest) {
                break;
            }
            if !self.slots.iter().any(|s| s.used && s.frame_id == id) {
                let Some(i) = self.slots.iter().position(|s| !s.used) else {
                    break;
                };
                let s = &mut self.slots[i];
                s.used = true;
                s.known = false;
                s.done = false;
                s.frame_id = id;
                s.count = 0;
                s.arrived = 0;
                s.first_us = now_us;
                s.last_us = now_us;
                s.nack_first = 0;
                s.nack_last = 0;
            }
            id = id.wrapping_add(1);
        }
        let retry = self.retry_us();
        for s in &mut self.slots {
            if !s.used || s.done || before(s.frame_id, exp) {
                continue;
            }
            let stalled =
                before(s.frame_id, newest) || now_us.saturating_sub(s.last_us) >= NACK_IDLE_US;
            if !stalled || (s.nack_last != 0 && now_us.saturating_sub(s.nack_last) < retry) {
                continue;
            }
            self.nack_buf.clear();
            if s.known {
                let (count, groups) = (s.count as usize, s.groups as usize);
                if groups == 0 {
                    self.nack_buf.extend(
                        (0..count)
                            .filter(|&i| !Slot::has(&s.bits, i))
                            .map(|i| i as u16)
                            .take(NACK_MAX),
                    );
                } else {
                    // Per group, only as many as FEC still lacks.
                    for g in 0..groups {
                        let k = group_size(count, groups, g);
                        let have = s.gdata[g] as usize + s.gpar[g] as usize;
                        let need = k.saturating_sub(have);
                        let room = NACK_MAX - self.nack_buf.len();
                        self.nack_buf.extend(
                            (g..count)
                                .step_by(groups)
                                .filter(|&i| !Slot::has(&s.bits, i))
                                .map(|i| i as u16)
                                .take(need.min(room)),
                        );
                    }
                }
                if self.nack_buf.is_empty() {
                    continue;
                }
            }
            nack(s.frame_id, &self.nack_buf);
            self.stats.nacks += 1;
            s.nack_last = now_us;
            if s.nack_first == 0 {
                s.nack_first = now_us;
            }
        }
    }

    /// The next frame the decoder may have, if it's here. Call until `None`
    /// after [`Self::push`] / [`Self::poll`].
    pub fn pop(&mut self, now_us: u64) -> Option<ReceivedFrame<'_>> {
        let si = loop {
            let Some(exp) = self.next_expected else {
                // Start-up: the oldest complete frame; anything older is
                // dropped.
                let si = self.oldest(|s| s.done)?;
                let id = self.slots[si].frame_id;
                for s in &mut self.slots {
                    if s.used && before(s.frame_id, id) {
                        s.used = false;
                    }
                }
                if self.accept(si, now_us) {
                    break si;
                }
                continue;
            };
            if let Some(si) = self
                .slots
                .iter()
                .position(|s| s.used && s.done && s.frame_id == exp)
            {
                if self.accept(si, now_us) {
                    break si;
                }
                continue;
            }
            // The next frame isn't here. A newer keyframe makes it moot.
            if let Some(si) = self.oldest(|s| s.done && s.flags & FLAG_IDR != 0) {
                let id = self.slots[si].frame_id;
                if before(exp, id) {
                    self.give_up_through(id.wrapping_sub(1), now_us);
                    continue;
                }
            }
            let newer_done = self
                .slots
                .iter()
                .any(|s| s.used && s.done && before(exp, s.frame_id));
            let give_up = if !self.nack || self.awaiting_idr {
                newer_done
            } else {
                let deadline = self.give_up_us();
                self.slots.iter().any(|s| {
                    s.used
                        && s.frame_id == exp
                        && s.nack_first != 0
                        && now_us.saturating_sub(s.nack_first) >= deadline
                })
            };
            if give_up {
                self.give_up_through(exp, now_us);
                continue;
            }
            return None;
        };
        let s = &self.slots[si];
        Some(ReceivedFrame {
            frame_id: s.frame_id,
            flags: s.flags,
            capture_ts_us: s.capture_ts_us,
            encode_us: s.encode_us,
            input_seq: s.input_seq,
            first_packet_us: s.first_us,
            last_packet_us: s.last_us,
            packets: s.count,
            data: &s.buf[FrameInfo::SIZE..s.total],
        })
    }

    /// The used slot with the oldest frame among those `f` picks.
    fn oldest(&self, f: impl Fn(&Slot) -> bool) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, s) in self.slots.iter().enumerate() {
            if s.used && f(s) && best.is_none_or(|b| before(s.frame_id, self.slots[b].frame_id)) {
                best = Some(i);
            }
        }
        best
    }

    /// Frees a slot, counting its data packets for the loss report.
    fn release(&mut self, si: usize) {
        let s = &mut self.slots[si];
        if s.known {
            self.stats.data_expected += s.count as u64;
            self.stats.data_missing += (s.count - s.arrived) as u64;
        }
        s.used = false;
    }

    /// Every frame up to `id` is lost (or, if complete, useless after one
    /// that's lost): wait for a keyframe.
    fn give_up_through(&mut self, id: u32, now_us: u64) {
        let exp = self.next_expected.unwrap_or(id);
        let mut in_slots = 0u64;
        for i in 0..self.slots.len() {
            let s = &self.slots[i];
            if s.used && !before(id, s.frame_id) {
                if s.done {
                    self.stats.frames_skipped += 1;
                } else {
                    self.stats.frames_lost += 1;
                }
                if !before(s.frame_id, exp) {
                    in_slots += 1;
                }
                self.release(i);
            }
        }
        if before(id, exp) {
            return;
        }
        // Frames that never showed and had no slot.
        let span = id.wrapping_sub(exp) as u64 + 1;
        self.stats.frames_lost += span.saturating_sub(in_slots);
        self.next_expected = Some(id.wrapping_add(1));
        self.awaiting_idr = true;
        self.lost_at.get_or_insert(now_us);
    }

    /// A free slot, evicting the oldest frame (given up) if all are busy.
    fn claim_slot(&mut self, now_us: u64) -> Option<usize> {
        if let Some(i) = self.slots.iter().position(|s| !s.used) {
            return Some(i);
        }
        let oldest = self.oldest(|_| true)?;
        let id = self.slots[oldest].frame_id;
        match self.next_expected {
            Some(exp) if !before(id, exp) => self.give_up_through(id, now_us),
            _ => self.release(oldest),
        }
        self.slots.iter().position(|s| !s.used)
    }

    /// Takes the complete frame in slot `si` off the queue: true if the
    /// decoder may have it.
    fn accept(&mut self, si: usize, now_us: u64) -> bool {
        let id = self.slots[si].frame_id;
        self.next_expected = Some(id.wrapping_add(1));
        // Saved by a NACK only if a resend completed it (m4-test6: 6 asks
        // were all answered by the originals arriving late, and the
        // results still said "5 frames saved").
        let nacked = self.slots[si].resent_used;
        self.release(si);
        let s = &mut self.slots[si];
        if s.count > s.arrived {
            self.stats.frames_recovered += 1;
        }
        if nacked {
            self.stats.frames_nacked += 1;
        }
        let whole = s.count as usize * MAX_PAYLOAD;
        let info = FrameInfo::from_bytes(&s.buf[..whole.min(s.buf.len())]);
        if s.total == 0 {
            // The last packet was rebuilt: the frame's length says where it ends.
            s.total = info.map_or(0, |i| FrameInfo::SIZE + i.frame_len as usize);
        }
        let Some(info) = info.filter(|i| {
            i.frame_len as usize + FrameInfo::SIZE == s.total
                && s.total <= whole
                && s.total > whole - MAX_PAYLOAD
        }) else {
            self.stats.bad += 1;
            self.awaiting_idr = true;
            return false;
        };
        s.encode_us = info.encode_us;
        s.input_seq = info.input_seq;
        let is_idr = s.flags & FLAG_IDR != 0;
        if self.awaiting_idr && !is_idr {
            self.stats.frames_skipped += 1;
            return false;
        }
        self.awaiting_idr = false;
        if let Some(t) = self.lost_at.take() {
            let d = now_us.saturating_sub(t);
            self.stats.recoveries += 1;
            self.stats.recovery_us_total += d;
            self.stats.recovery_us_max = self.stats.recovery_us_max.max(d);
        }
        self.stats.frames += 1;
        true
    }

    /// Rebuilds group `g`'s missing data packets once enough of its packets
    /// are here.
    fn try_rebuild(s: &mut Slot, g: usize, stats: &mut ReassemblyStats) {
        let (count, groups, m) = (s.count as usize, s.groups as usize, s.parity as usize);
        let k = group_size(count, groups, g);
        let have = s.gdata[g] as usize;
        if have >= k || have + (s.gpar[g] as usize) < k {
            return;
        }
        let present: Vec<bool> = (g..count)
            .step_by(groups)
            .map(|i| Slot::has(&s.bits, i))
            .collect();
        let parity: Vec<(usize, &[u8])> = (0..m)
            .filter(|j| Slot::has(&s.pbits, g * m + j))
            .map(|j| {
                (
                    j,
                    &s.pbuf[(g * m + j) * MAX_PAYLOAD..(g * m + j + 1) * MAX_PAYLOAD],
                )
            })
            .collect();
        let mut data: Vec<&mut [u8]> = s.buf[..count * MAX_PAYLOAD]
            .chunks_mut(MAX_PAYLOAD)
            .skip(g)
            .step_by(groups)
            .collect();
        if !fec::reconstruct(&mut data, &present, &parity) {
            return;
        }
        let rebuilt = present.iter().filter(|p| !**p).count();
        for (t, p) in present.iter().enumerate() {
            if !p {
                Slot::set(&mut s.bits, g + t * groups);
            }
        }
        if present.last() == Some(&false) && (g + (k - 1) * groups) == count - 1 {
            s.total = 0; // the last packet was rebuilt: its length comes from FrameInfo
        }
        s.gdata[g] = k as u16;
        s.got += rebuilt as u16;
        stats.recovered += rebuilt as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_data(id: u32, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u32).wrapping_mul(31).wrapping_add(id) as u8)
            .collect()
    }

    fn packets(id: u32, idr: bool, data: &[u8]) -> Vec<Vec<u8>> {
        with_fec(id, idr, data, -1.0)
    }

    /// The frame's datagrams: data packets first, then parity, protecting
    /// against `loss` (negative: no FEC).
    fn with_fec(id: u32, idr: bool, data: &[u8], loss: f32) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let f = OutFrame {
            frame_id: id,
            flags: if idr { FLAG_IDR } else { 0 },
            capture_ts_us: 1_000 + id as u64,
            encode_us: 2_500,
            input_seq: id,
        };
        let mut pk = Packetiser {
            fec_loss: loss,
            ..Default::default()
        };
        let n = pk
            .send_frame(&f, data, 0, |p| {
                out.push(p.to_vec());
                Ok(())
            })
            .unwrap();
        let (g, m) = fec_layout(n as usize, loss);
        assert_eq!(n as usize + g as usize * m as usize, out.len());
        assert_eq!(n as usize, packet_count(data.len()));
        assert!(out.iter().all(|p| p.len() <= MAX_DATAGRAM));
        out
    }

    /// Delivers packets and returns (frame_id, data) of every delivered frame.
    fn feed(r: &mut Reassembler, pkts: &[Vec<u8>]) -> Vec<(u32, Vec<u8>)> {
        let mut got = Vec::new();
        for (i, p) in pkts.iter().enumerate() {
            let now = 10_000 + i as u64;
            r.push(p, now);
            while let Some(f) = r.pop(now) {
                assert!(f.last_packet_us >= f.first_packet_us);
                assert_eq!(f.encode_us, 2_500);
                assert_eq!(f.input_seq, f.frame_id);
                assert_eq!(f.capture_ts_us, 1_000 + f.frame_id as u64);
                got.push((f.frame_id, f.data.to_vec()));
            }
        }
        got
    }

    #[test]
    fn round_trip_sizes() {
        // Edge sizes around packet boundaries, incl. a FrameInfo-only first chunk edge.
        for len in [
            0,
            1,
            MAX_PAYLOAD - FrameInfo::SIZE - 1,
            MAX_PAYLOAD - FrameInfo::SIZE,
            MAX_PAYLOAD - FrameInfo::SIZE + 1,
            MAX_PAYLOAD * 3,
            211_200,
            3 << 20, // bigger than the initial slot buffer
        ] {
            let mut r = Reassembler::new();
            let d = frame_data(0, len);
            let got = feed(&mut r, &packets(0, true, &d));
            assert_eq!(got.len(), 1, "len {len}");
            assert_eq!(got[0].1, d, "len {len}");
        }
    }

    #[test]
    fn out_of_order_and_duplicates() {
        let mut r = Reassembler::new();
        let d = frame_data(5, 20_000);
        let mut p = packets(5, true, &d);
        p.reverse();
        let dup = p[3].clone();
        p.insert(5, dup);
        let got = feed(&mut r, &p);
        assert_eq!(got, vec![(5, d)]);
        assert_eq!(r.stats.duplicates, 1);
        assert_eq!(r.stats.frames, 1);
    }

    #[test]
    fn stream_in_order() {
        let mut r = Reassembler::new();
        let mut all = Vec::new();
        let mut want = Vec::new();
        for id in 0..50u32 {
            let d = frame_data(id, 500 + id as usize * 997);
            all.extend(packets(id, id == 0, &d));
            want.push((id, d));
        }
        assert_eq!(feed(&mut r, &all), want);
        assert_eq!(r.stats.frames_lost, 0);
        assert!(!r.needs_keyframe());
    }

    #[test]
    fn waits_for_first_idr() {
        let mut r = Reassembler::new();
        assert!(r.needs_keyframe());
        let got = feed(&mut r, &packets(0, false, &frame_data(0, 100)));
        assert!(got.is_empty());
        assert_eq!(r.stats.frames_skipped, 1);
        let got = feed(&mut r, &packets(1, true, &frame_data(1, 100)));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn loss_withholds_until_idr() {
        let mut r = Reassembler::without_nack();
        feed(&mut r, &packets(0, true, &frame_data(0, 5_000)));
        // Frame 1 loses a packet.
        let mut p1 = packets(1, false, &frame_data(1, 5_000));
        p1.remove(2);
        assert!(feed(&mut r, &p1).is_empty());
        // Frame 2 completes -> frame 1 counted lost, frame 2 withheld (P-frame).
        assert!(feed(&mut r, &packets(2, false, &frame_data(2, 5_000))).is_empty());
        assert_eq!(r.stats.frames_lost, 1);
        assert!(r.needs_keyframe());
        // A late packet for frame 1 is ignored (a recent frame: counted as
        // a duplicate, not late).
        let late = packets(1, false, &frame_data(1, 5_000));
        assert!(feed(&mut r, &late[2..3]).is_empty());
        assert_eq!((r.stats.late, r.stats.duplicates), (0, 1));
        // IDR resumes delivery.
        let got = feed(&mut r, &packets(3, true, &frame_data(3, 5_000)));
        assert_eq!(got.len(), 1);
        assert!(!r.needs_keyframe());
        assert_eq!(
            feed(&mut r, &packets(4, false, &frame_data(4, 10))).len(),
            1
        );
    }

    #[test]
    fn whole_frame_missing_is_lost() {
        let mut r = Reassembler::without_nack();
        feed(&mut r, &packets(10, true, &frame_data(10, 100)));
        // Frames 11 and 12 never arrive at all.
        assert!(feed(&mut r, &packets(13, false, &frame_data(13, 100))).is_empty());
        assert_eq!(r.stats.frames_lost, 2);
    }

    #[test]
    fn evicts_when_all_slots_incomplete() {
        let mut r = Reassembler::without_nack();
        feed(&mut r, &packets(0, true, &frame_data(0, 100)));
        // SLOTS+1 frames each missing their last packet.
        for id in 1..=SLOTS as u32 + 1 {
            let mut p = packets(id, false, &frame_data(id, 5_000));
            p.pop();
            assert!(feed(&mut r, &p).is_empty());
        }
        let got = feed(&mut r, &packets(20, true, &frame_data(20, 100)));
        assert_eq!(got.len(), 1);
        assert_eq!(r.stats.frames_lost, 19);
    }

    #[test]
    fn rejects_bad_packets() {
        let mut r = Reassembler::new();
        r.push(&[1, 2, 3], 0);
        let mut p = packets(0, true, &frame_data(0, 5_000));
        // Non-last packet truncated.
        p[0].truncate(100);
        r.push(&p[0], 0);
        assert_eq!(r.stats.bad, 2);
    }

    #[test]
    fn frame_ids_wrap() {
        let mut r = Reassembler::new();
        let start = u32::MAX - 2;
        let mut want = Vec::new();
        let mut all = Vec::new();
        for k in 0..6u32 {
            let id = start.wrapping_add(k);
            let d = frame_data(id, 3_000);
            all.extend(packets(id, k == 0, &d));
            want.push((id, d));
        }
        // capture_ts check in feed() assumes small ids; check data only.
        let mut got = Vec::new();
        for p in &all {
            r.push(p, 1);
            while let Some(f) = r.pop(1) {
                got.push((f.frame_id, f.data.to_vec()));
            }
        }
        assert_eq!(got, want);
    }

    fn header_of(p: &[u8]) -> VideoPacketHeader {
        VideoPacketHeader::decode(p).unwrap()
    }

    #[test]
    fn fec_rebuilds_lost_packets() {
        let mut r = Reassembler::new();
        let d = frame_data(0, 40 * MAX_PAYLOAD - 500); // 40 data packets, the last short
        let mut p = with_fec(0, true, &d, 0.05);
        let (g, m) = fec_layout(40, 0.05);
        assert!(g == 1 && m >= 5, "{g} {m}");
        // Lose the first (it holds FrameInfo), the last (short) and three more.
        for i in [39, 20, 11, 5, 0] {
            p.remove(i);
        }
        let got = feed(&mut r, &p);
        assert_eq!(got, vec![(0, d)]);
        assert_eq!(r.stats.recovered, 5);
        assert_eq!(r.stats.frames_recovered, 1);
        assert_eq!((r.stats.data_expected, r.stats.data_missing), (40, 5));
        assert_eq!(r.stats.frames_lost, 0);
    }

    #[test]
    fn interleaving_spreads_a_burst() {
        // 150 data packets: three groups of 50, 5 parity each.
        let mut r = Reassembler::new();
        let d = frame_data(0, 150 * MAX_PAYLOAD - 100);
        let p = with_fec(0, true, &d, 0.02);
        let (g, m) = fec_layout(150, 0.02);
        assert!(g == 3 && m >= 4, "{g} {m}");
        // 12 packets in a row lost: 4 from each group, within its 5.
        let kept: Vec<Vec<u8>> = p
            .iter()
            .filter(|q| {
                let h = header_of(q);
                h.is_parity() || !(60..72).contains(&h.packet_index)
            })
            .cloned()
            .collect();
        assert_eq!(feed(&mut r, &kept), vec![(0, d)]);
        assert_eq!(r.stats.recovered, 12);
    }

    #[test]
    fn too_much_loss_is_still_a_loss() {
        let mut r = Reassembler::without_nack();
        feed(&mut r, &with_fec(0, true, &frame_data(0, 100), 0.0));
        let mut p = with_fec(1, false, &frame_data(1, 10 * MAX_PAYLOAD), 0.0);
        assert_eq!(fec_layout(11, 0.0), (1, 2));
        p.drain(0..3); // three lost, two parity
        assert!(feed(&mut r, &p).is_empty());
        assert!(feed(&mut r, &with_fec(2, false, &frame_data(2, 100), 0.0)).is_empty());
        assert_eq!(r.stats.frames_lost, 1);
        assert!(r.needs_keyframe());
    }

    /// m4-test1: a bogus "100 % loss" report made the host double the video
    /// with parity and the session drowned. Parity stays at most half again,
    /// whatever the reported loss.
    #[test]
    fn parity_is_capped_whatever_the_loss() {
        for count in [1, 2, 3, 10, 40, 64, 65, 200, 1000] {
            for loss in [0.0, 0.05, 0.15, 0.5, 1.0, 100.0] {
                let (g, m) = fec_layout(count, loss);
                let k = count.div_ceil(g as usize);
                assert!(
                    m as usize <= (k.div_ceil(2)).max(3).min(k),
                    "{count} {loss}: {m}"
                );
            }
        }
    }

    #[test]
    fn parity_after_a_whole_frame_is_not_late() {
        let mut r = Reassembler::new();
        let d = frame_data(0, 5 * MAX_PAYLOAD);
        let got = feed(&mut r, &with_fec(0, true, &d, 0.05));
        assert_eq!(got, vec![(0, d)]);
        assert_eq!((r.stats.late, r.stats.bad, r.stats.recovered), (0, 0, 0));
    }

    /// CLAUDE.md: FEC must recover at 5% random loss. 2000 frames of 2 to
    /// 120 packets (tiny ones are the hard case), each packet lost with
    /// probability 5%, protected for 5% reported loss.
    #[test]
    fn five_percent_random_loss() {
        let mut r = Reassembler::without_nack();
        let mut x: u32 = 0x1234_5678;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut delivered = 0;
        for id in 0..2000u32 {
            let len = (2 + (id as usize * 37) % 119) * MAX_PAYLOAD - 77;
            let d = frame_data(id, len);
            let p: Vec<Vec<u8>> = with_fec(id, id == 0, &d, 0.05)
                .into_iter()
                .filter(|_| rnd() % 100 >= 5)
                .collect();
            delivered += feed(&mut r, &p).len();
        }
        assert_eq!(r.stats.frames_lost, 0, "{:?}", r.stats);
        assert_eq!(delivered, 2000);
        assert!(r.stats.recovered > 500, "{:?}", r.stats);
    }

    /// A host that keeps its packetiser, for resends.
    struct Host {
        pk: Packetiser,
    }

    impl Host {
        fn new(loss: f32) -> Self {
            Self {
                pk: Packetiser {
                    fec_loss: loss,
                    ..Default::default()
                },
            }
        }

        fn send(&mut self, id: u32, idr: bool, data: &[u8]) -> Vec<Vec<u8>> {
            let f = OutFrame {
                frame_id: id,
                flags: if idr { FLAG_IDR } else { 0 },
                capture_ts_us: 1_000 + id as u64,
                encode_us: 2_500,
                input_seq: id,
            };
            let mut out = Vec::new();
            self.pk
                .send_frame(&f, data, 0, |p| {
                    out.push(p.to_vec());
                    Ok(())
                })
                .unwrap();
            out
        }

        fn resend(&mut self, id: u32, ix: &[u16]) -> Vec<Vec<u8>> {
            let mut out = Vec::new();
            self.pk
                .resend(id, ix.iter().copied(), 1_000, |p| {
                    out.push(p.to_vec());
                    Ok(())
                })
                .unwrap();
            out
        }
    }

    fn nacks(r: &mut Reassembler, now: u64) -> Vec<(u32, Vec<u16>)> {
        let mut v = Vec::new();
        r.poll(now, |id, ix| v.push((id, ix.to_vec())));
        v
    }

    #[test]
    fn nack_recovers_a_lost_packet() {
        let mut h = Host::new(-1.0); // no FEC: only the NACK can help
        let mut r = Reassembler::new();
        let d: Vec<Vec<u8>> = (0..3).map(|i| frame_data(i, 5_000)).collect();
        assert_eq!(feed(&mut r, &h.send(0, true, &d[0])).len(), 1);
        let mut p1 = h.send(1, false, &d[1]);
        p1.remove(2);
        assert!(feed(&mut r, &p1).is_empty());
        // Frame 2 arrives whole: held behind frame 1, which is asked for.
        assert!(feed(&mut r, &h.send(2, false, &d[2])).is_empty());
        let asked = nacks(&mut r, 20_000);
        assert_eq!(asked, vec![(1, vec![2])]);
        // Not again before the retry time.
        assert!(nacks(&mut r, 21_000).is_empty());
        let again = h.resend(1, &[2]);
        assert_eq!(again.len(), 1);
        assert!(header_of(&again[0]).flags & FLAG_RESENT != 0);
        let got = feed(&mut r, &again);
        assert_eq!(got, vec![(1, d[1].clone()), (2, d[2].clone())]);
        let s = r.stats;
        assert_eq!(
            (s.frames_lost, s.nacks, s.resent, s.frames_nacked),
            (0, 1, 1, 1)
        );
        // The loss report still sees the original loss.
        assert_eq!(s.data_missing, 1);
        assert!(!r.needs_keyframe());
    }

    #[test]
    fn a_late_original_is_not_a_save() {
        let mut h = Host::new(-1.0);
        let mut r = Reassembler::new();
        feed(&mut r, &h.send(0, true, &frame_data(0, 100)));
        let mut p1 = h.send(1, false, &frame_data(1, 5_000));
        let slow = p1.remove(2);
        feed(&mut r, &p1);
        feed(&mut r, &h.send(2, false, &frame_data(2, 100)));
        assert_eq!(nacks(&mut r, 20_000).len(), 1);
        // The original turns up before the resend: the resend is a duplicate.
        assert_eq!(feed(&mut r, &[slow]).len(), 2);
        feed(&mut r, &h.resend(1, &[2]));
        let s = r.stats;
        assert_eq!(
            (s.nacks, s.resent, s.frames_nacked, s.duplicates),
            (1, 0, 0, 1)
        );
    }

    #[test]
    fn nack_asks_only_what_fec_lacks() {
        let mut h = Host::new(0.0);
        let mut r = Reassembler::new();
        feed(&mut r, &h.send(0, true, &frame_data(0, 100)));
        let d1 = frame_data(1, 10 * MAX_PAYLOAD);
        assert_eq!(fec_layout(11, 0.0), (1, 2));
        let mut p1 = h.send(1, false, &d1);
        p1.drain(0..3); // three data packets lost, two parity came
        assert!(feed(&mut r, &p1).is_empty());
        feed(&mut r, &h.send(2, false, &frame_data(2, 100)));
        let asked = nacks(&mut r, 20_000);
        assert_eq!(asked, vec![(1, vec![0])], "one more packet is enough");
        let got = feed(&mut r, &h.resend(1, &asked[0].1));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (1, d1));
        assert_eq!(r.stats.recovered, 2);
    }

    #[test]
    fn nack_asks_for_a_whole_missing_frame() {
        let mut h = Host::new(0.02);
        let mut r = Reassembler::new();
        feed(&mut r, &h.send(0, true, &frame_data(0, 100)));
        let d1 = frame_data(1, 300);
        let _gone = h.send(1, false, &d1);
        feed(&mut r, &h.send(2, false, &frame_data(2, 300)));
        let asked = nacks(&mut r, 20_000);
        assert_eq!(asked, vec![(1, vec![])]);
        let got = feed(&mut r, &h.resend(1, &[]));
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(got[0].1, d1);
        assert_eq!(r.stats.frames_lost, 0);
    }

    #[test]
    fn nack_gives_up_in_time() {
        let mut h = Host::new(-1.0);
        let mut r = Reassembler::new();
        r.set_rtt(3_000);
        feed(&mut r, &h.send(0, true, &frame_data(0, 100)));
        let mut p1 = h.send(1, false, &frame_data(1, 5_000));
        p1.remove(1);
        feed(&mut r, &p1);
        feed(&mut r, &h.send(2, false, &frame_data(2, 100)));
        assert_eq!(nacks(&mut r, 20_000).len(), 1);
        // Asked again after a round trip and a half.
        assert_eq!(nacks(&mut r, 26_000).len(), 1);
        assert!(r.pop(39_000).is_none());
        assert_eq!(r.stats.frames_lost, 0);
        // 4 round trips + 10 ms (22 ms) after the first ask: lost.
        assert!(r.pop(42_000).is_none());
        assert_eq!((r.stats.frames_lost, r.stats.frames_skipped), (1, 1));
        assert!(r.needs_keyframe());
        // Nothing more is asked for while waiting for a keyframe.
        assert!(nacks(&mut r, 50_000).is_empty());
    }

    #[test]
    fn nack_for_the_last_frame_before_a_still_screen() {
        let mut h = Host::new(-1.0);
        let mut r = Reassembler::new();
        feed(&mut r, &h.send(0, true, &frame_data(0, 100)));
        let d1 = frame_data(1, 5_000);
        let mut p1 = h.send(1, false, &d1);
        p1.pop();
        feed(&mut r, &p1); // last packet at 10_002
        assert!(r.pending());
        assert!(
            nacks(&mut r, 11_000).is_empty(),
            "its burst may not be over"
        );
        let asked = nacks(&mut r, 10_002 + NACK_IDLE_US);
        assert_eq!(asked, vec![(1, vec![3])]);
        assert_eq!(feed(&mut r, &h.resend(1, &[3])), vec![(1, d1)]);
        assert!(!r.pending());
    }

    #[test]
    fn a_hitch_is_measured_from_giving_up_to_the_keyframe() {
        let mut h = Host::new(-1.0);
        let mut r = Reassembler::without_nack();
        feed(&mut r, &h.send(0, true, &frame_data(0, 100))); // start-up: no hitch
        let mut p1 = h.send(1, false, &frame_data(1, 5_000));
        p1.remove(0);
        for p in &p1 {
            r.push(p, 50_000);
        }
        for p in h.send(2, false, &frame_data(2, 100)) {
            r.push(&p, 60_000);
        }
        assert!(r.pop(60_000).is_none()); // frame 1 lost at 60 ms
        for p in h.send(3, true, &frame_data(3, 100)) {
            r.push(&p, 95_000);
        }
        assert!(r.pop(95_000).is_some());
        let s = r.stats;
        assert_eq!(
            (s.recoveries, s.recovery_us_total, s.recovery_us_max),
            (1, 35_000, 35_000)
        );
    }

    /// m5-test7: paced frames arrive over several ms, in Wi-Fi clumps; the
    /// old 3 ms idle wait asked for packets still on their way.
    #[test]
    fn a_paced_frame_in_clumps_is_not_asked_for() {
        let mut h = Host::new(0.02);
        let mut r = Reassembler::new();
        feed(&mut r, &h.send(0, true, &frame_data(0, 100)));
        let p1 = h.send(1, false, &frame_data(1, 40 * MAX_PAYLOAD));
        let mut now = 100_000u64;
        let mut asked = 0;
        // Clumps of 8 packets, 4 ms apart.
        for clump in p1.chunks(8) {
            for p in clump {
                r.push(p, now);
            }
            r.poll(now, |_, _| asked += 1);
            now += 4_000;
            r.poll(now, |_, _| asked += 1);
        }
        assert_eq!(asked, 0);
        assert!(r.pop(now).is_some());
    }

    #[test]
    fn a_newer_keyframe_ends_the_wait() {
        let mut h = Host::new(-1.0);
        let mut r = Reassembler::new();
        feed(&mut r, &h.send(0, true, &frame_data(0, 100)));
        let mut p1 = h.send(1, false, &frame_data(1, 5_000));
        p1.remove(0);
        feed(&mut r, &p1);
        let d2 = frame_data(2, 100);
        assert_eq!(feed(&mut r, &h.send(2, true, &d2)), vec![(2, d2)]);
        assert_eq!(r.stats.frames_lost, 1);
        assert!(!r.needs_keyframe());
    }

    #[test]
    fn resends_stop_when_too_old() {
        let mut h = Host::new(-1.0);
        h.send(7, true, &frame_data(7, 100));
        let mut n = 0;
        let mut go = |now| {
            h.pk.resend(7, std::iter::empty(), now, |_| {
                n += 1;
                Ok(())
            })
            .unwrap()
        };
        assert_eq!(go(RESEND_MAX_AGE_US), 1);
        assert_eq!(go(RESEND_MAX_AGE_US + 1), 0);
        assert_eq!(
            h.pk.resend(8, std::iter::empty(), 0, |_| Ok(())).unwrap(),
            0
        );
    }
}

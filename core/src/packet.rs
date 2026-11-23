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
use crate::protocol::{FrameInfo, VideoPacketHeader, FLAG_IDR};
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
    let p = (loss as f64 * 1.5 + 0.005).min(0.5);
    let cap = k.min(fec::MAX_SHARDS - k).max(1);
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

/// Cuts frames into datagrams in reusable buffers, adding FEC.
pub struct Packetiser {
    buf: [u8; MAX_DATAGRAM],
    /// The frame's logical payload, zero-padded to whole chunks.
    logical: Vec<u8>,
    parity: Vec<u8>,
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
            fec_loss: FEC_LOSS_START,
        }
    }
}

/// The loss to protect against before the viewer has said how lossy the
/// link is.
pub const FEC_LOSS_START: f32 = 0.02;

impl Packetiser {
    /// Calls `send` once per datagram: the data packets in order, then the
    /// parity packets. Returns the number of data packets.
    pub fn send_frame(
        &mut self,
        f: &OutFrame,
        data: &[u8],
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
        let header = |index: usize| VideoPacketHeader {
            stream_id: 0,
            flags: f.flags,
            frame_id: f.frame_id,
            packet_index: index as u16,
            packet_count: count as u16,
            fec_groups: groups,
            fec_parity: m,
            capture_ts_us: f.capture_ts_us,
        };
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
        Ok(count as u16)
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
}

/// Frames in flight at once. More than enough without reordering across
/// frames (M2); M5's jitter buffer may want more.
const SLOTS: usize = 4;
/// Initial per-slot buffer: a 1440p IDR is ~75-210 KB; 4K scene changes more.
const INITIAL_FRAME_CAP: usize = 1 << 20;
const WORDS: usize = (u16::MAX as usize).div_ceil(64);

struct Slot {
    used: bool,
    frame_id: u32,
    flags: u8,
    capture_ts_us: u64,
    count: u16,
    groups: u8,
    parity: u8,
    got: u16,
    /// Data packets that came (before any rebuilding), for the loss report.
    arrived: u16,
    total: usize,
    first_us: u64,
    last_us: u64,
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
}

/// Wrapping frame-id order: true if `a` is before `b`.
fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// Rebuilds frames from datagrams and decides which ones the decoder may see.
///
/// Lost data packets are rebuilt from the frame's FEC parity as soon as a
/// group has enough packets. HEVC P-frames depend on the previous frame, so
/// after a frame is lost anyway the reassembler withholds frames until the
/// next IDR ([`Self::needs_keyframe`] tells the client to ask for one).
/// Frames are delivered in order; a frame still incomplete when a newer one
/// completes counts as lost. LATER(M4 step 2): NACK and a reorder window.
pub struct Reassembler {
    slots: Vec<Slot>,
    next_expected: Option<u32>,
    awaiting_idr: bool,
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
            awaiting_idr: true,
            stats: ReassemblyStats::default(),
        }
    }

    /// True while frames are being withheld until an IDR arrives.
    pub fn needs_keyframe(&self) -> bool {
        self.awaiting_idr
    }

    /// Withhold frames until the next IDR (e.g. the consumer dropped one).
    pub fn resync(&mut self) {
        self.awaiting_idr = true;
    }

    /// Feeds one datagram received at `now_us`. Returns a frame when this
    /// packet completed one that may be decoded.
    pub fn push(&mut self, pkt: &[u8], now_us: u64) -> Option<ReceivedFrame<'_>> {
        let Ok(h) = VideoPacketHeader::decode(pkt) else {
            self.stats.bad += 1;
            return None;
        };
        let payload = &pkt[VideoPacketHeader::SIZE..];
        self.stats.packets += 1;
        self.stats.bytes += pkt.len() as u64;
        if let Some(exp) = self.next_expected {
            if before(h.frame_id, exp) {
                // Parity for a frame that's done (it came whole): expected.
                // A data packet of the frame just delivered was only
                // reordered: FEC rebuilt it first. Not lost after all, and
                // not "late" (that slows file copies down).
                if h.frame_id == exp.wrapping_sub(1) && !h.is_parity() {
                    self.stats.data_missing = self.stats.data_missing.saturating_sub(1);
                } else if !h.is_parity() {
                    self.stats.late += 1;
                }
                return None;
            }
        }
        let last = h.packet_index + 1 == h.packet_count;
        let size_ok = if h.is_parity() || !last {
            payload.len() == MAX_PAYLOAD
        } else {
            payload.len() <= MAX_PAYLOAD
        };
        if !size_ok {
            self.stats.bad += 1;
            return None;
        }

        let si = match self
            .slots
            .iter()
            .position(|s| s.used && s.frame_id == h.frame_id)
        {
            Some(i) => i,
            None => self.claim_slot(&h, now_us),
        };
        let s = &mut self.slots[si];
        if s.count != h.packet_count || s.groups != h.fec_groups || s.parity != h.fec_parity {
            self.stats.bad += 1;
            return None;
        }
        let groups = s.groups as usize;
        let group = if h.is_parity() {
            let pi = (h.packet_index - h.packet_count) as usize;
            if Slot::has(&s.pbits, pi) {
                self.stats.duplicates += 1;
                return None;
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
                return None;
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
            s.arrived += 1;
            if groups > 0 {
                s.gdata[i % groups] += 1;
            }
            (groups > 0).then(|| i % groups)
        };
        s.last_us = now_us;
        if let Some(g) = group {
            Self::try_rebuild(s, g, &mut self.stats);
        }
        if s.got < s.count {
            return None;
        }
        self.complete(si)
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

    fn claim_slot(&mut self, h: &VideoPacketHeader, now_us: u64) -> usize {
        let si = match self.slots.iter().position(|s| !s.used) {
            Some(i) => i,
            None => {
                // Evict the oldest incomplete frame; it's lost.
                let mut oldest = 0;
                for (i, s) in self.slots.iter().enumerate() {
                    if before(s.frame_id, self.slots[oldest].frame_id) {
                        oldest = i;
                    }
                }
                let s = &self.slots[oldest];
                self.stats.data_expected += s.count as u64;
                self.stats.data_missing += (s.count - s.arrived) as u64;
                oldest
            }
        };
        let s = &mut self.slots[si];
        let need = h.packet_count as usize * MAX_PAYLOAD;
        if s.buf.len() < need {
            s.buf.resize(need, 0); // rare: only frames larger than any before
        }
        let pneed = h.fec_groups as usize * h.fec_parity as usize * MAX_PAYLOAD;
        if s.pbuf.len() < pneed {
            s.pbuf.resize(pneed, 0);
        }
        let words = (h.packet_count as usize).div_ceil(64);
        s.bits[..words].fill(0);
        let pwords = (h.fec_groups as usize * h.fec_parity as usize).div_ceil(64);
        s.pbits[..pwords].fill(0);
        s.gdata[..h.fec_groups as usize].fill(0);
        s.gpar[..h.fec_groups as usize].fill(0);
        s.used = true;
        s.frame_id = h.frame_id;
        s.flags = h.flags;
        s.capture_ts_us = h.capture_ts_us;
        s.count = h.packet_count;
        s.groups = h.fec_groups;
        s.parity = h.fec_parity;
        s.got = 0;
        s.arrived = 0;
        s.total = 0;
        s.first_us = now_us;
        si
    }

    fn complete(&mut self, si: usize) -> Option<ReceivedFrame<'_>> {
        let id = self.slots[si].frame_id;
        self.slots[si].used = false;
        // Anything older still in flight can never be delivered now.
        for s in &mut self.slots {
            if s.used && before(s.frame_id, id) {
                s.used = false;
                self.stats.data_expected += s.count as u64;
                self.stats.data_missing += (s.count - s.arrived) as u64;
            }
        }
        if let Some(exp) = self.next_expected {
            let gap = id.wrapping_sub(exp) as u64;
            if gap > 0 {
                self.stats.frames_lost += gap;
                self.awaiting_idr = true;
            }
        }
        self.next_expected = Some(id.wrapping_add(1));

        let s = &mut self.slots[si];
        let missing = (s.count - s.arrived) as u64;
        self.stats.data_expected += s.count as u64;
        self.stats.data_missing += missing;
        if missing > 0 {
            self.stats.frames_recovered += 1;
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
            return None;
        };
        let is_idr = s.flags & FLAG_IDR != 0;
        if self.awaiting_idr && !is_idr {
            self.stats.frames_skipped += 1;
            return None;
        }
        self.awaiting_idr = false;
        self.stats.frames += 1;
        let s = &self.slots[si];
        Some(ReceivedFrame {
            frame_id: id,
            flags: s.flags,
            capture_ts_us: s.capture_ts_us,
            encode_us: info.encode_us,
            input_seq: info.input_seq,
            first_packet_us: s.first_us,
            last_packet_us: s.last_us,
            packets: s.count,
            data: &s.buf[FrameInfo::SIZE..s.total],
        })
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
            .send_frame(&f, data, |p| {
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
            if let Some(f) = r.push(p, 10_000 + i as u64) {
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
        let mut r = Reassembler::new();
        feed(&mut r, &packets(0, true, &frame_data(0, 5_000)));
        // Frame 1 loses a packet.
        let mut p1 = packets(1, false, &frame_data(1, 5_000));
        p1.remove(2);
        assert!(feed(&mut r, &p1).is_empty());
        // Frame 2 completes -> frame 1 counted lost, frame 2 withheld (P-frame).
        assert!(feed(&mut r, &packets(2, false, &frame_data(2, 5_000))).is_empty());
        assert_eq!(r.stats.frames_lost, 1);
        assert!(r.needs_keyframe());
        // Late packet for frame 1 is ignored.
        let late = packets(1, false, &frame_data(1, 5_000));
        assert!(feed(&mut r, &late[2..3]).is_empty());
        assert_eq!(r.stats.late, 1);
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
        let mut r = Reassembler::new();
        feed(&mut r, &packets(10, true, &frame_data(10, 100)));
        // Frames 11 and 12 never arrive at all.
        assert!(feed(&mut r, &packets(13, false, &frame_data(13, 100))).is_empty());
        assert_eq!(r.stats.frames_lost, 2);
    }

    #[test]
    fn evicts_when_all_slots_incomplete() {
        let mut r = Reassembler::new();
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
        assert!(r.push(&[1, 2, 3], 0).is_none());
        let mut p = packets(0, true, &frame_data(0, 5_000));
        // Non-last packet truncated.
        p[0].truncate(100);
        assert!(r.push(&p[0], 0).is_none());
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
            if let Some(f) = r.push(p, 1) {
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
        let mut r = Reassembler::new();
        feed(&mut r, &with_fec(0, true, &frame_data(0, 100), 0.0));
        let mut p = with_fec(1, false, &frame_data(1, 10 * MAX_PAYLOAD), 0.0);
        assert_eq!(fec_layout(11, 0.0), (1, 2));
        p.drain(0..3); // three lost, two parity
        assert!(feed(&mut r, &p).is_empty());
        assert!(feed(&mut r, &with_fec(2, false, &frame_data(2, 100), 0.0)).is_empty());
        assert_eq!(r.stats.frames_lost, 1);
        assert!(r.needs_keyframe());
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
        let mut r = Reassembler::new();
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
}

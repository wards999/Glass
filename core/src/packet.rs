//! Packetiser (frame -> MTU-sized UDP datagrams) and reassembler (datagrams ->
//! frames). Both are allocation-free in steady state: buffers are sized once
//! and only grow for an unusually large frame.
//!
//! A frame's logical payload is `FrameInfo` (8 bytes) followed by the Annex-B
//! access unit, cut into chunks of [`MAX_PAYLOAD`] bytes. Every chunk except
//! the last is exactly `MAX_PAYLOAD` long, so chunk `i` lives at offset
//! `i * MAX_PAYLOAD` and the receiver can place packets in any order.

use crate::protocol::{FrameInfo, VideoPacketHeader, FLAG_IDR};
use std::io;

/// Whole UDP datagram, header included. Fits a 1500-byte Ethernet MTU with
/// room for IP/UDP (and, from M6, the AEAD tag).
pub const MAX_DATAGRAM: usize = 1400;
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM - VideoPacketHeader::SIZE;

/// What the host knows about a frame when it sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutFrame {
    pub frame_id: u32,
    pub flags: u8,
    pub capture_ts_us: u64,
    pub encode_us: u32,
}

/// Number of datagrams a frame of `data_len` Annex-B bytes needs.
pub fn packet_count(data_len: usize) -> usize {
    (FrameInfo::SIZE + data_len).div_ceil(MAX_PAYLOAD)
}

/// Cuts frames into datagrams in a reusable buffer.
pub struct Packetiser {
    buf: [u8; MAX_DATAGRAM],
}

impl Default for Packetiser {
    fn default() -> Self {
        Self {
            buf: [0; MAX_DATAGRAM],
        }
    }
}

impl Packetiser {
    /// Calls `send` once per datagram, in order. Returns the packet count.
    pub fn send_frame(
        &mut self,
        f: &OutFrame,
        data: &[u8],
        mut send: impl FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<u16> {
        let count = packet_count(data.len());
        if count > u16::MAX as usize || data.len() > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame too large",
            ));
        }
        let info = FrameInfo {
            encode_us: f.encode_us,
            frame_len: data.len() as u32,
        }
        .to_bytes();
        let total = FrameInfo::SIZE + data.len();
        let hs = VideoPacketHeader::SIZE;
        for i in 0..count {
            let start = i * MAX_PAYLOAD;
            let end = (start + MAX_PAYLOAD).min(total);
            VideoPacketHeader {
                stream_id: 0,
                flags: f.flags,
                frame_id: f.frame_id,
                packet_index: i as u16,
                packet_count: count as u16,
                fec_group: 0, // LATER(M4): FEC groups.
                capture_ts_us: f.capture_ts_us,
            }
            .encode(&mut self.buf)
            .map_err(|_| io::Error::other("header"))?;
            // Logical range [start, end) spans FrameInfo then data.
            let mut w = hs;
            if start < FrameInfo::SIZE {
                let e = end.min(FrameInfo::SIZE);
                self.buf[w..w + e - start].copy_from_slice(&info[start..e]);
                w += e - start;
            }
            let ds = start.max(FrameInfo::SIZE) - FrameInfo::SIZE;
            let de = end - FrameInfo::SIZE;
            if de > ds {
                self.buf[w..w + de - ds].copy_from_slice(&data[ds..de]);
                w += de - ds;
            }
            send(&self.buf[..w])?;
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
    /// Packets for frames older than the last delivered one.
    pub late: u64,
    /// Unparseable or inconsistent packets.
    pub bad: u64,
    pub frames: u64,
    /// Frames that never completed (a packet was lost).
    pub frames_lost: u64,
    /// Complete frames discarded while waiting for an IDR after a loss.
    pub frames_skipped: u64,
}

/// Frames in flight at once. More than enough without reordering across
/// frames (M2); M5's jitter buffer may want more.
const SLOTS: usize = 4;
/// Initial per-slot buffer: a 1440p IDR is ~75-210 KB; 4K scene changes more.
const INITIAL_FRAME_CAP: usize = 1 << 20;

struct Slot {
    used: bool,
    frame_id: u32,
    flags: u8,
    capture_ts_us: u64,
    count: u16,
    got: u16,
    total: usize,
    first_us: u64,
    last_us: u64,
    bits: Vec<u64>,
    buf: Vec<u8>,
}

impl Slot {
    fn new() -> Self {
        Self {
            used: false,
            frame_id: 0,
            flags: 0,
            capture_ts_us: 0,
            count: 0,
            got: 0,
            total: 0,
            first_us: 0,
            last_us: 0,
            bits: vec![0; (u16::MAX as usize).div_ceil(64)],
            buf: vec![0; INITIAL_FRAME_CAP],
        }
    }
}

/// Wrapping frame-id order: true if `a` is before `b`.
fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// Rebuilds frames from datagrams and decides which ones the decoder may see.
///
/// HEVC P-frames depend on the previous frame, so after any lost frame the
/// reassembler withholds frames until the next IDR ([`Self::needs_keyframe`]
/// tells the client to ask for one). Frames are delivered in order; a frame
/// still incomplete when a newer one completes counts as lost.
/// LATER(M4): FEC recovery before declaring a loss. LATER(M5): reorder window.
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
                self.stats.late += 1;
                return None;
            }
        }
        let last = h.packet_index + 1 == h.packet_count;
        if payload.len() > MAX_PAYLOAD || (!last && payload.len() != MAX_PAYLOAD) {
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
        if s.count != h.packet_count {
            self.stats.bad += 1;
            return None;
        }
        let (word, bit) = (h.packet_index as usize / 64, h.packet_index as usize % 64);
        if s.bits[word] & (1 << bit) != 0 {
            self.stats.duplicates += 1;
            return None;
        }
        s.bits[word] |= 1 << bit;
        let off = h.packet_index as usize * MAX_PAYLOAD;
        s.buf[off..off + payload.len()].copy_from_slice(payload);
        if last {
            s.total = off + payload.len();
        }
        s.got += 1;
        s.last_us = now_us;
        if s.got < s.count {
            return None;
        }
        self.complete(si)
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
                oldest
            }
        };
        let s = &mut self.slots[si];
        let need = h.packet_count as usize * MAX_PAYLOAD;
        if s.buf.len() < need {
            s.buf.resize(need, 0); // rare: only frames larger than any before
        }
        let words = (h.packet_count as usize).div_ceil(64);
        s.bits[..words].fill(0);
        s.used = true;
        s.frame_id = h.frame_id;
        s.flags = h.flags;
        s.capture_ts_us = h.capture_ts_us;
        s.count = h.packet_count;
        s.got = 0;
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

        let s = &self.slots[si];
        let info = FrameInfo::from_bytes(&s.buf[..s.total]);
        let Some(info) = info.filter(|i| i.frame_len as usize + FrameInfo::SIZE == s.total) else {
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
        Some(ReceivedFrame {
            frame_id: id,
            flags: s.flags,
            capture_ts_us: s.capture_ts_us,
            encode_us: info.encode_us,
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
        let mut out = Vec::new();
        let f = OutFrame {
            frame_id: id,
            flags: if idr { FLAG_IDR } else { 0 },
            capture_ts_us: 1_000 + id as u64,
            encode_us: 2_500,
        };
        let n = Packetiser::default()
            .send_frame(&f, data, |p| {
                out.push(p.to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(n as usize, out.len());
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
}

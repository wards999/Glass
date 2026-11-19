//! Wire formats. `docs/protocol.md` is the source of truth; keep them in sync and
//! bump [`PROTOCOL_VERSION`] on any change.

/// Protocol version. 0 = draft (pre-M2); nothing is on the wire yet.
pub const PROTOCOL_VERSION: u8 = 0;

/// Frame is an IDR / keyframe.
pub const FLAG_IDR: u8 = 1 << 0;
/// Frame is a still-region refinement update.
pub const FLAG_REFINEMENT: u8 = 1 << 1;

/// Header at the start of every video packet. Fixed size, big-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoPacketHeader {
    pub stream_id: u8,
    pub flags: u8,
    pub frame_id: u32,
    pub packet_index: u16,
    pub packet_count: u16,
    /// FEC group info. Always 0 until M4 defines the layout.
    pub fec_group: u32,
    /// Host capture timestamp, microseconds on the host clock.
    pub capture_ts_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    TooShort,
    BadVersion(u8),
    /// packet_count == 0 or packet_index >= packet_count.
    BadIndex,
}

impl VideoPacketHeader {
    pub const SIZE: usize = 24;

    /// Writes the header into the first [`Self::SIZE`] bytes of `buf`.
    pub fn encode(&self, buf: &mut [u8]) -> Result<(), HeaderError> {
        if buf.len() < Self::SIZE {
            return Err(HeaderError::TooShort);
        }
        if self.packet_count == 0 || self.packet_index >= self.packet_count {
            return Err(HeaderError::BadIndex);
        }
        buf[0] = PROTOCOL_VERSION;
        buf[1] = self.stream_id;
        buf[2] = self.flags;
        buf[3] = 0; // reserved
        buf[4..8].copy_from_slice(&self.frame_id.to_be_bytes());
        buf[8..10].copy_from_slice(&self.packet_index.to_be_bytes());
        buf[10..12].copy_from_slice(&self.packet_count.to_be_bytes());
        buf[12..16].copy_from_slice(&self.fec_group.to_be_bytes());
        buf[16..24].copy_from_slice(&self.capture_ts_us.to_be_bytes());
        Ok(())
    }

    /// Parses a header from the first [`Self::SIZE`] bytes of `buf`.
    pub fn decode(buf: &[u8]) -> Result<Self, HeaderError> {
        if buf.len() < Self::SIZE {
            return Err(HeaderError::TooShort);
        }
        if buf[0] != PROTOCOL_VERSION {
            return Err(HeaderError::BadVersion(buf[0]));
        }
        let u16_at = |i: usize| u16::from_be_bytes([buf[i], buf[i + 1]]);
        let u32_at = |i: usize| u32::from_be_bytes(buf[i..i + 4].try_into().unwrap());
        let h = Self {
            stream_id: buf[1],
            flags: buf[2],
            frame_id: u32_at(4),
            packet_index: u16_at(8),
            packet_count: u16_at(10),
            fec_group: u32_at(12),
            capture_ts_us: u64::from_be_bytes(buf[16..24].try_into().unwrap()),
        };
        if h.packet_count == 0 || h.packet_index >= h.packet_count {
            return Err(HeaderError::BadIndex);
        }
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> VideoPacketHeader {
        VideoPacketHeader {
            stream_id: 1,
            flags: FLAG_IDR | FLAG_REFINEMENT,
            frame_id: 0xDEAD_BEEF,
            packet_index: 41,
            packet_count: 42,
            fec_group: 0,
            capture_ts_us: 0x0123_4567_89AB_CDEF,
        }
    }

    #[test]
    fn header_round_trip() {
        let mut buf = [0u8; 1400];
        sample().encode(&mut buf).unwrap();
        assert_eq!(VideoPacketHeader::decode(&buf).unwrap(), sample());
    }

    #[test]
    fn header_is_big_endian_on_the_wire() {
        let mut buf = [0u8; VideoPacketHeader::SIZE];
        sample().encode(&mut buf).unwrap();
        assert_eq!(&buf[4..8], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(&buf[8..12], &[0, 41, 0, 42]);
    }

    #[test]
    fn rejects_short_buffers() {
        let mut buf = [0u8; VideoPacketHeader::SIZE - 1];
        assert_eq!(sample().encode(&mut buf), Err(HeaderError::TooShort));
        assert_eq!(VideoPacketHeader::decode(&buf), Err(HeaderError::TooShort));
    }

    #[test]
    fn rejects_wrong_version() {
        let mut buf = [0u8; VideoPacketHeader::SIZE];
        sample().encode(&mut buf).unwrap();
        buf[0] = PROTOCOL_VERSION.wrapping_add(1);
        assert_eq!(
            VideoPacketHeader::decode(&buf),
            Err(HeaderError::BadVersion(PROTOCOL_VERSION.wrapping_add(1)))
        );
    }

    #[test]
    fn rejects_bad_index() {
        let mut buf = [0u8; VideoPacketHeader::SIZE];
        let mut h = sample();
        h.packet_index = h.packet_count;
        assert_eq!(h.encode(&mut buf), Err(HeaderError::BadIndex));
        sample().encode(&mut buf).unwrap();
        buf[10..12].copy_from_slice(&0u16.to_be_bytes());
        assert_eq!(VideoPacketHeader::decode(&buf), Err(HeaderError::BadIndex));
    }
}

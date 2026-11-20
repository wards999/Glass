//! Wire formats. `docs/protocol.md` is the source of truth; keep them in sync and
//! bump [`PROTOCOL_VERSION`] on any change.

/// Protocol version. 3 = M3 (test 2): v2 (input datagrams, cursor messages,
/// `FrameInfo::input_seq`) plus `HostNotice`.
pub const PROTOCOL_VERSION: u8 = 3;

/// Default TCP control port; the host's UDP media socket uses the same number.
pub const DEFAULT_PORT: u16 = 47800;

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

/// Per-frame info at the start of each frame's payload (before the Annex-B
/// bytes), so it travels in packet 0. Fixed size, big-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameInfo {
    /// Host time from capture to encoded packet out, µs.
    pub encode_us: u32,
    /// Length of the Annex-B data that follows.
    pub frame_len: u32,
    /// Last input packet the host had applied when this frame was captured
    /// (0 = none). The viewer turns it into input -> screen latency.
    pub input_seq: u32,
}

impl FrameInfo {
    pub const SIZE: usize = 12;

    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.encode_us.to_be_bytes());
        b[4..8].copy_from_slice(&self.frame_len.to_be_bytes());
        b[8..12].copy_from_slice(&self.input_seq.to_be_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let at = |i: usize| -> Option<u32> {
            Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
        };
        Some(Self {
            encode_us: at(0)?,
            frame_len: at(4)?,
            input_seq: at(8)?,
        })
    }
}

/// UDP datagram client -> host that tells the host where to send video (and
/// opens the client's firewall/NAT for the reply). `"GLPN"` + session id.
pub const PUNCH_MAGIC: [u8; 4] = *b"GLPN";
pub const PUNCH_SIZE: usize = 8;

pub fn encode_punch(session_id: u32) -> [u8; PUNCH_SIZE] {
    let mut b = [0u8; PUNCH_SIZE];
    b[0..4].copy_from_slice(&PUNCH_MAGIC);
    b[4..8].copy_from_slice(&session_id.to_be_bytes());
    b
}

pub fn decode_punch(b: &[u8]) -> Option<u32> {
    if b.len() != PUNCH_SIZE || b[0..4] != PUNCH_MAGIC {
        return None;
    }
    Some(u32::from_be_bytes(b[4..8].try_into().ok()?))
}

/// Stream description sent by the host in [`Control::Welcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct StreamInfo {
    /// `recording::CODEC_HEVC`.
    pub codec: u8,
    /// `recording::CHROMA_420` or `CHROMA_444`.
    pub chroma: u8,
    pub width: u16,
    pub height: u16,
    /// Host display refresh rate (capture rate cap).
    pub fps: u16,
}

/// `HostNotice` codes: why the host can't do what the viewer expects.
pub const NOTICE_NONE: u8 = 0;
/// An administrator window is in front; Windows drops injected input (UIPI).
pub const NOTICE_INPUT_BLOCKED_ELEVATED: u8 = 1;
/// UAC prompt / lock screen: the secure desktop takes no input and isn't captured.
pub const NOTICE_INPUT_BLOCKED_SECURE_DESKTOP: u8 = 2;

/// Why a host refused a viewer.
pub const REJECT_VERSION: u8 = 1;
pub const REJECT_BUSY: u8 = 2;

/// Control-channel messages (TCP). Framing: `u16 length` (big-endian, covers
/// type + body) then `u8 type` then the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Client -> host, first message. Display size is informational until M7.
    Hello {
        version: u8,
        display_w: u16,
        display_h: u16,
    },
    /// Host -> client reply to Hello.
    Welcome {
        version: u8,
        info: StreamInfo,
        udp_port: u16,
        session_id: u32,
    },
    Reject {
        reason: u8,
    },
    /// Client -> host; `client_ts` echoed back for RTT/offset.
    Ping {
        seq: u32,
        client_ts: u64,
    },
    /// Host -> client; `host_ts` taken when the Ping was read.
    Pong {
        seq: u32,
        client_ts: u64,
        host_ts: u64,
    },
    /// Client -> host: the decoder needs an IDR (loss or decode error).
    KeyframeRequest,
    /// Either side, before closing.
    Bye,
    /// Host -> client: the host's pointer is shown or hidden.
    CursorVisible {
        visible: bool,
    },
    /// Host -> client: a condition the user should know about (`NOTICE_*`);
    /// `NOTICE_NONE` clears it.
    HostNotice {
        code: u8,
    },
}

/// Largest encoded control message (including the 2-byte length).
pub const CONTROL_MAX: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    /// Need more bytes.
    Incomplete,
    /// Unknown type or wrong body length.
    Malformed,
}

impl Control {
    fn kind(&self) -> u8 {
        match self {
            Control::Hello { .. } => 1,
            Control::Welcome { .. } => 2,
            Control::Reject { .. } => 3,
            Control::Ping { .. } => 4,
            Control::Pong { .. } => 5,
            Control::KeyframeRequest => 6,
            Control::Bye => 7,
            Control::CursorVisible { .. } => 9,
            Control::HostNotice { .. } => 10,
        }
    }

    /// Encodes into `buf` and returns the encoded length (≤ [`CONTROL_MAX`]).
    pub fn encode(&self, buf: &mut [u8; CONTROL_MAX]) -> usize {
        let mut n = 3;
        let mut put = |bytes: &[u8]| {
            buf[n..n + bytes.len()].copy_from_slice(bytes);
            n += bytes.len();
        };
        match *self {
            Control::Hello {
                version,
                display_w,
                display_h,
            } => {
                put(&[version]);
                put(&display_w.to_be_bytes());
                put(&display_h.to_be_bytes());
            }
            Control::Welcome {
                version,
                info,
                udp_port,
                session_id,
            } => {
                put(&[version, info.codec, info.chroma, 0]);
                put(&info.width.to_be_bytes());
                put(&info.height.to_be_bytes());
                put(&info.fps.to_be_bytes());
                put(&udp_port.to_be_bytes());
                put(&session_id.to_be_bytes());
            }
            Control::Reject { reason } => put(&[reason]),
            Control::CursorVisible { visible } => put(&[visible as u8]),
            Control::HostNotice { code } => put(&[code]),
            Control::Ping { seq, client_ts } => {
                put(&seq.to_be_bytes());
                put(&client_ts.to_be_bytes());
            }
            Control::Pong {
                seq,
                client_ts,
                host_ts,
            } => {
                put(&seq.to_be_bytes());
                put(&client_ts.to_be_bytes());
                put(&host_ts.to_be_bytes());
            }
            Control::KeyframeRequest | Control::Bye => {}
        }
        buf[2] = self.kind();
        buf[0..2].copy_from_slice(&((n - 2) as u16).to_be_bytes());
        n
    }

    /// Decodes one small message from the start of `buf`; returns it and the
    /// bytes used. Cursor images need [`Message::decode`].
    pub fn decode(buf: &[u8]) -> Result<(Self, usize), ControlError> {
        let (kind, b, used) = decode_frame(buf)?;
        Self::decode_body(kind, b).map(|m| (m, used))
    }

    fn decode_body(kind: u8, b: &[u8]) -> Result<Self, ControlError> {
        let u16_at = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_be_bytes(b[i..i + 8].try_into().unwrap());
        let want = match kind {
            1 => 5,
            2 => 16,
            3 => 1,
            4 => 12,
            5 => 20,
            6 | 7 => 0,
            9 | 10 => 1,
            _ => return Err(ControlError::Malformed),
        };
        if b.len() != want {
            return Err(ControlError::Malformed);
        }
        let msg = match kind {
            1 => Control::Hello {
                version: b[0],
                display_w: u16_at(1),
                display_h: u16_at(3),
            },
            2 => Control::Welcome {
                version: b[0],
                info: StreamInfo {
                    codec: b[1],
                    chroma: b[2],
                    width: u16_at(4),
                    height: u16_at(6),
                    fps: u16_at(8),
                },
                udp_port: u16_at(10),
                session_id: u32_at(12),
            },
            3 => Control::Reject { reason: b[0] },
            4 => Control::Ping {
                seq: u32_at(0),
                client_ts: u64_at(4),
            },
            5 => Control::Pong {
                seq: u32_at(0),
                client_ts: u64_at(4),
                host_ts: u64_at(12),
            },
            6 => Control::KeyframeRequest,
            9 => Control::CursorVisible { visible: b[0] != 0 },
            10 => Control::HostNotice { code: b[0] },
            _ => Control::Bye,
        };
        Ok(msg)
    }
}

/// Largest control frame of any type (2-byte length + up to 65535).
pub const FRAME_MAX: usize = 2 + u16::MAX as usize;

/// Splits one frame off the front of `buf`: (type, body, bytes used).
pub fn decode_frame(buf: &[u8]) -> Result<(u8, &[u8], usize), ControlError> {
    if buf.len() < 2 {
        return Err(ControlError::Incomplete);
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    if len == 0 {
        return Err(ControlError::Malformed);
    }
    if buf.len() < len + 2 {
        return Err(ControlError::Incomplete);
    }
    Ok((buf[2], &buf[3..len + 2], len + 2))
}

/// Pointer images are capped at this size (larger ones are scaled down by the
/// host) so a shape fits one control frame.
pub const CURSOR_MAX_DIM: u16 = 96;
const KIND_CURSOR_SHAPE: u8 = 8;

/// Host -> client pointer shape: straight (non-premultiplied) RGBA, rows top
/// to bottom, hotspot in pixels from the top-left. Sent when it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorShape<'a> {
    pub serial: u32,
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: &'a [u8],
}

impl<'a> CursorShape<'a> {
    const HEADER: usize = 12;

    fn valid(&self) -> bool {
        (1..=CURSOR_MAX_DIM).contains(&self.width)
            && (1..=CURSOR_MAX_DIM).contains(&self.height)
            && self.rgba.len() == self.width as usize * self.height as usize * 4
    }

    /// Appends the encoded frame to `out`. False (nothing written) if invalid.
    pub fn encode(&self, out: &mut Vec<u8>) -> bool {
        if !self.valid() {
            return false;
        }
        let len = 1 + Self::HEADER + self.rgba.len();
        out.extend_from_slice(&(len as u16).to_be_bytes());
        out.push(KIND_CURSOR_SHAPE);
        out.extend_from_slice(&self.serial.to_be_bytes());
        for v in [self.width, self.height, self.hot_x, self.hot_y] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(self.rgba);
        true
    }

    fn decode_body(b: &'a [u8]) -> Option<Self> {
        if b.len() < Self::HEADER {
            return None;
        }
        let u16_at = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        let c = Self {
            serial: u32::from_be_bytes(b[0..4].try_into().ok()?),
            width: u16_at(4),
            height: u16_at(6),
            hot_x: u16_at(8),
            hot_y: u16_at(10),
            rgba: &b[Self::HEADER..],
        };
        c.valid().then_some(c)
    }
}

/// Any control-channel message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    Control(Control),
    Cursor(CursorShape<'a>),
}

impl<'a> Message<'a> {
    pub fn decode(buf: &'a [u8]) -> Result<(Self, usize), ControlError> {
        let (kind, b, used) = decode_frame(buf)?;
        let m = if kind == KIND_CURSOR_SHAPE {
            Message::Cursor(CursorShape::decode_body(b).ok_or(ControlError::Malformed)?)
        } else {
            Message::Control(Control::decode_body(kind, b)?)
        };
        Ok((m, used))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_round_trip() {
        let msgs = [
            Control::Hello {
                version: PROTOCOL_VERSION,
                display_w: 2940,
                display_h: 1912,
            },
            Control::Welcome {
                version: PROTOCOL_VERSION,
                info: StreamInfo {
                    codec: 1,
                    chroma: 1,
                    width: 2560,
                    height: 1440,
                    fps: 165,
                },
                udp_port: 47800,
                session_id: 0xA1B2_C3D4,
            },
            Control::Reject {
                reason: REJECT_BUSY,
            },
            Control::Ping {
                seq: 7,
                client_ts: u64::MAX - 1,
            },
            Control::Pong {
                seq: 7,
                client_ts: 1,
                host_ts: 2,
            },
            Control::KeyframeRequest,
            Control::Bye,
            Control::CursorVisible { visible: true },
            Control::HostNotice {
                code: NOTICE_INPUT_BLOCKED_ELEVATED,
            },
        ];
        // All messages back to back in one stream, as on TCP.
        let mut stream = Vec::new();
        let mut buf = [0u8; CONTROL_MAX];
        for m in &msgs {
            let n = m.encode(&mut buf);
            stream.extend_from_slice(&buf[..n]);
        }
        let mut rest = &stream[..];
        for m in &msgs {
            // A partial message is reported as incomplete, not malformed.
            assert_eq!(Control::decode(&rest[..1]), Err(ControlError::Incomplete));
            let (got, used) = Control::decode(rest).unwrap();
            assert_eq!(&got, m);
            rest = &rest[used..];
        }
        assert!(rest.is_empty());
    }

    #[test]
    fn control_rejects_garbage() {
        assert_eq!(Control::decode(&[0, 1, 99]), Err(ControlError::Malformed));
        // Ping with a short body.
        assert_eq!(
            Control::decode(&[0, 3, 4, 0, 0]),
            Err(ControlError::Malformed)
        );
        // Length says 65535 bytes follow: just incomplete.
        assert_eq!(
            Control::decode(&[0xff, 0xff]),
            Err(ControlError::Incomplete)
        );
        assert_eq!(Control::decode(&[0, 0]), Err(ControlError::Malformed));
    }

    #[test]
    fn cursor_shape_round_trip() {
        let rgba: Vec<u8> = (0..32 * 48 * 4).map(|i| i as u8).collect();
        let c = CursorShape {
            serial: 9,
            width: 32,
            height: 48,
            hot_x: 3,
            hot_y: 40,
            rgba: &rgba,
        };
        let mut buf = Vec::new();
        assert!(c.encode(&mut buf));
        let mut small = [0u8; CONTROL_MAX];
        let n = Control::Bye.encode(&mut small);
        buf.extend_from_slice(&small[..n]);
        let (m, used) = Message::decode(&buf).unwrap();
        assert_eq!(m, Message::Cursor(c));
        assert_eq!(
            Message::decode(&buf[used..]).unwrap().0,
            Message::Control(Control::Bye)
        );
        // Wrong pixel count, oversize, and a cursor frame via Control::decode.
        let bad = CursorShape {
            rgba: &rgba[4..],
            ..c
        };
        assert!(!bad.encode(&mut Vec::new()));
        let big = CursorShape { width: 200, ..c };
        assert!(!big.encode(&mut Vec::new()));
        assert_eq!(Control::decode(&buf), Err(ControlError::Malformed));
    }

    #[test]
    fn frame_info_and_punch() {
        let fi = FrameInfo {
            encode_us: 3_210,
            frame_len: 54_600,
            input_seq: 77,
        };
        assert_eq!(FrameInfo::from_bytes(&fi.to_bytes()), Some(fi));
        assert_eq!(FrameInfo::from_bytes(&[0; 11]), None);
        assert_eq!(decode_punch(&encode_punch(42)), Some(42));
        assert_eq!(decode_punch(b"GLPX\0\0\0\x2a"), None);
    }

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

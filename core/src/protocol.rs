//! Wire formats. `docs/protocol.md` is the source of truth; keep them in sync and
//! bump [`PROTOCOL_VERSION`] on any change.

/// Protocol version. 7 = M3d: `Hello` lists who sees the viewer, so a host
/// can refuse a connection loop (`REJECT_LOOP`). 6 = M3d: `Welcome` says what the host is (OS) and can
/// do (view modes, 4:4:4), so any host/viewer pair works the same way.
/// 5 = M3c: the viewer describes its screen and what it
/// can decode in `Hello`, picks a view mode (`SetMode`), and the host
/// announces new stream parameters mid-session (`StreamChange`).
/// v4 = M6: mutual TLS 1.3, pinned certificates, PIN pairing, AES-GCM sealed
/// datagrams. v3 = M3: input, cursor, `HostNotice`.
pub const PROTOCOL_VERSION: u8 = 15;

/// Most "seen by" ids a `Hello` carries (no-loop rule).
pub const MAX_SEEN_BY: usize = 8;

/// `Welcome::host_os`.
pub const HOST_OS_UNKNOWN: u8 = 0;
pub const HOST_OS_WINDOWS: u8 = 1;
pub const HOST_OS_MAC: u8 = 2;

/// `Welcome::host_caps` bits.
/// The host can show the "fit" view modes (a virtual display).
pub const HOST_CAP_VIEW_MODES: u8 = 1;
/// The host can encode 4:4:4 and switch to it mid-session (`SetMode`).
pub const HOST_CAP_444: u8 = 2;

/// Default TCP control port; the host's UDP media socket uses the same number.
pub const DEFAULT_PORT: u16 = 47800;

/// Frame is an IDR / keyframe.
pub const FLAG_IDR: u8 = 1 << 0;
/// Frame is a still-region refinement update.
pub const FLAG_REFINEMENT: u8 = 1 << 1;
/// This packet is a resend, asked for with a [`Nack`] (M4). Not part of the
/// frame's flags.
pub const FLAG_RESENT: u8 = 1 << 2;

/// Header at the start of every video packet. Fixed size, big-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoPacketHeader {
    pub stream_id: u8,
    pub flags: u8,
    pub frame_id: u32,
    pub packet_index: u16,
    pub packet_count: u16,
    /// FEC (M4): the frame's data packets are in `fec_groups` interleaved
    /// groups (packet i in group i mod groups), each with `fec_parity`
    /// parity packets after the data: `packet_index` from `packet_count` up.
    /// 0 and 0: no FEC.
    pub fec_groups: u8,
    pub fec_parity: u8,
    /// Host capture timestamp, microseconds on the host clock.
    pub capture_ts_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    TooShort,
    BadVersion(u8),
    /// packet_count == 0 or packet_index past the data and parity packets.
    BadIndex,
}

impl VideoPacketHeader {
    pub const SIZE: usize = 24;

    /// Writes the header into the first [`Self::SIZE`] bytes of `buf`.
    pub fn encode(&self, buf: &mut [u8]) -> Result<(), HeaderError> {
        if buf.len() < Self::SIZE {
            return Err(HeaderError::TooShort);
        }
        if !self.index_ok() {
            return Err(HeaderError::BadIndex);
        }
        buf[0] = PROTOCOL_VERSION;
        buf[1] = self.stream_id;
        buf[2] = self.flags;
        buf[3] = 0; // reserved
        buf[4..8].copy_from_slice(&self.frame_id.to_be_bytes());
        buf[8..10].copy_from_slice(&self.packet_index.to_be_bytes());
        buf[10..12].copy_from_slice(&self.packet_count.to_be_bytes());
        buf[12] = self.fec_groups;
        buf[13] = self.fec_parity;
        buf[14..16].copy_from_slice(&[0, 0]); // reserved
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
            fec_groups: buf[12],
            fec_parity: buf[13],
            capture_ts_us: u64::from_be_bytes(buf[16..24].try_into().unwrap()),
        };
        if !h.index_ok() {
            return Err(HeaderError::BadIndex);
        }
        Ok(h)
    }

    /// Data and parity packets in this packet's frame.
    pub fn total_packets(&self) -> usize {
        self.packet_count as usize + self.fec_groups as usize * self.fec_parity as usize
    }

    /// A parity packet (FEC), not data.
    pub fn is_parity(&self) -> bool {
        self.packet_index >= self.packet_count
    }

    fn index_ok(&self) -> bool {
        self.packet_count > 0 && (self.packet_index as usize) < self.total_packets()
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

/// UDP datagram viewer -> host asking for a frame's missing data packets
/// again (M4 NACK): `"GLNK"`, session id u32, frame id u32, count u8, then
/// `count` packet indices (u16). No indices: every data packet of the frame
/// (none of it came). Sealed with the viewer's key, like input.
pub const NACK_MAGIC: [u8; 4] = *b"GLNK";
/// At most this many packet indices in one NACK.
pub const NACK_MAX: usize = 128;
pub const NACK_HEADER: usize = 13;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nack<'a> {
    pub session_id: u32,
    pub frame_id: u32,
    /// Big-endian u16 packet indices (`count * 2` bytes).
    indices: &'a [u8],
}

impl Nack<'_> {
    /// Writes a NACK into `out` (at least `NACK_HEADER + 2 * NACK_MAX`
    /// bytes); returns its length. Indices past [`NACK_MAX`] are left out.
    pub fn encode(session_id: u32, frame_id: u32, indices: &[u16], out: &mut [u8]) -> usize {
        let n = indices.len().min(NACK_MAX);
        out[0..4].copy_from_slice(&NACK_MAGIC);
        out[4..8].copy_from_slice(&session_id.to_be_bytes());
        out[8..12].copy_from_slice(&frame_id.to_be_bytes());
        out[12] = n as u8;
        for (i, ix) in indices[..n].iter().enumerate() {
            out[NACK_HEADER + 2 * i..NACK_HEADER + 2 * i + 2].copy_from_slice(&ix.to_be_bytes());
        }
        NACK_HEADER + 2 * n
    }

    pub fn decode(b: &[u8]) -> Option<Nack<'_>> {
        if b.len() < NACK_HEADER || b[0..4] != NACK_MAGIC {
            return None;
        }
        let n = b[12] as usize;
        if n > NACK_MAX || b.len() != NACK_HEADER + 2 * n {
            return None;
        }
        Some(Nack {
            session_id: u32::from_be_bytes(b[4..8].try_into().ok()?),
            frame_id: u32::from_be_bytes(b[8..12].try_into().ok()?),
            indices: &b[NACK_HEADER..],
        })
    }

    /// The packet indices asked for (empty: the whole frame).
    pub fn indices(&self) -> impl Iterator<Item = u16> + '_ {
        self.indices
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
    }

    pub fn whole_frame(&self) -> bool {
        self.indices.is_empty()
    }
}

/// UDP datagram host -> viewer that keeps the viewer's radio awake while
/// no video is going (a still screen): `"GLKA"` + session id, every
/// [`KEEPALIVE_US`] the host sent nothing else (M4 step 4). The viewer's
/// punches do the same the other way.
pub const KEEPALIVE_MAGIC: [u8; 4] = *b"GLKA";
pub const KEEPALIVE_US: u64 = 10_000;

pub fn encode_keepalive(session_id: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[0..4].copy_from_slice(&KEEPALIVE_MAGIC);
    b[4..8].copy_from_slice(&session_id.to_be_bytes());
    b
}

pub fn is_keepalive(b: &[u8]) -> bool {
    b.len() == 8 && b[0..4] == KEEPALIVE_MAGIC
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
    /// `MODE_*` the host is streaming.
    pub mode: u8,
}

/// View modes (viewer's choice; the host may fall back to `MODE_HOST_SCREEN`).
/// The host's own screen, scaled on the viewer to fit.
pub const MODE_HOST_SCREEN: u8 = 0;
/// A host display at the viewer's pixel size; things look about as big as on
/// the host's own screen (Mac: HiDPI "looks like" viewer size / 1.5).
pub const MODE_FIT: u8 = 1;
/// A host display at the viewer's pixel size at 1x: most space, smaller text.
pub const MODE_FIT_MORE_SPACE: u8 = 2;
/// A host display at the viewer's pixel size at 2x: larger text.
pub const MODE_FIT_LARGER: u8 = 3;
pub const MODE_COUNT: u8 = 4;

/// `Hello::caps` bits.
/// The viewer can decode HEVC 4:4:4 (8-bit).
pub const CAP_HEVC_444: u8 = 1;
/// The viewer wants 4:2:0 to start with (faster; it may still switch to
/// 4:4:4 later with `SetMode`).
pub const CAP_PREFER_420: u8 = 2;

/// `HostNotice` codes: why the host can't do what the viewer expects.
pub const NOTICE_NONE: u8 = 0;
/// An administrator window is in front; Windows drops injected input (UIPI).
pub const NOTICE_INPUT_BLOCKED_ELEVATED: u8 = 1;
/// UAC prompt / lock screen: the secure desktop takes no input and isn't captured.
pub const NOTICE_INPUT_BLOCKED_SECURE_DESKTOP: u8 = 2;

/// Why a host refused a viewer.
pub const REJECT_VERSION: u8 = 1;
pub const REJECT_BUSY: u8 = 2;
/// The host doesn't know this viewer (never paired, or forgotten).
pub const REJECT_NOT_PAIRED: u8 = 3;
/// The host's user said no ("ask before each connection").
pub const REJECT_DENIED: u8 = 4;
/// Too many wrong PINs: pairing is off until the host restarts.
pub const REJECT_PAIRING_LOCKED: u8 = 5;
/// Wrong (or expired) PIN.
pub const REJECT_WRONG_PIN: u8 = 6;
/// The host is this machine, or already sees it: the connection would loop.
pub const REJECT_LOOP: u8 = 7;

/// Pairing messages (TLS control channel, before Hello; see `pairing.rs`).
/// Viewer -> host: `name` (UTF-8, ≤ 63 bytes).
pub const PAIR_START: u8 = 11;
/// Host -> viewer: `name`; the host is now showing a PIN.
pub const PAIR_READY: u8 = 12;
/// Either way: SPAKE2 message (33 bytes).
pub const PAIR_PAKE: u8 = 13;
/// Either way: key-confirmation HMAC-SHA256 (32 bytes).
pub const PAIR_CONFIRM: u8 = 14;

fn pair_body_ok(kind: u8, len: usize) -> bool {
    match kind {
        PAIR_START | PAIR_READY => len <= crate::identity::NAME_MAX,
        PAIR_PAKE => len == 33,
        PAIR_CONFIRM => len == 32,
        _ => false,
    }
}

/// Appends a pairing message frame to `out`. False if the body is invalid.
pub fn encode_pair(kind: u8, body: &[u8], out: &mut Vec<u8>) -> bool {
    if !pair_body_ok(kind, body.len()) {
        return false;
    }
    out.extend_from_slice(&((body.len() + 1) as u16).to_be_bytes());
    out.push(kind);
    out.extend_from_slice(body);
    true
}

/// Control-channel messages (TCP). Framing: `u16 length` (big-endian, covers
/// type + body) then `u8 type` then the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Client -> host, first message: the viewer's screen (pixels, Hz; 0 =
    /// unknown), what it can decode (`CAP_*`) and the view mode it wants.
    Hello {
        version: u8,
        display_w: u16,
        display_h: u16,
        refresh_hz: u16,
        /// The viewer's display scaling, percent (200 on a Retina Mac; 0 =
        /// unknown): a Windows host's virtual display uses the size it
        /// looks like (v15).
        scale_pct: u16,
        caps: u8,
        mode: u8,
        /// Machines that see the viewer (first 8 bytes of their
        /// fingerprints), `seen_count` of them.
        seen_count: u8,
        seen_by: [[u8; 8]; MAX_SEEN_BY],
    },
    /// Host -> client reply to Hello.
    Welcome {
        version: u8,
        info: StreamInfo,
        udp_port: u16,
        session_id: u32,
        /// `HOST_OS_*`.
        host_os: u8,
        /// `HOST_CAP_*` bits.
        host_caps: u8,
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
    /// Client -> host: switch to view mode `mode` (`MODE_*`); `allow_444`
    /// false asks for 4:2:0 (e.g. after 4:4:4 failed to decode).
    SetMode {
        mode: u8,
        allow_444: bool,
    },
    /// Host -> client: from the next IDR on, frames have these parameters.
    StreamChange {
        info: StreamInfo,
    },
    /// Client -> host, every 0.25 s (M4): data packets the viewer expected
    /// since the last report and how many of them didn't arrive (rebuilt by
    /// FEC or not). The host sizes its FEC from it.
    ReceiverReport {
        expected: u32,
        missing: u32,
    },
    /// Client -> host (M5 rate control): keep video at or below this many
    /// bits/s (0 = no limit: the host's own setting). Sent when it changes,
    /// and again every second.
    VideoRate {
        bits_per_s: u32,
    },
}

/// Largest encoded control message (including the 2-byte length).
pub const CONTROL_MAX: usize = 96;

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
            Control::SetMode { .. } => 15,
            Control::StreamChange { .. } => 16,
            Control::ReceiverReport { .. } => 22,
            Control::VideoRate { .. } => 23,
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
                refresh_hz,
                scale_pct,
                caps,
                mode,
                seen_count,
                seen_by,
            } => {
                put(&[version]);
                put(&display_w.to_be_bytes());
                put(&display_h.to_be_bytes());
                put(&refresh_hz.to_be_bytes());
                put(&scale_pct.to_be_bytes());
                let n = (seen_count as usize).min(MAX_SEEN_BY);
                put(&[caps, mode, n as u8]);
                for id in &seen_by[..n] {
                    put(id);
                }
            }
            Control::SetMode { mode, allow_444 } => put(&[mode, allow_444 as u8]),
            Control::ReceiverReport { expected, missing } => {
                put(&expected.to_be_bytes());
                put(&missing.to_be_bytes());
            }
            Control::VideoRate { bits_per_s } => put(&bits_per_s.to_be_bytes()),
            Control::StreamChange { info } => {
                put(&[info.codec, info.chroma, info.mode, 0]);
                put(&info.width.to_be_bytes());
                put(&info.height.to_be_bytes());
                put(&info.fps.to_be_bytes());
            }
            Control::Welcome {
                version,
                info,
                udp_port,
                session_id,
                host_os,
                host_caps,
            } => {
                put(&[version, info.codec, info.chroma, info.mode]);
                put(&info.width.to_be_bytes());
                put(&info.height.to_be_bytes());
                put(&info.fps.to_be_bytes());
                put(&udp_port.to_be_bytes());
                put(&session_id.to_be_bytes());
                put(&[host_os, host_caps]);
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
        // Hello: 12 bytes, then 8 per "seen by" id.
        if kind == 1 {
            let n = *b.get(11).ok_or(ControlError::Malformed)? as usize;
            if n > MAX_SEEN_BY || b.len() != 12 + 8 * n {
                return Err(ControlError::Malformed);
            }
            let mut seen_by = [[0u8; 8]; MAX_SEEN_BY];
            for (i, id) in seen_by.iter_mut().enumerate().take(n) {
                id.copy_from_slice(&b[12 + 8 * i..20 + 8 * i]);
            }
            return Ok(Control::Hello {
                version: b[0],
                display_w: u16_at(1),
                display_h: u16_at(3),
                refresh_hz: u16_at(5),
                scale_pct: u16_at(7),
                caps: b[9],
                mode: b[10],
                seen_count: n as u8,
                seen_by,
            });
        }
        let want = match kind {
            2 => 18,
            3 => 1,
            4 => 12,
            5 => 20,
            6 | 7 => 0,
            9 | 10 => 1,
            15 => 2,
            16 => 10,
            22 => 8,
            23 => 4,
            _ => return Err(ControlError::Malformed),
        };
        if b.len() != want {
            return Err(ControlError::Malformed);
        }
        let msg = match kind {
            2 => Control::Welcome {
                version: b[0],
                info: StreamInfo {
                    codec: b[1],
                    chroma: b[2],
                    mode: b[3],
                    width: u16_at(4),
                    height: u16_at(6),
                    fps: u16_at(8),
                },
                udp_port: u16_at(10),
                session_id: u32_at(12),
                host_os: b[16],
                host_caps: b[17],
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
            15 => Control::SetMode {
                mode: b[0],
                allow_444: b[1] != 0,
            },
            16 => Control::StreamChange {
                info: StreamInfo {
                    codec: b[0],
                    chroma: b[1],
                    mode: b[2],
                    width: u16_at(4),
                    height: u16_at(6),
                    fps: u16_at(8),
                },
            },
            22 => Control::ReceiverReport {
                expected: u32_at(0),
                missing: u32_at(4),
            },
            23 => Control::VideoRate {
                bits_per_s: u32_at(0),
            },
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

// Message types in use: 1-7, 8 (pointer shape), 9-10, 11-14 (pairing),
// 15-16, 17-21 (clipboard and files), 22 (receiver report).
const KIND_CLIP: u8 = 17;
const KIND_FILE_DATA: u8 = 18;
const KIND_FILE_CANCEL: u8 = 19;
const KIND_FILE_REQUEST: u8 = 20;
const KIND_FILE_RATE: u8 = 21;
/// Bytes of clipboard data per chunk (well inside a 64 KB frame).
pub const CLIP_CHUNK: usize = 60_000;

/// Either way: one chunk of a clipboard item (`clipboard.rs`). `id` names
/// the item, `total` is its size, `offset` where this chunk's `data` goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipChunk<'a> {
    pub id: u32,
    pub kind: u8,
    pub total: u32,
    pub offset: u32,
    pub data: &'a [u8],
}

impl<'a> ClipChunk<'a> {
    const HEADER: usize = 13;

    /// Appends the encoded frame to `out`. False (nothing written) if the
    /// chunk is too big for a frame.
    pub fn encode(&self, out: &mut Vec<u8>) -> bool {
        if self.data.len() > CLIP_CHUNK {
            return false;
        }
        let len = 1 + Self::HEADER + self.data.len();
        out.extend_from_slice(&(len as u16).to_be_bytes());
        out.push(KIND_CLIP);
        out.extend_from_slice(&self.id.to_be_bytes());
        out.push(self.kind);
        out.extend_from_slice(&self.total.to_be_bytes());
        out.extend_from_slice(&self.offset.to_be_bytes());
        out.extend_from_slice(self.data);
        true
    }

    fn decode_body(b: &'a [u8]) -> Option<Self> {
        if b.len() < Self::HEADER {
            return None;
        }
        let u32_at = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Some(Self {
            id: u32_at(0),
            kind: b[4],
            total: u32_at(5),
            offset: u32_at(9),
            data: &b[Self::HEADER..],
        })
    }
}

/// Either way: part of a copied file (`clipboard.rs`). `id` is the
/// transfer (its file list came as a `CLIP_FILES` item with that id),
/// `index` the file in that list, `offset` where `data` goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileData<'a> {
    pub id: u32,
    pub index: u32,
    pub offset: u64,
    pub data: &'a [u8],
}

impl<'a> FileData<'a> {
    const HEADER: usize = 16;

    /// Appends the encoded frame to `out`. False if `data` is too big.
    pub fn encode(&self, out: &mut Vec<u8>) -> bool {
        if self.data.len() > CLIP_CHUNK {
            return false;
        }
        let len = 1 + Self::HEADER + self.data.len();
        out.extend_from_slice(&(len as u16).to_be_bytes());
        out.push(KIND_FILE_DATA);
        out.extend_from_slice(&self.id.to_be_bytes());
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.offset.to_be_bytes());
        out.extend_from_slice(self.data);
        true
    }

    fn decode_body(b: &'a [u8]) -> Option<Self> {
        if b.len() < Self::HEADER {
            return None;
        }
        Some(Self {
            id: u32::from_be_bytes(b[0..4].try_into().ok()?),
            index: u32::from_be_bytes(b[4..8].try_into().ok()?),
            offset: u64::from_be_bytes(b[8..16].try_into().ok()?),
            data: &b[Self::HEADER..],
        })
    }
}

/// Appends a `FileCancel` frame: the sender gave up on transfer `id` (the
/// files changed, couldn't be read, or something newer was copied).
pub fn encode_file_cancel(id: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&5u16.to_be_bytes());
    out.push(KIND_FILE_CANCEL);
    out.extend_from_slice(&id.to_be_bytes());
}

/// Appends a `FileRequest` frame: something was pasted, send what offer
/// `id` stands for (files or a picture).
pub fn encode_file_request(id: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&5u16.to_be_bytes());
    out.push(KIND_FILE_REQUEST);
    out.extend_from_slice(&id.to_be_bytes());
}

/// Appends a `FileRate` frame: file data to this side may go at up to
/// `bytes_per_s` (the viewer steers it from how its picture is doing).
pub fn encode_file_rate(bytes_per_s: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&5u16.to_be_bytes());
    out.push(KIND_FILE_RATE);
    out.extend_from_slice(&bytes_per_s.to_be_bytes());
}

/// Any control-channel message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    Control(Control),
    Cursor(CursorShape<'a>),
    Clip(ClipChunk<'a>),
    FileData(FileData<'a>),
    /// The transfer with this id stopped part-way: drop what arrived.
    FileCancel(u32),
    /// Pasted: send the contents of offer `id`.
    FileRequest(u32),
    /// Viewer -> host: file data may go at up to this many bytes/s.
    FileRate(u32),
    /// `PAIR_*` kind and its body.
    Pair {
        kind: u8,
        body: &'a [u8],
    },
}

impl<'a> Message<'a> {
    pub fn decode(buf: &'a [u8]) -> Result<(Self, usize), ControlError> {
        let (kind, b, used) = decode_frame(buf)?;
        let m = if kind == KIND_CURSOR_SHAPE {
            Message::Cursor(CursorShape::decode_body(b).ok_or(ControlError::Malformed)?)
        } else if kind == KIND_CLIP {
            Message::Clip(ClipChunk::decode_body(b).ok_or(ControlError::Malformed)?)
        } else if kind == KIND_FILE_DATA {
            Message::FileData(FileData::decode_body(b).ok_or(ControlError::Malformed)?)
        } else if kind == KIND_FILE_CANCEL || kind == KIND_FILE_REQUEST || kind == KIND_FILE_RATE {
            let v = u32::from_be_bytes(b.try_into().map_err(|_| ControlError::Malformed)?);
            match kind {
                KIND_FILE_CANCEL => Message::FileCancel(v),
                KIND_FILE_REQUEST => Message::FileRequest(v),
                _ => Message::FileRate(v),
            }
        } else if (PAIR_START..=PAIR_CONFIRM).contains(&kind) {
            if !pair_body_ok(kind, b.len()) {
                return Err(ControlError::Malformed);
            }
            Message::Pair { kind, body: b }
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
    fn nack_round_trip() {
        let mut b = [0u8; NACK_HEADER + 2 * NACK_MAX];
        let n = Nack::encode(7, 0xDEAD_BEEF, &[3, 900, 65_000], &mut b);
        let k = Nack::decode(&b[..n]).unwrap();
        assert_eq!((k.session_id, k.frame_id), (7, 0xDEAD_BEEF));
        assert_eq!(k.indices().collect::<Vec<_>>(), vec![3, 900, 65_000]);
        assert!(!k.whole_frame());
        let n = Nack::encode(7, 1, &[], &mut b);
        assert!(Nack::decode(&b[..n]).unwrap().whole_frame());
        // Too many indices are cut to NACK_MAX; bad lengths are refused.
        let many: Vec<u16> = (0..300).collect();
        let n = Nack::encode(7, 1, &many, &mut b);
        assert_eq!(Nack::decode(&b[..n]).unwrap().indices().count(), NACK_MAX);
        assert!(Nack::decode(&b[..n - 1]).is_none());
        assert!(decode_punch(&b[..8]).is_none());
    }

    #[test]
    fn control_round_trip() {
        let msgs = [
            Control::Hello {
                version: PROTOCOL_VERSION,
                display_w: 2940,
                display_h: 1912,
                refresh_hz: 120,
                scale_pct: 200,
                caps: CAP_HEVC_444,
                mode: MODE_FIT_MORE_SPACE,
                seen_count: 2,
                seen_by: {
                    let mut s = [[0u8; 8]; MAX_SEEN_BY];
                    s[0] = [1, 2, 3, 4, 5, 6, 7, 8];
                    s[1] = [9; 8];
                    s
                },
            },
            Control::SetMode {
                mode: MODE_FIT,
                allow_444: false,
            },
            Control::ReceiverReport {
                expected: 1234,
                missing: 56,
            },
            Control::VideoRate {
                bits_per_s: 42_000_000,
            },
            Control::StreamChange {
                info: StreamInfo {
                    codec: 1,
                    chroma: 1,
                    width: 2560,
                    height: 1440,
                    fps: 60,
                    mode: MODE_FIT,
                },
            },
            Control::Welcome {
                version: PROTOCOL_VERSION,
                info: StreamInfo {
                    codec: 1,
                    chroma: 1,
                    width: 2560,
                    height: 1440,
                    fps: 165,
                    mode: MODE_HOST_SCREEN,
                },
                udp_port: 47800,
                session_id: 0xA1B2_C3D4,
                host_os: HOST_OS_MAC,
                host_caps: HOST_CAP_VIEW_MODES | HOST_CAP_444,
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
    fn pair_messages_round_trip() {
        let mut buf = Vec::new();
        assert!(encode_pair(PAIR_START, b"Simon's Mac", &mut buf));
        assert!(encode_pair(PAIR_PAKE, &[7; 33], &mut buf));
        assert!(encode_pair(PAIR_CONFIRM, &[9; 32], &mut buf));
        assert!(!encode_pair(PAIR_PAKE, &[7; 32], &mut buf));
        assert!(!encode_pair(PAIR_READY, &[b'x'; 64], &mut buf));
        let (m, n) = Message::decode(&buf).unwrap();
        assert_eq!(
            m,
            Message::Pair {
                kind: PAIR_START,
                body: b"Simon's Mac"
            }
        );
        let (m, n2) = Message::decode(&buf[n..]).unwrap();
        assert!(matches!(m, Message::Pair { kind: PAIR_PAKE, body } if body == [7; 33]));
        let (m, _) = Message::decode(&buf[n + n2..]).unwrap();
        assert!(matches!(
            m,
            Message::Pair {
                kind: PAIR_CONFIRM,
                ..
            }
        ));
        // Wrong length for the kind.
        assert_eq!(
            Message::decode(&[0, 3, PAIR_PAKE, 1, 2]),
            Err(ControlError::Malformed)
        );
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
            fec_groups: 2,
            fec_parity: 3,
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
        h.packet_index = h.packet_count + 5; // the last parity packet: fine
        h.encode(&mut buf).unwrap();
        h.packet_index = h.packet_count + 6; // past it
        assert_eq!(h.encode(&mut buf), Err(HeaderError::BadIndex));
        sample().encode(&mut buf).unwrap();
        buf[10..12].copy_from_slice(&0u16.to_be_bytes());
        assert_eq!(VideoPacketHeader::decode(&buf), Err(HeaderError::BadIndex));
    }
}

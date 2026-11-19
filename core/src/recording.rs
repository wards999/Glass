//! `.glsr` recording file: frame-delimited HEVC Annex-B with per-frame timing.
//! Format spec: `docs/recording.md`. Used by M1 (offline pipeline); M2 streams
//! the same per-frame data over UDP.

use std::io::{self, Write};

pub const MAGIC: [u8; 4] = *b"GLSR";
pub const FORMAT_VERSION: u16 = 1;
pub const CODEC_HEVC: u8 = 1;
pub const CHROMA_420: u8 = 0;
pub const CHROMA_444: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct RecordingHeader {
    pub codec: u8,
    pub chroma: u8,
    pub width: u32,
    pub height: u32,
    /// Nominal capture rate in frames per second (display refresh).
    pub fps: u32,
}

impl RecordingHeader {
    pub const SIZE: usize = 20;

    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&MAGIC);
        b[4..6].copy_from_slice(&FORMAT_VERSION.to_be_bytes());
        b[6] = self.codec;
        b[7] = self.chroma;
        b[8..12].copy_from_slice(&self.width.to_be_bytes());
        b[12..16].copy_from_slice(&self.height.to_be_bytes());
        b[16..20].copy_from_slice(&self.fps.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self, RecordingError> {
        if b.len() < Self::SIZE {
            return Err(RecordingError::Truncated);
        }
        if b[0..4] != MAGIC {
            return Err(RecordingError::BadMagic);
        }
        let v = u16::from_be_bytes([b[4], b[5]]);
        if v != FORMAT_VERSION {
            return Err(RecordingError::BadVersion);
        }
        Ok(Self {
            codec: b[6],
            chroma: b[7],
            width: be32(&b[8..]),
            height: be32(&b[12..]),
            fps: be32(&b[16..]),
        })
    }
}

/// Per-frame metadata preceding each frame's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct FrameMeta {
    pub frame_id: u32,
    /// `protocol::FLAG_*`.
    pub flags: u8,
    /// Host capture timestamp, microseconds.
    pub capture_ts_us: u64,
    /// Capture-arrival -> encoded packet out, microseconds.
    pub encode_us: u32,
    /// Length of the Annex-B payload that follows.
    pub len: u32,
}

impl FrameMeta {
    pub const SIZE: usize = 24;

    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.frame_id.to_be_bytes());
        b[4] = self.flags;
        b[8..16].copy_from_slice(&self.capture_ts_us.to_be_bytes());
        b[16..20].copy_from_slice(&self.encode_us.to_be_bytes());
        b[20..24].copy_from_slice(&self.len.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self, RecordingError> {
        if b.len() < Self::SIZE {
            return Err(RecordingError::Truncated);
        }
        Ok(Self {
            frame_id: be32(b),
            flags: b[4],
            capture_ts_us: u64::from_be_bytes(b[8..16].try_into().unwrap()),
            encode_us: be32(&b[16..]),
            len: be32(&b[20..]),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum RecordingError {
    Truncated = 1,
    BadMagic = 2,
    BadVersion = 3,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[0..4].try_into().unwrap())
}

/// Streams a recording to any writer. Not on the capture thread's hot path in
/// spirit (it does I/O), but it does not allocate.
pub struct RecordingWriter<W: Write> {
    w: W,
}

impl<W: Write> RecordingWriter<W> {
    pub fn new(mut w: W, header: &RecordingHeader) -> io::Result<Self> {
        w.write_all(&header.encode())?;
        Ok(Self { w })
    }

    pub fn write_frame(&mut self, meta: &FrameMeta, data: &[u8]) -> io::Result<()> {
        debug_assert_eq!(meta.len as usize, data.len());
        let mut m = *meta;
        m.len = data.len() as u32;
        self.w.write_all(&m.encode())?;
        self.w.write_all(data)
    }

    pub fn into_inner(self) -> W {
        self.w
    }
}

/// Zero-copy reader over a whole recording held in memory.
pub struct RecordingReader<'a> {
    pub header: RecordingHeader,
    rest: &'a [u8],
}

impl<'a> RecordingReader<'a> {
    pub fn new(buf: &'a [u8]) -> Result<Self, RecordingError> {
        let header = RecordingHeader::decode(buf)?;
        Ok(Self {
            header,
            rest: &buf[RecordingHeader::SIZE..],
        })
    }
}

impl<'a> Iterator for RecordingReader<'a> {
    type Item = Result<(FrameMeta, &'a [u8]), RecordingError>;

    fn next(&mut self) -> Option<Self::Item> {
        next_frame(&mut self.rest)
    }
}

/// Reads one frame from the front of `rest` and advances it. `None` at a clean
/// end of file.
pub fn next_frame<'a>(
    rest: &mut &'a [u8],
) -> Option<Result<(FrameMeta, &'a [u8]), RecordingError>> {
    if rest.is_empty() {
        return None;
    }
    let meta = match FrameMeta::decode(rest) {
        Ok(m) => m,
        Err(e) => return Some(Err(e)),
    };
    let start = FrameMeta::SIZE;
    let end = start + meta.len as usize;
    if rest.len() < end {
        return Some(Err(RecordingError::Truncated));
    }
    let data = &rest[start..end];
    *rest = &rest[end..];
    Some(Ok((meta, data)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> RecordingHeader {
        RecordingHeader {
            codec: CODEC_HEVC,
            chroma: CHROMA_444,
            width: 2560,
            height: 1440,
            fps: 120,
        }
    }

    #[test]
    fn round_trip() {
        let frames: [(u32, &[u8]); 3] = [(0, &[0, 0, 0, 1, 0x40, 1]), (1, &[0, 0, 1, 2]), (2, &[])];
        let mut w = RecordingWriter::new(Vec::new(), &header()).unwrap();
        for (id, data) in frames {
            let meta = FrameMeta {
                frame_id: id,
                flags: (id == 0) as u8,
                capture_ts_us: 1_000_000 + id as u64 * 8_333,
                encode_us: 3_000 + id,
                len: data.len() as u32,
            };
            w.write_frame(&meta, data).unwrap();
        }
        let buf = w.into_inner();
        let r = RecordingReader::new(&buf).unwrap();
        assert_eq!(r.header, header());
        let got: Vec<_> = r.map(|f| f.unwrap()).collect();
        assert_eq!(got.len(), 3);
        for (i, (meta, data)) in got.iter().enumerate() {
            assert_eq!(meta.frame_id, frames[i].0);
            assert_eq!(*data, frames[i].1);
            assert_eq!(meta.encode_us, 3_000 + i as u32);
        }
        assert_eq!(got[0].0.flags, 1);
    }

    #[test]
    fn rejects_bad_header_and_truncation() {
        let mut buf = header().encode().to_vec();
        buf[0] = b'X';
        assert_eq!(
            RecordingReader::new(&buf).err(),
            Some(RecordingError::BadMagic)
        );
        buf[0] = b'G';
        buf[5] = 99;
        assert_eq!(
            RecordingReader::new(&buf).err(),
            Some(RecordingError::BadVersion)
        );

        let mut w = RecordingWriter::new(Vec::new(), &header()).unwrap();
        let meta = FrameMeta {
            len: 4,
            ..Default::default()
        };
        w.write_frame(&meta, &[1, 2, 3, 4]).unwrap();
        let mut buf = w.into_inner();
        buf.pop();
        let mut r = RecordingReader::new(&buf).unwrap();
        assert_eq!(r.next(), Some(Err(RecordingError::Truncated)));
    }
}

//! The local link between a host's **broker**, which keeps the viewer's
//! connection, and its **helpers**, which do the work in the user's session
//! (ADR 0017). On Windows the broker is the Glass service and the helpers run
//! in whichever session is on screen. The screen helper captures, encodes and
//! injects input; the clipboard helper runs as the signed-in user. A helper
//! can restart (the user signs in, Remote Desktop takes over) while the
//! viewer's session carries on.
//!
//! Internal to one machine: the viewer never sees these messages, and they
//! aren't part of `docs/protocol.md`. Framing: `u32 length` (LE, kind + body)
//! · `u8 kind` · body. Integers in bodies are little-endian too. The transport
//! (named pipes on Windows) is the platform's.

use crate::input::InputAction;
use crate::net::ViewerHello;
use crate::protocol::{CursorShape, StreamInfo};
use std::io::{self, Read, Write};

/// The largest message (an encoded keyframe at a high bitrate fits easily).
pub const MAX_MESSAGE: usize = 16 << 20;

/// Broker → screen helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToHelper {
    /// A viewer is connected (or the helper is new mid-session): start
    /// capturing for it, keyframe first.
    Start {
        hello: ViewerHello,
        peer: String,
    },
    Keyframe,
    SetMode {
        mode: u8,
        allow_444: bool,
    },
    Input(InputAction),
    /// The viewer's input applied so far (for frames' `input_seq`).
    InputSeq(u32),
    /// The viewer left: stop, put the screen back, send the results.
    Stop,
    /// The viewer's video limit (bits/s, 0 = none), M5 rate control.
    Bitrate(u32),
}

/// Helper → broker. Borrowed from the read buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToBroker<'a> {
    /// The screen being captured (sent on connect and when it changes).
    Display {
        width: u16,
        height: u16,
        fps: u16,
    },
    Frame {
        key: bool,
        capture_us: u64,
        encode_us: u32,
        input_seq: u32,
        data: &'a [u8],
    },
    StreamChange(StreamInfo),
    Cursor(CursorShape<'a>),
    CursorVisible(bool),
    Notice(u8),
    /// The helper's part of a session's results.
    Report(&'a str),
    /// Clipboard helper: a clipboard control message (kinds 17–20 of the
    /// protocol) to pass to the viewer unchanged.
    ClipFrame(&'a [u8]),
    /// Clipboard helper: its results line.
    ClipSummary(&'a str),
}

const START: u8 = 1;
const KEYFRAME: u8 = 2;
const SET_MODE: u8 = 3;
const INPUT: u8 = 4;
const INPUT_SEQ: u8 = 5;
const STOP: u8 = 6;
const BITRATE: u8 = 8;
/// Broker → clipboard helper: a clipboard control message from the viewer.
pub const CLIP_TO_HELPER: u8 = 7;

const DISPLAY: u8 = 20;
const FRAME: u8 = 21;
const STREAM_CHANGE: u8 = 22;
const CURSOR: u8 = 23;
const CURSOR_VISIBLE: u8 = 24;
const NOTICE: u8 = 25;
const REPORT: u8 = 26;
const CLIP_FRAME: u8 = 40;
const CLIP_SUMMARY: u8 = 41;

fn begin(out: &mut Vec<u8>, kind: u8) {
    out.clear();
    out.extend_from_slice(&[0; 4]);
    out.push(kind);
}

fn finish(out: &mut [u8]) {
    let n = (out.len() - 4) as u32;
    out[..4].copy_from_slice(&n.to_le_bytes());
}

/// A reader over a message body.
struct Body<'a>(&'a [u8]);

impl<'a> Body<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn rest(self) -> &'a [u8] {
        self.0
    }
}

impl ToHelper {
    /// The whole framed message, into `out` (cleared first).
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            ToHelper::Start { hello, peer } => {
                begin(out, START);
                out.extend_from_slice(&hello.display_w.to_le_bytes());
                out.extend_from_slice(&hello.display_h.to_le_bytes());
                out.extend_from_slice(&hello.refresh_hz.to_le_bytes());
                out.push(hello.caps);
                out.push(hello.mode);
                out.extend_from_slice(peer.as_bytes());
            }
            ToHelper::Keyframe => begin(out, KEYFRAME),
            ToHelper::SetMode { mode, allow_444 } => {
                begin(out, SET_MODE);
                out.push(*mode);
                out.push(*allow_444 as u8);
            }
            ToHelper::Input(a) => {
                begin(out, INPUT);
                match *a {
                    InputAction::MoveTo { x, y } => {
                        out.push(0);
                        out.extend_from_slice(&x.to_le_bytes());
                        out.extend_from_slice(&y.to_le_bytes());
                    }
                    InputAction::Button { button, down } => {
                        out.extend_from_slice(&[1, button, down as u8]);
                    }
                    InputAction::Key { hid, down } => out.extend_from_slice(&[2, hid, down as u8]),
                    InputAction::Scroll { dy, dx } => {
                        out.push(3);
                        out.extend_from_slice(&dy.to_le_bytes());
                        out.extend_from_slice(&dx.to_le_bytes());
                    }
                }
            }
            ToHelper::InputSeq(s) => {
                begin(out, INPUT_SEQ);
                out.extend_from_slice(&s.to_le_bytes());
            }
            ToHelper::Stop => begin(out, STOP),
            ToHelper::Bitrate(b) => {
                begin(out, BITRATE);
                out.extend_from_slice(&b.to_le_bytes());
            }
        }
        finish(out);
    }

    pub fn decode(kind: u8, body: &[u8]) -> Option<Self> {
        let mut b = Body(body);
        Some(match kind {
            START => {
                let hello = ViewerHello {
                    display_w: b.u16()?,
                    display_h: b.u16()?,
                    refresh_hz: b.u16()?,
                    caps: b.u8()?,
                    mode: b.u8()?,
                };
                let peer = String::from_utf8_lossy(b.rest()).into_owned();
                ToHelper::Start { hello, peer }
            }
            KEYFRAME => ToHelper::Keyframe,
            SET_MODE => ToHelper::SetMode {
                mode: b.u8()?,
                allow_444: b.u8()? != 0,
            },
            INPUT => ToHelper::Input(match b.u8()? {
                0 => InputAction::MoveTo {
                    x: b.u16()?,
                    y: b.u16()?,
                },
                1 => InputAction::Button {
                    button: b.u8()?,
                    down: b.u8()? != 0,
                },
                2 => InputAction::Key {
                    hid: b.u8()?,
                    down: b.u8()? != 0,
                },
                3 => InputAction::Scroll {
                    dy: b.i16()?,
                    dx: b.i16()?,
                },
                _ => return None,
            }),
            INPUT_SEQ => ToHelper::InputSeq(b.u32()?),
            STOP => ToHelper::Stop,
            BITRATE => ToHelper::Bitrate(b.u32()?),
            _ => return None,
        })
    }
}

impl<'a> ToBroker<'a> {
    /// The whole framed message, into `out` (cleared first).
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            ToBroker::Display { width, height, fps } => {
                begin(out, DISPLAY);
                for v in [width, height, fps] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            ToBroker::Frame {
                key,
                capture_us,
                encode_us,
                input_seq,
                data,
            } => {
                begin(out, FRAME);
                out.push(*key as u8);
                out.extend_from_slice(&capture_us.to_le_bytes());
                out.extend_from_slice(&encode_us.to_le_bytes());
                out.extend_from_slice(&input_seq.to_le_bytes());
                out.extend_from_slice(data);
            }
            ToBroker::StreamChange(i) => {
                begin(out, STREAM_CHANGE);
                out.extend_from_slice(&[i.codec, i.chroma]);
                for v in [i.width, i.height, i.fps] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.push(i.mode);
            }
            ToBroker::Cursor(c) => {
                begin(out, CURSOR);
                out.extend_from_slice(&c.serial.to_le_bytes());
                for v in [c.width, c.height, c.hot_x, c.hot_y] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.extend_from_slice(c.rgba);
            }
            ToBroker::CursorVisible(v) => {
                begin(out, CURSOR_VISIBLE);
                out.push(*v as u8);
            }
            ToBroker::Notice(n) => {
                begin(out, NOTICE);
                out.push(*n);
            }
            ToBroker::Report(t) => {
                begin(out, REPORT);
                out.extend_from_slice(t.as_bytes());
            }
            ToBroker::ClipFrame(f) => {
                begin(out, CLIP_FRAME);
                out.extend_from_slice(f);
            }
            ToBroker::ClipSummary(t) => {
                begin(out, CLIP_SUMMARY);
                out.extend_from_slice(t.as_bytes());
            }
        }
        finish(out);
    }

    pub fn decode(kind: u8, body: &'a [u8]) -> Option<Self> {
        let mut b = Body(body);
        Some(match kind {
            DISPLAY => ToBroker::Display {
                width: b.u16()?,
                height: b.u16()?,
                fps: b.u16()?,
            },
            FRAME => ToBroker::Frame {
                key: b.u8()? != 0,
                capture_us: b.u64()?,
                encode_us: b.u32()?,
                input_seq: b.u32()?,
                data: b.rest(),
            },
            STREAM_CHANGE => ToBroker::StreamChange(StreamInfo {
                codec: b.u8()?,
                chroma: b.u8()?,
                width: b.u16()?,
                height: b.u16()?,
                fps: b.u16()?,
                mode: b.u8()?,
            }),
            CURSOR => ToBroker::Cursor(CursorShape {
                serial: b.u32()?,
                width: b.u16()?,
                height: b.u16()?,
                hot_x: b.u16()?,
                hot_y: b.u16()?,
                rgba: b.rest(),
            }),
            CURSOR_VISIBLE => ToBroker::CursorVisible(b.u8()? != 0),
            NOTICE => ToBroker::Notice(b.u8()?),
            REPORT => ToBroker::Report(std::str::from_utf8(b.rest()).ok()?),
            CLIP_FRAME => ToBroker::ClipFrame(b.rest()),
            CLIP_SUMMARY => ToBroker::ClipSummary(std::str::from_utf8(b.rest()).ok()?),
            _ => return None,
        })
    }
}

/// Broker → clipboard helper: a clipboard control message from the viewer.
pub fn encode_clip_to_helper(frame: &[u8], out: &mut Vec<u8>) {
    begin(out, CLIP_TO_HELPER);
    out.extend_from_slice(frame);
    finish(out);
}

/// Reads one message into `buf` (its body; the buffer is reused). Returns
/// its kind.
pub fn read_message(r: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<u8> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let n = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
    if n == 0 || n > MAX_MESSAGE {
        return Err(io::ErrorKind::InvalidData.into());
    }
    buf.resize(n - 1, 0);
    r.read_exact(buf)?;
    Ok(head[4])
}

/// Writes an encoded message (from `encode`).
pub fn write_message(w: &mut impl Write, msg: &[u8]) -> io::Result<()> {
    w.write_all(msg)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn through<T>(encoded: &[u8], decode: impl Fn(u8, &[u8]) -> Option<T>) -> T {
        let mut r = encoded;
        let mut buf = Vec::new();
        let kind = read_message(&mut r, &mut buf).unwrap();
        assert!(r.is_empty(), "one whole message");
        decode(kind, &buf).expect("decodes")
    }

    #[test]
    fn broker_to_helper_round_trips() {
        let msgs = [
            ToHelper::Start {
                hello: ViewerHello {
                    display_w: 2560,
                    display_h: 1440,
                    refresh_hz: 165,
                    caps: 3,
                    mode: 1,
                },
                peer: "QUIETPC22 ✓".into(),
            },
            ToHelper::Keyframe,
            ToHelper::SetMode {
                mode: 2,
                allow_444: true,
            },
            ToHelper::Input(InputAction::MoveTo { x: 1, y: 65535 }),
            ToHelper::Input(InputAction::Button {
                button: 2,
                down: true,
            }),
            ToHelper::Input(InputAction::Key {
                hid: 0xE3,
                down: false,
            }),
            ToHelper::Input(InputAction::Scroll { dy: -120, dx: 7 }),
            ToHelper::InputSeq(u32::MAX),
            ToHelper::Bitrate(17_000_000),
            ToHelper::Stop,
        ];
        let mut out = Vec::new();
        for m in msgs {
            m.encode(&mut out);
            assert_eq!(through(&out, ToHelper::decode), m);
        }
    }

    #[test]
    fn helper_to_broker_round_trips() {
        let rgba = vec![9u8; 4 * 4 * 4];
        let msgs = [
            ToBroker::Display {
                width: 1280,
                height: 1024,
                fps: 60,
            },
            ToBroker::Frame {
                key: true,
                capture_us: 1 << 40,
                encode_us: 6_500,
                input_seq: 77,
                data: &[0, 0, 0, 1, 0x40],
            },
            ToBroker::StreamChange(StreamInfo {
                codec: 1,
                chroma: 3,
                width: 2560,
                height: 1440,
                fps: 60,
                mode: 0,
            }),
            ToBroker::Cursor(CursorShape {
                serial: 5,
                width: 4,
                height: 4,
                hot_x: 1,
                hot_y: 2,
                rgba: &rgba,
            }),
            ToBroker::CursorVisible(false),
            ToBroker::Notice(2),
            ToBroker::Report("  encode p50 7 ms\n"),
            ToBroker::ClipFrame(&[0, 5, 19, 0, 0, 0, 9]),
            ToBroker::ClipSummary("text and pictures: 1 sent"),
        ];
        let mut out = Vec::new();
        for m in msgs {
            m.encode(&mut out);
            let back = through(&out, |k, b| {
                ToBroker::decode(k, b).map(|m| format!("{m:?}"))
            });
            assert_eq!(back, format!("{m:?}"));
        }
    }

    #[test]
    fn bad_input_is_refused() {
        let mut buf = Vec::new();
        // Zero length, and a length over the limit.
        assert!(read_message(&mut &[0u8, 0, 0, 0, 1][..], &mut buf).is_err());
        let huge = ((MAX_MESSAGE + 1) as u32).to_le_bytes();
        assert!(read_message(&mut &[huge[0], huge[1], huge[2], huge[3], 1][..], &mut buf).is_err());
        // A truncated body, and an unknown kind.
        assert_eq!(ToHelper::decode(SET_MODE, &[1]), None);
        assert_eq!(ToHelper::decode(99, &[]), None);
        assert_eq!(ToBroker::decode(FRAME, &[1, 2]), None);
    }
}

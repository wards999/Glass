//! Clipboard sharing (M5c, step 1): text and images, both directions,
//! over the session's encrypted control connection.
//!
//! Each side watches its own clipboard; when it changes, the new contents go
//! to the other side as one *item* — UTF-8 text or a PNG image — which is
//! put on that machine's clipboard. Items can be large (screenshots), and a
//! control frame holds at most 64 KB, so an item travels as numbered chunks
//! written one at a time from a helper thread: pings, pointer shapes and the
//! rest of the control traffic keep flowing in between. Items over
//! [`CLIP_MAX`] aren't sent.
//!
//! Echo: each app remembers what it last put on its clipboard and doesn't
//! send that back.

use crate::protocol::{ClipChunk, CLIP_CHUNK};

/// UTF-8 text.
pub const CLIP_TEXT: u8 = 1;
/// A PNG image.
pub const CLIP_PNG: u8 = 2;
/// The largest item that's sent (16 MB).
pub const CLIP_MAX: usize = 16 << 20;

/// One clipboard item: `CLIP_*` kind and its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClipItem {
    pub kind: u8,
    pub data: Vec<u8>,
}

/// Splits an item into the chunks that carry it (the caller writes each
/// one as its own frame). Empty if the item is empty or too big.
pub fn chunks(id: u32, item: &ClipItem) -> Vec<ClipChunk<'_>> {
    if item.data.is_empty() || item.data.len() > CLIP_MAX {
        return Vec::new();
    }
    let total = item.data.len() as u32;
    item.data
        .chunks(CLIP_CHUNK)
        .enumerate()
        .map(|(i, d)| ClipChunk {
            id,
            kind: item.kind,
            total,
            offset: (i * CLIP_CHUNK) as u32,
            data: d,
        })
        .collect()
}

/// Puts an item back together from its chunks, in order. A chunk that
/// doesn't continue the current item (a new id, a gap) starts over, so a
/// half-sent item is dropped, never delivered.
#[derive(Default)]
pub struct Reassembler {
    id: u32,
    kind: u8,
    total: usize,
    buf: Vec<u8>,
}

impl Reassembler {
    /// Feeds one chunk; returns the item when it's complete.
    pub fn push(&mut self, c: &ClipChunk) -> Option<ClipItem> {
        let total = c.total as usize;
        if total == 0 || total > CLIP_MAX {
            return None;
        }
        if c.offset == 0 {
            self.id = c.id;
            self.kind = c.kind;
            self.total = total;
            self.buf.clear();
            self.buf.reserve(total);
        } else if c.id != self.id || c.offset as usize != self.buf.len() || total != self.total {
            self.buf.clear();
            self.total = 0;
            return None;
        }
        if self.total == 0 || self.buf.len() + c.data.len() > self.total {
            self.buf.clear();
            self.total = 0;
            return None;
        }
        self.buf.extend_from_slice(c.data);
        if self.buf.len() == self.total {
            self.total = 0;
            return Some(ClipItem {
                kind: self.kind,
                data: std::mem::take(&mut self.buf),
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Message;

    fn round_trip(item: &ClipItem) -> Option<ClipItem> {
        let mut r = Reassembler::default();
        let mut out = None;
        for c in chunks(7, item) {
            let mut wire = Vec::new();
            assert!(c.encode(&mut wire));
            let (m, used) = Message::decode(&wire).unwrap();
            assert_eq!(used, wire.len());
            let Message::Clip(back) = m else {
                panic!("not a clip chunk")
            };
            assert_eq!(back, c);
            if let Some(i) = r.push(&back) {
                out = Some(i);
            }
        }
        out
    }

    #[test]
    fn text_and_big_images_survive_chunking() {
        let text = ClipItem {
            kind: CLIP_TEXT,
            data: "Glass ✓ clipboard".as_bytes().to_vec(),
        };
        assert_eq!(round_trip(&text), Some(text));
        // Several chunks, ending part-way through one.
        let big: Vec<u8> = (0..(CLIP_CHUNK * 3 + 1234))
            .map(|i| (i % 251) as u8)
            .collect();
        let png = ClipItem {
            kind: CLIP_PNG,
            data: big,
        };
        assert_eq!(round_trip(&png), Some(png));
    }

    #[test]
    fn too_big_or_empty_items_are_not_sent() {
        assert!(chunks(
            1,
            &ClipItem {
                kind: CLIP_TEXT,
                data: Vec::new()
            }
        )
        .is_empty());
        let huge = ClipItem {
            kind: CLIP_PNG,
            data: vec![0; CLIP_MAX + 1],
        };
        assert!(chunks(1, &huge).is_empty());
    }

    #[test]
    fn a_broken_item_is_dropped_not_delivered() {
        let data: Vec<u8> = vec![5; CLIP_CHUNK * 2 + 10];
        let item = ClipItem {
            kind: CLIP_PNG,
            data,
        };
        let cs = chunks(3, &item);
        let mut r = Reassembler::default();
        // The middle chunk is lost: the last one doesn't continue the item.
        assert_eq!(r.push(&cs[0]), None);
        assert_eq!(r.push(&cs[2]), None);
        // A whole new item afterwards still comes through.
        let text = ClipItem {
            kind: CLIP_TEXT,
            data: b"next".to_vec(),
        };
        let mut got = None;
        for c in chunks(4, &text) {
            got = r.push(&c).or(got);
        }
        assert_eq!(got, Some(text));
    }
}

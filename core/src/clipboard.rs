//! Clipboard sharing (M5c): text, images and files, both directions, over
//! the session's encrypted control connection.
//!
//! Each side watches its own clipboard; when it changes, the new contents go
//! to the other side: one *item* (UTF-8 text or a PNG image), or a set of
//! *files*. Items are put on that machine's clipboard. Files are written into
//! a Glass folder there, and once they've all arrived the clipboard holds
//! them as real files, so an ordinary paste works in Explorer or Finder.
//!
//! A control frame holds at most 64 KB, so everything travels as chunks.
//! One sender thread per session writes them a frame at a time, so pings,
//! pointer shapes and the rest of the control traffic keep flowing in
//! between. Only the newest copy is sent: copying again stops what was
//! being sent. File data is paced ([`FILE_RATE`]) so the picture keeps the
//! network. LATER(M5): pace from the rate controller instead.
//!
//! Files: the list (a [`CLIP_FILES`] item: names, sizes, folders) comes
//! first, then each file's bytes in order as `FileData`. A copy over
//! [`FILES_MAX`] isn't sent. LATER(M5c): send big copies only when pasted.
//!
//! Echo: each app remembers what it last put on its clipboard and doesn't
//! send that back.

use crate::protocol::{encode_file_cancel, ClipChunk, FileData, Message, CLIP_CHUNK};
use crate::tls::TlsWriter;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// UTF-8 text.
pub const CLIP_TEXT: u8 = 1;
/// A PNG image.
pub const CLIP_PNG: u8 = 2;
/// A file list (the start of a file transfer).
pub const CLIP_FILES: u8 = 3;
/// The largest item that's sent (16 MB).
pub const CLIP_MAX: usize = 16 << 20;
/// The largest copy of files that's sent (200 MB in all).
pub const FILES_MAX: u64 = 200 << 20;
/// At most this many files and folders in one copy.
pub const FILES_MAX_ENTRIES: usize = 10_000;
/// File data goes at most this fast (bytes/s, about 60 Mbps).
pub const FILE_RATE: u64 = 7_500_000;

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

    /// The id of the item last started.
    pub fn id(&self) -> u32 {
        self.id
    }
}

// ---- File lists ------------------------------------------------------------

/// One entry of a file list: a path relative to the copy, `/`-separated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub dir: bool,
    pub size: u64,
}

/// `version u8 (1)`, `count u32`, then per entry `dir u8`, `size u64`,
/// `path_len u16`, path (UTF-8).
pub fn encode_list(entries: &[Entry]) -> Vec<u8> {
    let mut b = vec![1];
    b.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        b.push(e.dir as u8);
        b.extend_from_slice(&e.size.to_be_bytes());
        b.extend_from_slice(&(e.path.len() as u16).to_be_bytes());
        b.extend_from_slice(e.path.as_bytes());
    }
    b
}

pub fn decode_list(b: &[u8]) -> Option<Vec<Entry>> {
    if b.first() != Some(&1) || b.len() < 5 {
        return None;
    }
    let count = u32::from_be_bytes(b[1..5].try_into().ok()?) as usize;
    if count > FILES_MAX_ENTRIES {
        return None;
    }
    let mut i = 5;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let head = b.get(i..i + 11)?;
        let size = u64::from_be_bytes(head[1..9].try_into().ok()?);
        let len = u16::from_be_bytes([head[9], head[10]]) as usize;
        let path = std::str::from_utf8(b.get(i + 11..i + 11 + len)?).ok()?;
        out.push(Entry {
            path: path.to_string(),
            dir: head[0] != 0,
            size,
        });
        i += 11 + len;
    }
    (i == b.len()).then_some(out)
}

/// Why a copy of files wasn't sent.
#[derive(Debug, PartialEq, Eq)]
pub enum NotSent {
    TooBig(u64),
    TooMany,
    Unreadable,
}

/// The file list for a copy of these paths (folders included, with all
/// they contain; links aren't followed), and the source of each entry.
pub fn collect(paths: &[PathBuf]) -> Result<(Vec<Entry>, Vec<PathBuf>), NotSent> {
    fn walk(
        src: &Path,
        rel: String,
        entries: &mut Vec<Entry>,
        sources: &mut Vec<PathBuf>,
        total: &mut u64,
    ) -> Result<(), NotSent> {
        if entries.len() >= FILES_MAX_ENTRIES {
            return Err(NotSent::TooMany);
        }
        let md = std::fs::symlink_metadata(src).map_err(|_| NotSent::Unreadable)?;
        if md.is_dir() {
            entries.push(Entry {
                path: rel.clone(),
                dir: true,
                size: 0,
            });
            sources.push(src.to_path_buf());
            let mut kids: Vec<_> = std::fs::read_dir(src)
                .map_err(|_| NotSent::Unreadable)?
                .filter_map(|e| e.ok())
                .collect();
            kids.sort_by_key(|e| e.file_name());
            for k in kids {
                let name = k.file_name().to_string_lossy().into_owned();
                walk(&k.path(), format!("{rel}/{name}"), entries, sources, total)?;
            }
        } else if md.is_file() {
            *total += md.len();
            if *total > FILES_MAX {
                return Err(NotSent::TooBig(*total));
            }
            entries.push(Entry {
                path: rel,
                dir: false,
                size: md.len(),
            });
            sources.push(src.to_path_buf());
        } // links and other special files are skipped
        Ok(())
    }
    let (mut entries, mut sources, mut total) = (Vec::new(), Vec::new(), 0);
    for p in paths {
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        walk(p, name, &mut entries, &mut sources, &mut total)?;
    }
    if entries.is_empty() {
        return Err(NotSent::Unreadable);
    }
    Ok((entries, sources))
}

/// A received path made safe to create under the Glass folder: no `..`,
/// no absolute paths, and no characters or names Windows can't store.
pub fn safe_path(rel: &str) -> Option<PathBuf> {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let mut out = PathBuf::new();
    for part in rel.split('/') {
        let mut c: String = part
            .chars()
            .map(|ch| {
                if ch.is_control() || "<>:\"\\|?*".contains(ch) {
                    '_'
                } else {
                    ch
                }
            })
            .collect();
        while c.ends_with('.') || c.ends_with(' ') {
            c.pop();
        }
        if c.is_empty() {
            return None;
        }
        let stem = c.split('.').next().unwrap_or("").to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            c.insert(0, '_');
        }
        out.push(c);
    }
    Some(out)
}

// ---- The link: one per session --------------------------------------------

enum Job {
    Item(ClipItem),
    Files(Vec<PathBuf>),
}

/// Transfer progress and counts, for the apps (bytes; `*_total` 0 = none).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClipProgress {
    pub in_done: u64,
    pub in_total: u64,
    pub out_done: u64,
    pub out_total: u64,
    /// Copies of files not sent (too big, too many files or unreadable).
    pub not_sent: u32,
    pub files_sent: u32,
    pub files_received: u32,
}

/// A file transfer being received.
struct Inbound {
    id: u32,
    base: PathBuf,
    /// Every list entry, in order: where it goes and its size (0 for
    /// folders). `FileData` refers to entries by index.
    files: Vec<(PathBuf, u64)>,
    tops: Vec<PathBuf>,
    next: usize,
    open: Option<std::fs::File>,
    written: u64,
}

#[derive(Default)]
struct Rx {
    clip: Reassembler,
    inbound: Option<Inbound>,
    /// The last complete transfer's folder: kept, its files are on the
    /// clipboard.
    keep: Option<PathBuf>,
}

/// Clipboard state for one session, both ways.
pub struct ClipLink {
    job: Mutex<Option<Job>>,
    wake: Condvar,
    /// Bumped on every new copy: the sender stops on a change.
    gen: AtomicU32,
    sender: Mutex<bool>,
    rx: Mutex<Rx>,
    item_in: Mutex<Option<ClipItem>>,
    files_in: Mutex<Option<Vec<PathBuf>>>,
    /// Where received files go; `None`: files aren't accepted.
    file_dir: Mutex<Option<PathBuf>>,
    in_done: AtomicU64,
    in_total: AtomicU64,
    out_done: AtomicU64,
    out_total: AtomicU64,
    not_sent: AtomicU32,
    files_sent: AtomicU32,
    files_received: AtomicU32,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Default for ClipLink {
    fn default() -> Self {
        Self {
            job: Mutex::new(None),
            wake: Condvar::new(),
            gen: AtomicU32::new(0),
            sender: Mutex::new(false),
            rx: Mutex::new(Rx::default()),
            item_in: Mutex::new(None),
            files_in: Mutex::new(None),
            file_dir: Mutex::new(None),
            in_done: AtomicU64::new(0),
            in_total: AtomicU64::new(0),
            out_done: AtomicU64::new(0),
            out_total: AtomicU64::new(0),
            not_sent: AtomicU32::new(0),
            files_sent: AtomicU32::new(0),
            files_received: AtomicU32::new(0),
        }
    }
}

impl ClipLink {
    fn queue(self: &Arc<Self>, w: &TlsWriter, job: Job) {
        self.gen.fetch_add(1, Ordering::SeqCst);
        *lock(&self.job) = Some(job);
        self.wake.notify_one();
        let mut started = lock(&self.sender);
        if !*started {
            *started = true;
            let (me, w) = (self.clone(), w.clone());
            let _ = std::thread::Builder::new()
                .name("glass-clip-send".into())
                .spawn(move || me.send_loop(w));
        }
    }

    /// Sends an item (replacing anything still being sent).
    pub(crate) fn send_item(self: &Arc<Self>, w: &TlsWriter, item: ClipItem) {
        self.queue(w, Job::Item(item));
    }

    /// Sends copies of these files and folders (replacing anything still
    /// being sent). The list is made and checked on the sender thread.
    pub(crate) fn send_files(self: &Arc<Self>, w: &TlsWriter, paths: Vec<PathBuf>) {
        self.queue(w, Job::Files(paths));
    }

    pub fn take_item(&self) -> Option<ClipItem> {
        lock(&self.item_in).take()
    }

    /// Received files, once all of them are there: the top-level files and
    /// folders, for the clipboard.
    pub fn take_files(&self) -> Option<Vec<PathBuf>> {
        lock(&self.files_in).take()
    }

    /// Accept files into `dir` (`None`: refuse them).
    pub fn set_file_dir(&self, dir: Option<PathBuf>) {
        *lock(&self.file_dir) = dir;
    }

    pub fn progress(&self) -> ClipProgress {
        ClipProgress {
            in_done: self.in_done.load(Ordering::Relaxed),
            in_total: self.in_total.load(Ordering::Relaxed),
            out_done: self.out_done.load(Ordering::Relaxed),
            out_total: self.out_total.load(Ordering::Relaxed),
            not_sent: self.not_sent.load(Ordering::Relaxed),
            files_sent: self.files_sent.load(Ordering::Relaxed),
            files_received: self.files_received.load(Ordering::Relaxed),
        }
    }

    // -- sending (its own thread) --

    fn send_loop(self: Arc<Self>, w: TlsWriter) {
        let mut frame = Vec::with_capacity(CLIP_CHUNK + 32);
        loop {
            let job = {
                let mut j = lock(&self.job);
                loop {
                    if let Some(job) = j.take() {
                        break job;
                    }
                    // Nobody else holds the link: the session is over.
                    if Arc::strong_count(&self) == 1 {
                        return;
                    }
                    j = self
                        .wake
                        .wait_timeout(j, Duration::from_millis(500))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            };
            let id = self.gen.load(Ordering::SeqCst);
            let ok = match job {
                Job::Item(item) => self.send_chunks(&w, id, &item, &mut frame),
                Job::Files(paths) => self.send_copy(&w, id, &paths, &mut frame),
            };
            if !ok {
                return; // the connection is gone
            }
        }
    }

    fn stale(&self, id: u32) -> bool {
        self.gen.load(Ordering::SeqCst) != id
    }

    /// Writes an item's chunks; true unless writing failed.
    fn send_chunks(&self, w: &TlsWriter, id: u32, item: &ClipItem, frame: &mut Vec<u8>) -> bool {
        for c in chunks(id, item) {
            if self.stale(id) {
                return true; // the next item's first chunk drops this one
            }
            frame.clear();
            if c.encode(frame) && w.write_all(frame).is_err() {
                return false;
            }
        }
        true
    }

    fn send_copy(&self, w: &TlsWriter, id: u32, paths: &[PathBuf], frame: &mut Vec<u8>) -> bool {
        let Ok((entries, sources)) = collect(paths) else {
            self.not_sent.fetch_add(1, Ordering::Relaxed);
            return true;
        };
        let list = encode_list(&entries);
        if list.len() > CLIP_MAX {
            self.not_sent.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        let total: u64 = entries.iter().map(|e| e.size).sum();
        self.out_done.store(0, Ordering::Relaxed);
        self.out_total.store(total, Ordering::Relaxed);
        let list = ClipItem {
            kind: CLIP_FILES,
            data: list,
        };
        if !self.send_chunks(w, id, &list, frame) {
            return false;
        }
        let start = std::time::Instant::now();
        let mut sent = 0u64;
        let mut buf = vec![0u8; CLIP_CHUNK];
        let cancel = |frame: &mut Vec<u8>| {
            self.out_total.store(0, Ordering::Relaxed);
            frame.clear();
            encode_file_cancel(id, frame);
            w.write_all(frame).is_ok()
        };
        for (index, (e, src)) in entries.iter().zip(&sources).enumerate() {
            if e.dir || e.size == 0 {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(src) else {
                return cancel(frame);
            };
            let mut offset = 0u64;
            while offset < e.size {
                if self.stale(id) {
                    return cancel(frame);
                }
                let want = ((e.size - offset) as usize).min(CLIP_CHUNK);
                if f.read_exact(&mut buf[..want]).is_err() {
                    return cancel(frame); // shorter than listed: it changed
                }
                frame.clear();
                let d = FileData {
                    id,
                    index: index as u32,
                    offset,
                    data: &buf[..want],
                };
                if d.encode(frame) && w.write_all(frame).is_err() {
                    return false;
                }
                offset += want as u64;
                sent += want as u64;
                self.out_done.store(sent, Ordering::Relaxed);
                // Paced: never ahead of FILE_RATE.
                let due = Duration::from_micros(sent * 1_000_000 / FILE_RATE);
                if let Some(wait) = due.checked_sub(start.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
        }
        self.files_sent.fetch_add(1, Ordering::Relaxed);
        true
    }

    // -- receiving (the session's control reader) --

    /// Handles clipboard messages; false for anything else.
    pub fn on_message(&self, m: &Message) -> bool {
        match m {
            Message::Clip(c) => {
                let mut rx = lock(&self.rx);
                if let Some(item) = rx.clip.push(c) {
                    if item.kind == CLIP_FILES {
                        let id = rx.clip.id();
                        self.start_inbound(&mut rx, id, &item.data);
                    } else {
                        *lock(&self.item_in) = Some(item);
                    }
                }
                true
            }
            Message::FileData(d) => {
                let mut rx = lock(&self.rx);
                if !self.file_data(&mut rx, d) {
                    self.drop_inbound(&mut rx);
                }
                true
            }
            Message::FileCancel(id) => {
                let mut rx = lock(&self.rx);
                if rx.inbound.as_ref().is_some_and(|i| i.id == *id) {
                    self.drop_inbound(&mut rx);
                }
                true
            }
            _ => false,
        }
    }

    fn drop_inbound(&self, rx: &mut Rx) {
        if let Some(i) = rx.inbound.take() {
            drop(i.open);
            let _ = std::fs::remove_dir_all(&i.base);
        }
        self.in_total.store(0, Ordering::Relaxed);
    }

    fn start_inbound(&self, rx: &mut Rx, id: u32, list: &[u8]) {
        self.drop_inbound(rx);
        let Some(dir) = lock(&self.file_dir).clone() else {
            return;
        };
        let Some(entries) = decode_list(list) else {
            return;
        };
        let total: u64 = entries.iter().filter(|e| !e.dir).map(|e| e.size).sum();
        if total > FILES_MAX {
            return;
        }
        // Older copies go; the last one stays (its files may be pasted).
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if Some(e.path()) != rx.keep {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let base = dir.join(format!("{}-{id}", crate::clock::now_us()));
        let mut files = Vec::with_capacity(entries.len());
        let mut tops = Vec::new();
        for e in &entries {
            let Some(rel) = safe_path(&e.path) else {
                let _ = std::fs::remove_dir_all(&base);
                return;
            };
            let at = base.join(&rel);
            if rel.components().count() == 1 {
                tops.push(at.clone());
            }
            let made = if e.dir {
                std::fs::create_dir_all(&at)
            } else {
                at.parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|_| std::fs::File::create(&at).map(|_| ()))
            };
            if made.is_err() {
                let _ = std::fs::remove_dir_all(&base);
                return;
            }
            files.push((at, if e.dir { 0 } else { e.size }));
        }
        self.in_done.store(0, Ordering::Relaxed);
        self.in_total.store(total, Ordering::Relaxed);
        rx.inbound = Some(Inbound {
            id,
            base,
            files,
            tops,
            next: 0,
            open: None,
            written: 0,
        });
        self.finish_if_done(rx);
    }

    /// Writes one piece; false if it doesn't fit the transfer (then it's
    /// dropped).
    fn file_data(&self, rx: &mut Rx, d: &FileData) -> bool {
        let Some(i) = rx.inbound.as_mut() else {
            return true; // not receiving (refused, or already dropped)
        };
        if d.id != i.id {
            return true; // a stale transfer's tail
        }
        if d.index as usize != i.next || d.offset != i.written {
            return false;
        }
        let (path, size) = &i.files[i.next];
        if i.written + d.data.len() as u64 > *size {
            return false;
        }
        if i.open.is_none() {
            match std::fs::OpenOptions::new().write(true).open(path) {
                Ok(f) => i.open = Some(f),
                Err(_) => return false,
            }
        }
        if i.open
            .as_mut()
            .is_some_and(|f| f.write_all(d.data).is_err())
        {
            return false;
        }
        i.written += d.data.len() as u64;
        self.in_done
            .fetch_add(d.data.len() as u64, Ordering::Relaxed);
        self.finish_if_done(rx);
        true
    }

    /// Moves past finished files; when all are there, hands them over.
    fn finish_if_done(&self, rx: &mut Rx) {
        let Some(i) = rx.inbound.as_mut() else { return };
        while i.next < i.files.len() && i.written >= i.files[i.next].1 {
            i.open = None;
            i.next += 1;
            i.written = 0;
        }
        if i.next == i.files.len() {
            if let Some(i) = rx.inbound.take() {
                rx.keep = Some(i.base);
                self.files_received.fetch_add(1, Ordering::Relaxed);
                *lock(&self.files_in) = Some(i.tops);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn file_messages_round_trip() {
        let d = FileData {
            id: 5,
            index: 2,
            offset: 1 << 33,
            data: b"abc",
        };
        let mut wire = Vec::new();
        assert!(d.encode(&mut wire));
        encode_file_cancel(5, &mut wire);
        let (m, n) = Message::decode(&wire).unwrap();
        assert_eq!(m, Message::FileData(d));
        let (m, _) = Message::decode(&wire[n..]).unwrap();
        assert_eq!(m, Message::FileCancel(5));
    }

    #[test]
    fn received_paths_are_made_safe() {
        assert_eq!(safe_path("a/b.txt"), Some(PathBuf::from("a").join("b.txt")));
        assert_eq!(safe_path("../etc/passwd"), None);
        assert_eq!(safe_path("a/../../x"), None);
        assert_eq!(safe_path("/abs"), None);
        assert_eq!(safe_path(""), None);
        assert_eq!(safe_path("C:/x"), Some(PathBuf::from("C_").join("x")));
        assert_eq!(safe_path("what?.txt"), Some(PathBuf::from("what_.txt")));
        assert_eq!(safe_path("con.txt"), Some(PathBuf::from("_con.txt")));
        assert_eq!(safe_path("dots..."), Some(PathBuf::from("dots")));
    }

    #[test]
    fn file_lists_round_trip() {
        let e = vec![
            Entry {
                path: "Photos".into(),
                dir: true,
                size: 0,
            },
            Entry {
                path: "Photos/café.jpg".into(),
                dir: false,
                size: 1 << 33,
            },
        ];
        assert_eq!(decode_list(&encode_list(&e)), Some(e));
        assert_eq!(decode_list(&[1, 0, 0, 0, 1]), None); // truncated
    }

    /// A folder and a loose file go across and land as the same tree; a
    /// broken transfer afterwards is dropped and the good copy stays.
    #[test]
    fn files_arrive_whole() {
        let tmp = std::env::temp_dir().join(format!("glass-clip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join("Folder/Inner")).unwrap();
        let big: Vec<u8> = (0..(CLIP_CHUNK * 2 + 77))
            .map(|i| (i % 253) as u8)
            .collect();
        std::fs::write(src.join("Folder/Inner/big.bin"), &big).unwrap();
        std::fs::write(src.join("Folder/empty.txt"), b"").unwrap();
        std::fs::write(src.join("note.txt"), b"hello").unwrap();

        let (entries, sources) = collect(&[src.join("Folder"), src.join("note.txt")]).unwrap();
        assert_eq!(entries.len(), 5);

        // Play the sender by hand: the list, then each file's pieces.
        let rx = ClipLink::default();
        rx.set_file_dir(Some(tmp.join("in")));
        let list = ClipItem {
            kind: CLIP_FILES,
            data: encode_list(&entries),
        };
        for c in chunks(9, &list) {
            assert!(rx.on_message(&Message::Clip(c)));
        }
        assert!(rx.take_files().is_none());
        for (index, (e, s)) in entries.iter().zip(&sources).enumerate() {
            if e.dir {
                continue;
            }
            let bytes = std::fs::read(s).unwrap();
            for (n, piece) in bytes.chunks(CLIP_CHUNK).enumerate() {
                rx.on_message(&Message::FileData(FileData {
                    id: 9,
                    index: index as u32,
                    offset: (n * CLIP_CHUNK) as u64,
                    data: piece,
                }));
            }
        }
        let tops = rx.take_files().expect("complete");
        assert_eq!(tops.len(), 2);
        let folder = tops.iter().find(|p| p.ends_with("Folder")).unwrap();
        assert_eq!(std::fs::read(folder.join("Inner/big.bin")).unwrap(), big);
        assert_eq!(std::fs::read(folder.join("empty.txt")).unwrap(), b"");
        let note = tops.iter().find(|p| p.ends_with("note.txt")).unwrap();
        assert_eq!(std::fs::read(note).unwrap(), b"hello");
        let p = rx.progress();
        assert_eq!((p.in_done, p.files_received), (p.in_total, 1));

        // A piece out of order drops a transfer and its folder.
        let one = vec![Entry {
            path: "x.bin".into(),
            dir: false,
            size: 10,
        }];
        let list = ClipItem {
            kind: CLIP_FILES,
            data: encode_list(&one),
        };
        for c in chunks(10, &list) {
            rx.on_message(&Message::Clip(c));
        }
        rx.on_message(&Message::FileData(FileData {
            id: 10,
            index: 0,
            offset: 4,
            data: b"late",
        }));
        assert!(rx.take_files().is_none());
        // Only the completed copy's folder is left.
        assert_eq!(std::fs::read_dir(tmp.join("in")).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

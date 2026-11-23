//! Clipboard sharing (M5c): text, pictures and files, both directions, over
//! the session's encrypted control connection.
//!
//! Each side watches its own clipboard. When it changes:
//! - **text** goes to the other side at once (it's small) and is put on its
//!   clipboard;
//! - **pictures and files** are only *offered*: a few bytes saying what was
//!   copied (a picture's size; the files' names and sizes). The other side
//!   puts a promise of them on its clipboard (Windows delayed rendering, Mac
//!   lazy pasteboard items). Only when something there is pasted does it
//!   ask (`FileRequest`), and the picture or the files come then, as Remote
//!   Desktop does. Copying something else first sends nothing more.
//!
//! Files land in a Glass folder on the receiving side, and the paste gets
//! them as real files. A control frame holds at most 64 KB, so everything
//! travels as chunks, written a frame at a time by one sender thread per
//! session: pings, pointer shapes and the rest keep flowing in between.
//! File data is paced ([`FILE_RATE`]) so the picture keeps the network.
//! LATER(M5): pace from the rate controller instead. LATER(M5c): Windows
//! virtual files / Mac file promises, so a huge paste streams with the
//! system's own progress instead of the paste waiting.
//!
//! Echo: each app remembers what it last put on its clipboard and doesn't
//! send that back.

use crate::protocol::{
    encode_file_cancel, encode_file_rate, encode_file_request, ClipChunk, FileData, Message,
    CLIP_CHUNK,
};

/// Where a link's control messages go: the session's connection, or (in a
/// helper, ADR 0017) the pipe to the broker. False if it's gone.
pub type FrameWriter = Box<dyn Fn(&[u8]) -> bool + Send + Sync>;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// UTF-8 text.
pub const CLIP_TEXT: u8 = 1;
/// A PNG image (sent when the offer of it is pasted).
pub const CLIP_PNG: u8 = 2;
/// A file list: an offer of files.
pub const CLIP_FILES: u8 = 3;
/// An offer of a picture: its PNG size (`u64`).
pub const CLIP_PNG_OFFER: u8 = 4;
/// The largest item that's sent (16 MB; a picture's PNG).
pub const CLIP_MAX: usize = 16 << 20;
/// At most this many files and folders in one copy.
pub const FILES_MAX_ENTRIES: usize = 10_000;
/// File data's starting speed (bytes/s, about 60 Mbps). The viewer then
/// steers it (`RateControl`): faster while its picture stays perfect, half
/// as fast the moment it doesn't.
pub const FILE_RATE: u64 = 7_500_000;
/// File data never goes slower or faster than this (bytes/s; 8 to 480 Mbps).
pub const FILE_RATE_MIN: u64 = 1_000_000;
pub const FILE_RATE_MAX: u64 = 60_000_000;

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

/// Why a copy of files wasn't offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotSent {
    /// Over [`FILES_MAX_ENTRIES`] files and folders.
    TooMany = 1,
    /// A file or folder couldn't be read (permissions, a cloud-only file).
    Unreadable = 2,
}

/// The file list for a copy of these paths (folders included, with all
/// they contain; links aren't followed), and the source of each entry.
pub fn collect(paths: &[PathBuf]) -> Result<(Vec<Entry>, Vec<PathBuf>), NotSent> {
    fn walk(
        src: &Path,
        rel: String,
        entries: &mut Vec<Entry>,
        sources: &mut Vec<PathBuf>,
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
                walk(&k.path(), format!("{rel}/{name}"), entries, sources)?;
            }
        } else if md.is_file() {
            entries.push(Entry {
                path: rel,
                dir: false,
                size: md.len(),
            });
            sources.push(src.to_path_buf());
        } // links and other special files are skipped
        Ok(())
    }
    let (mut entries, mut sources) = (Vec::new(), Vec::new());
    for p in paths {
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        walk(p, name, &mut entries, &mut sources)?;
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
        // Also turns "." and ".." into "": refused.
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

/// How long a paste waits for `size` bytes: generous, as the data comes at
/// [`FILE_RATE`] with the picture going too.
pub fn fetch_timeout(size: u64) -> Duration {
    Duration::from_secs(30 + size / (FILE_RATE / 2))
}

/// A clipboard message (17–20) as its frame again, for passing it on
/// unchanged. False for any other message.
pub fn encode_message(m: &Message, out: &mut Vec<u8>) -> bool {
    match m {
        Message::Clip(c) => c.encode(out),
        Message::FileData(d) => d.encode(out),
        Message::FileCancel(id) => {
            encode_file_cancel(*id, out);
            true
        }
        Message::FileRequest(id) => {
            encode_file_request(*id, out);
            true
        }
        Message::FileRate(r) => {
            encode_file_rate(*r, out);
            true
        }
        _ => false,
    }
}

/// The viewer's steering of file speed (M5c): every half second, from what
/// it sees of the picture. Lost or late video, or a round trip well above
/// the quiet baseline, halves the speed; otherwise, while a copy is
/// actually moving, it rises by a quarter. With nothing moving it drifts
/// back to [`FILE_RATE`], so a new copy never starts at full blast.
pub struct RateControl {
    rate: f64,
    base_rtt: u32,
    base_since: u64,
    last_bad: u64,
}

impl Default for RateControl {
    fn default() -> Self {
        Self {
            rate: FILE_RATE as f64,
            base_rtt: u32::MAX,
            base_since: 0,
            last_bad: 0,
        }
    }
}

impl RateControl {
    /// `bad`: video frames lost plus late packets so far; `rtt_us`: the
    /// latest round trip; `moving`: a copy is going either way. Returns the
    /// speed to use (bytes/s).
    pub fn tick(&mut self, now_us: u64, bad: u64, rtt_us: u32, moving: bool) -> u64 {
        // The quiet baseline: the lowest round trip of the last 10 s.
        if rtt_us > 0
            && (rtt_us < self.base_rtt || now_us.saturating_sub(self.base_since) > 10_000_000)
        {
            self.base_rtt = rtt_us;
            self.base_since = now_us;
        }
        let worse = bad > self.last_bad
            || (self.base_rtt != u32::MAX && rtt_us > self.base_rtt.saturating_add(10_000));
        self.last_bad = bad;
        if worse {
            self.rate /= 2.0;
        } else if moving {
            self.rate *= 1.25;
        } else {
            self.rate += (FILE_RATE as f64 - self.rate) * 0.2;
        }
        self.rate = self.rate.clamp(FILE_RATE_MIN as f64, FILE_RATE_MAX as f64);
        self.rate as u64
    }
}

// ---- The link: one per session --------------------------------------------

/// Something the other side copied and offers: put a promise of it on the
/// clipboard, and `fetch` it when pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub id: u32,
    /// `CLIP_FILES` or `CLIP_PNG`.
    pub kind: u8,
    /// Bytes in all.
    pub size: u64,
    /// Files: where the top-level files and folders will be once fetched.
    pub files: Vec<PathBuf>,
    /// Files: every file and folder, in order (relative, `/`-separated).
    pub entries: Vec<Entry>,
}

/// What a paste got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    Files(Vec<PathBuf>),
    Png(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchState {
    Pending,
    Done(Fetched),
    Failed,
}

/// Transfer progress and counts, for the apps (bytes; `*_total` 0 = none).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClipProgress {
    pub in_done: u64,
    pub in_total: u64,
    pub out_done: u64,
    pub out_total: u64,
    /// Text sent and received, pictures pasted either way.
    pub items_sent: u32,
    pub items_received: u32,
    /// Copies of files offered, and pasted either way.
    pub files_offered: u32,
    pub files_sent: u32,
    pub files_received: u32,
    /// Copies of files not offered, and why the last one wasn't
    /// (`NotSent`: 1 too many files, 2 unreadable).
    pub not_sent: u32,
    pub not_sent_why: u8,
}

enum Job {
    Text(u32, ClipItem),
    Png(u32, Vec<u8>),
    Files(u32, Vec<PathBuf>),
    Deliver(u32),
}

enum Offered {
    Png(Arc<Vec<u8>>),
    Files {
        entries: Arc<Vec<Entry>>,
        sources: Arc<Vec<PathBuf>>,
    },
}

/// The offer this side made (what a `FileRequest` asks for).
struct OfferOut {
    id: u32,
    what: Offered,
}

/// The offer the other side made, as received.
struct OfferIn {
    offer: Offer,
    entries: Vec<Entry>,
    base: Option<PathBuf>,
    state: Option<FetchState>,
}

/// Files being received.
struct Inbound {
    id: u32,
    base: PathBuf,
    /// Every list entry in order: where it goes and its size (0 for
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
    offer: Option<OfferIn>,
    inbound: Option<Inbound>,
    /// The last fetched folder: kept, its files are on the clipboard.
    keep: Option<PathBuf>,
}

#[derive(Default)]
struct Counts {
    in_done: AtomicU64,
    in_total: AtomicU64,
    out_done: AtomicU64,
    out_total: AtomicU64,
    items_sent: AtomicU32,
    items_received: AtomicU32,
    files_offered: AtomicU32,
    files_sent: AtomicU32,
    files_received: AtomicU32,
    not_sent: AtomicU32,
    not_sent_why: AtomicU32,
}

/// Clipboard state for one session, both ways.
pub struct ClipLink {
    w: Option<FrameWriter>,
    jobs: Mutex<VecDeque<Job>>,
    wake: Condvar,
    /// Bumped on every new copy: text still being sent stops.
    gen: AtomicU32,
    /// How fast file data may go (bytes/s; the viewer steers it).
    rate: AtomicU64,
    sender: Mutex<bool>,
    offer_out: Mutex<Option<OfferOut>>,
    rx: Mutex<Rx>,
    text_in: Mutex<Option<ClipItem>>,
    offer_new: Mutex<Option<Offer>>,
    /// Where received files go; `None`: files aren't accepted.
    file_dir: Mutex<Option<PathBuf>>,
    n: Counts,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ClipLink {
    /// `w`: where its messages go (`None` in tests).
    pub fn new(w: Option<FrameWriter>) -> Self {
        Self {
            w,
            jobs: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            gen: AtomicU32::new(0),
            rate: AtomicU64::new(FILE_RATE),
            sender: Mutex::new(false),
            offer_out: Mutex::new(None),
            rx: Mutex::new(Rx::default()),
            text_in: Mutex::new(None),
            offer_new: Mutex::new(None),
            file_dir: Mutex::new(None),
            n: Counts::default(),
        }
    }

    fn write(&self, frame: &[u8]) -> bool {
        self.w.as_ref().is_none_or(|w| w(frame))
    }

    fn push(self: &Arc<Self>, job: Job) {
        {
            let mut q = lock(&self.jobs);
            if !matches!(job, Job::Deliver(_)) {
                // A newer copy replaces copies not yet handled.
                q.retain(|j| matches!(j, Job::Deliver(_)));
            }
            q.push_back(job);
        }
        self.wake.notify_one();
        let mut started = lock(&self.sender);
        if !*started && self.w.is_some() {
            *started = true;
            let me = self.clone();
            let _ = std::thread::Builder::new()
                .name("glass-clip-send".into())
                .spawn(move || me.send_loop());
        }
    }

    /// This side copied text (sent now) or a picture (`CLIP_PNG`: offered).
    pub fn send_item(self: &Arc<Self>, item: ClipItem) {
        if item.data.is_empty() || item.data.len() > CLIP_MAX {
            return;
        }
        let id = self.gen.fetch_add(1, Ordering::SeqCst) + 1;
        match item.kind {
            CLIP_PNG => self.push(Job::Png(id, item.data)),
            _ => self.push(Job::Text(id, item)),
        }
    }

    /// This side copied files and folders: offered (their list is made on
    /// the sender thread).
    pub fn send_files(self: &Arc<Self>, paths: Vec<PathBuf>) {
        let id = self.gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.push(Job::Files(id, paths));
    }

    /// Text from the other side, once.
    pub fn take_item(&self) -> Option<ClipItem> {
        lock(&self.text_in).take()
    }

    /// A new offer from the other side, once: put a promise of it on the
    /// clipboard.
    pub fn take_offer(&self) -> Option<Offer> {
        lock(&self.offer_new).take()
    }

    /// Something was pasted: fetches offer `id`. Call again (cheap) until it
    /// isn't `Pending`. Not from the session's control reader.
    pub fn fetch(&self, id: u32) -> FetchState {
        let mut rx = lock(&self.rx);
        let (kind, size, entries, base) = match rx.offer.as_mut().filter(|o| o.offer.id == id) {
            None => return FetchState::Failed,
            Some(o) => {
                if let Some(s) = &o.state {
                    return s.clone();
                }
                o.state = Some(FetchState::Pending);
                (
                    o.offer.kind,
                    o.offer.size,
                    o.entries.clone(),
                    o.base.clone(),
                )
            }
        };
        let mut frame = Vec::with_capacity(8);
        encode_file_request(id, &mut frame);
        if !self.write(&frame) {
            Self::fail(&mut rx);
            return FetchState::Failed;
        }
        if kind == CLIP_FILES {
            self.start_inbound(&mut rx, id, &entries, base);
        } else {
            self.n.in_done.store(0, Ordering::Relaxed);
            self.n.in_total.store(size, Ordering::Relaxed);
        }
        rx.offer
            .as_ref()
            .and_then(|o| o.state.clone())
            .unwrap_or(FetchState::Failed)
    }

    /// `fetch`, waiting up to `timeout`.
    pub fn fetch_wait(&self, id: u32, timeout: Duration) -> Option<Fetched> {
        let until = Instant::now() + timeout;
        loop {
            match self.fetch(id) {
                FetchState::Done(f) => return Some(f),
                FetchState::Failed => return None,
                FetchState::Pending if Instant::now() >= until => return None,
                FetchState::Pending => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// How fast this side may send file data (bytes/s).
    pub fn set_rate(&self, bytes_per_s: u64) {
        let r = bytes_per_s.clamp(FILE_RATE_MIN, FILE_RATE_MAX);
        self.rate.store(r, Ordering::Relaxed);
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// A copy is going either way.
    pub fn moving(&self) -> bool {
        let p = self.progress();
        (p.in_total > 0 && p.in_done < p.in_total) || (p.out_total > 0 && p.out_done < p.out_total)
    }

    /// How much of file `index` of offer `id` is here so far, and its size,
    /// while a paste fetches it (a paste streams it as it comes). `None` if
    /// the fetch failed or isn't this offer's.
    pub fn file_progress(&self, id: u32, index: usize) -> Option<(u64, u64)> {
        let rx = lock(&self.rx);
        let o = rx.offer.as_ref().filter(|o| o.offer.id == id)?;
        let e = o.entries.get(index)?;
        let size = if e.dir { 0 } else { e.size };
        match &o.state {
            Some(FetchState::Failed) => None,
            Some(FetchState::Done(_)) => Some((size, size)),
            None => Some((0, size)),
            Some(FetchState::Pending) => match rx.inbound.as_ref().filter(|i| i.id == id) {
                Some(i) if index < i.next => Some((size, size)),
                Some(i) if index == i.next => Some((i.written, size)),
                Some(_) => Some((0, size)),
                None => Some((0, size)),
            },
        }
    }

    /// Accept files into `dir` (`None`: refuse them).
    pub fn set_file_dir(&self, dir: Option<PathBuf>) {
        *lock(&self.file_dir) = dir;
    }

    pub fn progress(&self) -> ClipProgress {
        let n = &self.n;
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let c = |a: &AtomicU32| a.load(Ordering::Relaxed);
        ClipProgress {
            in_done: l(&n.in_done),
            in_total: l(&n.in_total),
            out_done: l(&n.out_done),
            out_total: l(&n.out_total),
            items_sent: c(&n.items_sent),
            items_received: c(&n.items_received),
            files_offered: c(&n.files_offered),
            files_sent: c(&n.files_sent),
            files_received: c(&n.files_received),
            not_sent: c(&n.not_sent),
            not_sent_why: c(&n.not_sent_why) as u8,
        }
    }

    // -- sending (its own thread) --

    fn send_loop(self: Arc<Self>) {
        let mut frame = Vec::with_capacity(CLIP_CHUNK + 32);
        loop {
            let job = {
                let mut q = lock(&self.jobs);
                loop {
                    if let Some(job) = q.pop_front() {
                        break job;
                    }
                    // Nobody else holds the link: the session is over.
                    if Arc::strong_count(&self) == 1 {
                        return;
                    }
                    q = self
                        .wake
                        .wait_timeout(q, Duration::from_millis(500))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            };
            let ok = match job {
                Job::Text(id, item) => {
                    let ok = self.send_chunks(id, &item, &mut frame, true);
                    self.n.items_sent.fetch_add(1, Ordering::Relaxed);
                    ok
                }
                Job::Png(id, data) => {
                    *lock(&self.offer_out) = Some(OfferOut {
                        id,
                        what: Offered::Png(Arc::new(data.clone())),
                    });
                    let offer = ClipItem {
                        kind: CLIP_PNG_OFFER,
                        data: (data.len() as u64).to_be_bytes().to_vec(),
                    };
                    self.send_chunks(id, &offer, &mut frame, false)
                }
                Job::Files(id, paths) => self.offer_files(id, &paths, &mut frame),
                Job::Deliver(id) => self.deliver(id, &mut frame),
            };
            if !ok {
                return; // the connection is gone
            }
        }
    }

    /// Writes an item's chunks; true unless writing failed. `stoppable`:
    /// give up if something newer was copied meanwhile.
    fn send_chunks(&self, id: u32, item: &ClipItem, frame: &mut Vec<u8>, stoppable: bool) -> bool {
        for c in chunks(id, item) {
            if stoppable && self.gen.load(Ordering::SeqCst) != id {
                return true; // the next item's first chunk drops this one
            }
            frame.clear();
            if c.encode(frame) && !self.write(frame) {
                return false;
            }
        }
        true
    }

    fn offer_files(&self, id: u32, paths: &[PathBuf], frame: &mut Vec<u8>) -> bool {
        let (entries, sources) = match collect(paths) {
            Ok(v) => v,
            Err(why) => {
                self.n.not_sent.fetch_add(1, Ordering::Relaxed);
                self.n.not_sent_why.store(why as u32, Ordering::Relaxed);
                return true;
            }
        };
        let list = ClipItem {
            kind: CLIP_FILES,
            data: encode_list(&entries),
        };
        if list.data.len() > CLIP_MAX {
            self.n.not_sent.fetch_add(1, Ordering::Relaxed);
            self.n
                .not_sent_why
                .store(NotSent::TooMany as u32, Ordering::Relaxed);
            return true;
        }
        *lock(&self.offer_out) = Some(OfferOut {
            id,
            what: Offered::Files {
                entries: Arc::new(entries),
                sources: Arc::new(sources),
            },
        });
        self.n.files_offered.fetch_add(1, Ordering::Relaxed);
        self.send_chunks(id, &list, frame, false)
    }

    /// The other side pasted offer `id`: send what it stands for.
    fn deliver(&self, id: u32, frame: &mut Vec<u8>) -> bool {
        let what = lock(&self.offer_out)
            .as_ref()
            .filter(|o| o.id == id)
            .map(|o| match &o.what {
                Offered::Png(d) => Offered::Png(d.clone()),
                Offered::Files { entries, sources } => Offered::Files {
                    entries: entries.clone(),
                    sources: sources.clone(),
                },
            });
        let cancel = |frame: &mut Vec<u8>| {
            self.n.out_total.store(0, Ordering::Relaxed);
            frame.clear();
            encode_file_cancel(id, frame);
            self.write(frame)
        };
        match what {
            None => cancel(frame), // something newer was copied since
            Some(Offered::Png(data)) => {
                let item = ClipItem {
                    kind: CLIP_PNG,
                    data: data.to_vec(),
                };
                self.n.items_sent.fetch_add(1, Ordering::Relaxed);
                self.send_chunks(id, &item, frame, false)
            }
            Some(Offered::Files { entries, sources }) => {
                let total: u64 = entries.iter().map(|e| e.size).sum();
                self.n.out_done.store(0, Ordering::Relaxed);
                self.n.out_total.store(total, Ordering::Relaxed);
                // Paced: each piece is due `len / rate` after the last.
                let mut due = Instant::now();
                let mut sent = 0u64;
                let mut buf = vec![0u8; CLIP_CHUNK];
                for (index, (e, src)) in entries.iter().zip(sources.iter()).enumerate() {
                    if e.dir || e.size == 0 {
                        continue;
                    }
                    let Ok(mut f) = std::fs::File::open(src) else {
                        return cancel(frame);
                    };
                    let mut offset = 0u64;
                    while offset < e.size {
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
                        if d.encode(frame) && !self.write(frame) {
                            return false;
                        }
                        offset += want as u64;
                        sent += want as u64;
                        self.n.out_done.store(sent, Ordering::Relaxed);
                        let rate = self.rate.load(Ordering::Relaxed).max(FILE_RATE_MIN);
                        due += Duration::from_micros(want as u64 * 1_000_000 / rate);
                        let now = Instant::now();
                        if due > now {
                            std::thread::sleep(due - now);
                        } else if now - due > Duration::from_millis(20) {
                            due = now; // fell behind: no burst to catch up
                        }
                    }
                }
                self.n.files_sent.fetch_add(1, Ordering::Relaxed);
                true
            }
        }
    }

    // -- receiving (the session's control reader) --

    /// Handles clipboard messages; false for anything else.
    pub fn on_message(self: &Arc<Self>, m: &Message) -> bool {
        match m {
            Message::Clip(c) => {
                let mut rx = lock(&self.rx);
                if let Some(item) = rx.clip.push(c) {
                    let id = rx.clip.id();
                    self.on_item(&mut rx, id, item);
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
                if let Some(o) = rx.offer.as_mut().filter(|o| o.offer.id == *id) {
                    o.state = Some(FetchState::Failed);
                }
                true
            }
            Message::FileRate(r) => {
                self.set_rate(*r as u64);
                true
            }
            Message::FileRequest(id) => {
                let ours = lock(&self.offer_out).as_ref().is_some_and(|o| o.id == *id);
                if ours {
                    self.push(Job::Deliver(*id));
                } else {
                    let mut frame = Vec::with_capacity(8);
                    encode_file_cancel(*id, &mut frame);
                    self.write(&frame);
                }
                true
            }
            _ => false,
        }
    }

    fn on_item(&self, rx: &mut Rx, id: u32, item: ClipItem) {
        match item.kind {
            CLIP_FILES => {
                let Some(dir) = lock(&self.file_dir).clone() else {
                    return; // files refused here
                };
                let Some(entries) = decode_list(&item.data) else {
                    return;
                };
                let mut files = Vec::new();
                let base = dir.join(format!("{}-{id}", crate::clock::now_us()));
                for e in &entries {
                    let Some(rel) = safe_path(&e.path) else {
                        return;
                    };
                    if rel.components().count() == 1 {
                        files.push(base.join(rel));
                    }
                }
                let size = entries.iter().filter(|e| !e.dir).map(|e| e.size).sum();
                self.new_offer(
                    rx,
                    Offer {
                        id,
                        kind: CLIP_FILES,
                        size,
                        files,
                        entries: entries.clone(),
                    },
                    entries,
                    Some(base),
                );
            }
            CLIP_PNG_OFFER => {
                let Some(size) = item.data.get(..8).and_then(|b| b.try_into().ok()) else {
                    return;
                };
                let offer = Offer {
                    id,
                    kind: CLIP_PNG,
                    size: u64::from_be_bytes(size),
                    files: Vec::new(),
                    entries: Vec::new(),
                };
                self.new_offer(rx, offer, Vec::new(), None);
            }
            CLIP_PNG => {
                // The picture a paste here asked for.
                if let Some(o) = rx.offer.as_mut().filter(|o| {
                    o.offer.id == id
                        && o.offer.kind == CLIP_PNG
                        && o.state == Some(FetchState::Pending)
                }) {
                    o.state = Some(FetchState::Done(Fetched::Png(item.data)));
                    self.n.in_done.store(o.offer.size, Ordering::Relaxed);
                    self.n.items_received.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {
                self.n.items_received.fetch_add(1, Ordering::Relaxed);
                *lock(&self.text_in) = Some(item);
            }
        }
    }

    fn new_offer(&self, rx: &mut Rx, offer: Offer, entries: Vec<Entry>, base: Option<PathBuf>) {
        self.drop_inbound(rx);
        rx.offer = Some(OfferIn {
            offer: offer.clone(),
            entries,
            base,
            state: None,
        });
        *lock(&self.offer_new) = Some(offer);
    }

    fn drop_inbound(&self, rx: &mut Rx) {
        if let Some(i) = rx.inbound.take() {
            drop(i.open);
            let _ = std::fs::remove_dir_all(&i.base);
            if let Some(o) = rx.offer.as_mut().filter(|o| o.offer.id == i.id) {
                o.state = Some(FetchState::Failed);
            }
        }
        self.n.in_total.store(0, Ordering::Relaxed);
    }

    fn fail(rx: &mut Rx) {
        if let Some(o) = rx.offer.as_mut() {
            o.state = Some(FetchState::Failed);
        }
    }

    fn start_inbound(&self, rx: &mut Rx, id: u32, entries: &[Entry], base: Option<PathBuf>) {
        let Some(base) = base else {
            return Self::fail(rx);
        };
        // Older fetched copies go; the last stays (it may be pasted again).
        if let Some(dir) = base.parent() {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    if Some(e.path()) != rx.keep {
                        let _ = std::fs::remove_dir_all(e.path());
                    }
                }
            }
        }
        let mut files = Vec::with_capacity(entries.len());
        let mut tops = Vec::new();
        for e in entries {
            let Some(rel) = safe_path(&e.path) else {
                return Self::fail(rx);
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
                return Self::fail(rx);
            }
            files.push((at, if e.dir { 0 } else { e.size }));
        }
        let total = files.iter().map(|f| f.1).sum();
        self.n.in_done.store(0, Ordering::Relaxed);
        self.n.in_total.store(total, Ordering::Relaxed);
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
            return true; // not receiving
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
        self.n
            .in_done
            .fetch_add(d.data.len() as u64, Ordering::Relaxed);
        self.finish_if_done(rx);
        true
    }

    /// Moves past finished files; when all are there, the fetch is done.
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
                self.n.files_received.fetch_add(1, Ordering::Relaxed);
                if let Some(o) = rx.offer.as_mut().filter(|o| o.offer.id == i.id) {
                    o.state = Some(FetchState::Done(Fetched::Files(i.tops)));
                }
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
        encode_file_request(6, &mut wire);
        let (m, n) = Message::decode(&wire).unwrap();
        assert_eq!(m, Message::FileData(d));
        let (m, n2) = Message::decode(&wire[n..]).unwrap();
        assert_eq!(m, Message::FileCancel(5));
        let (m, _) = Message::decode(&wire[n + n2..]).unwrap();
        assert_eq!(m, Message::FileRequest(6));
    }

    #[test]
    fn file_speed_follows_the_picture() {
        let mut r = RateControl::default();
        let mut t = 0;
        let mut tick = |r: &mut RateControl, bad, rtt, moving| {
            t += 500_000;
            r.tick(t, bad, rtt, moving)
        };
        // A quiet network and a copy going: it speeds up to the cap.
        let mut v = 0;
        for _ in 0..40 {
            v = tick(&mut r, 0, 3_000, true);
        }
        assert_eq!(v, FILE_RATE_MAX);
        // The round trip jumps: halved at once.
        assert_eq!(tick(&mut r, 0, 20_000, true), FILE_RATE_MAX / 2);
        // A lost frame: halved again.
        assert_eq!(tick(&mut r, 1, 3_000, true), FILE_RATE_MAX / 4);
        // Nothing moving: back towards the starting speed, not up.
        for _ in 0..40 {
            v = tick(&mut r, 1, 3_000, false);
        }
        assert!((v as i64 - FILE_RATE as i64).abs() < 100_000, "{v}");
        // Never below the floor.
        for i in 0..40 {
            v = tick(&mut r, 2 + i, 3_000, true);
        }
        assert_eq!(v, FILE_RATE_MIN);
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

    fn feed(rx: &Arc<ClipLink>, id: u32, item: &ClipItem) {
        for c in chunks(id, item) {
            assert!(rx.on_message(&Message::Clip(c)));
        }
    }

    /// Files are only offered; a paste fetches them, and they land as the
    /// same tree. A broken transfer afterwards fails and leaves nothing.
    #[test]
    fn files_are_fetched_on_paste_and_arrive_whole() {
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

        let rx = Arc::new(ClipLink::new(None));
        rx.set_file_dir(Some(tmp.join("in")));
        let list = ClipItem {
            kind: CLIP_FILES,
            data: encode_list(&entries),
        };
        feed(&rx, 9, &list);
        // An offer: nothing written yet.
        let offer = rx.take_offer().expect("offer");
        assert_eq!(
            (offer.id, offer.kind, offer.files.len()),
            (9, CLIP_FILES, 2)
        );
        assert_eq!(offer.size, big.len() as u64 + 5);
        assert!(!tmp.join("in").exists());
        // Pasted: the request goes, then the sender's pieces arrive.
        assert_eq!(rx.fetch(9), FetchState::Pending);
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
        assert_eq!(
            rx.file_progress(9, 2),
            Some((big.len() as u64, big.len() as u64))
        );
        let FetchState::Done(Fetched::Files(tops)) = rx.fetch(9) else {
            panic!("not fetched")
        };
        assert_eq!(tops, offer.files);
        let folder = tops.iter().find(|p| p.ends_with("Folder")).unwrap();
        assert_eq!(std::fs::read(folder.join("Inner/big.bin")).unwrap(), big);
        assert_eq!(std::fs::read(folder.join("empty.txt")).unwrap(), b"");
        let note = tops.iter().find(|p| p.ends_with("note.txt")).unwrap();
        assert_eq!(std::fs::read(note).unwrap(), b"hello");
        let p = rx.progress();
        assert_eq!((p.in_done, p.files_received), (p.in_total, 1));

        // A piece out of order fails the next fetch and removes its folder.
        let one = vec![Entry {
            path: "x.bin".into(),
            dir: false,
            size: 10,
        }];
        let list = ClipItem {
            kind: CLIP_FILES,
            data: encode_list(&one),
        };
        feed(&rx, 10, &list);
        assert_eq!(rx.fetch(10), FetchState::Pending);
        rx.on_message(&Message::FileData(FileData {
            id: 10,
            index: 0,
            offset: 4,
            data: b"late",
        }));
        assert_eq!(rx.fetch(10), FetchState::Failed);
        // Only the completed copy's folder is left.
        assert_eq!(std::fs::read_dir(tmp.join("in")).unwrap().count(), 1);
        // An old offer can't be fetched any more.
        assert_eq!(rx.fetch(9), FetchState::Failed);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pictures_are_fetched_on_paste() {
        let rx = Arc::new(ClipLink::new(None));
        let png = vec![7u8; CLIP_CHUNK + 5];
        let offer = ClipItem {
            kind: CLIP_PNG_OFFER,
            data: (png.len() as u64).to_be_bytes().to_vec(),
        };
        feed(&rx, 3, &offer);
        let o = rx.take_offer().unwrap();
        assert_eq!((o.kind, o.size), (CLIP_PNG, png.len() as u64));
        // Not asked for yet: a picture arriving now isn't taken as the paste.
        assert_eq!(rx.fetch(3), FetchState::Pending);
        feed(
            &rx,
            3,
            &ClipItem {
                kind: CLIP_PNG,
                data: png.clone(),
            },
        );
        assert_eq!(rx.fetch(3), FetchState::Done(Fetched::Png(png)));
        // Text still comes straight away.
        feed(
            &rx,
            4,
            &ClipItem {
                kind: CLIP_TEXT,
                data: b"hi".to_vec(),
            },
        );
        assert_eq!(rx.take_item().unwrap().data, b"hi");
    }
}

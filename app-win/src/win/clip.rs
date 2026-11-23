//! Clipboard sharing for a session (M5c): text, pictures and files, both
//! ways, through the session's clipboard link (core `clipboard.rs`).
//!
//! A thread of its own, with a hidden window (the clipboard talks to its
//! owner through window messages), watches this PC's clipboard every 100 ms:
//! - text copied here goes to the other side at once; text from there goes
//!   straight on the clipboard;
//! - a picture or files copied here are only offered. An offer from the
//!   other side goes on the clipboard as a *promise*, fetched only when an
//!   app pastes, as Remote Desktop does: a picture by delayed rendering
//!   (Windows asks this window for it, `WM_RENDERFORMAT`), files as virtual
//!   files (`vfiles.rs`).
//!
//! None of this runs on the capture, encode or viewer threads.
//!
//! Text travels as UTF-8 with `\n` line endings; pictures as PNG (the
//! registered "PNG" format if the copying app offered it, else the DIB
//! converted), pasted as both "PNG" and a DIB so every app can take it.
//! Files copied in Explorer (CF_HDROP) are offered by path; pasted files
//! land in Glass's folder first.

use glass_core::clipboard::{
    fetch_timeout, ClipItem, ClipLink, Fetched, Offer, CLIP_FILES, CLIP_MAX, CLIP_PNG, CLIP_TEXT,
};
use std::cell::RefCell;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassW, SetTimer, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROY,
    WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS, WM_RENDERFORMAT, WM_TIMER, WNDCLASSW,
};

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
/// `DROPFILES`: offset of the names, a point, two flags.
#[cfg(test)]
const DROPFILES_SIZE: usize = 20;
/// The largest image converted from a DIB (pixels).
const MAX_PIXELS: usize = 8192 * 8192;

/// The clipboard thread for one session; it stops when this is dropped.
pub struct ClipSync(Arc<AtomicBool>);

impl ClipSync {
    /// `send_current`: also send what's on the clipboard now (the viewer
    /// does, so something copied before connecting can be pasted there).
    pub fn start(link: Arc<ClipLink>, send_current: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        CHANGES.store(0, Ordering::Relaxed);
        READ_FAILS.store(0, Ordering::Relaxed);
        LAST_FAIL.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let _ = std::thread::Builder::new()
            .name("glass-clipboard".into())
            .spawn(move || run(link, send_current, s));
        Self(stop)
    }
}

impl Drop for ClipSync {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Where this PC keeps files that arrive over the clipboard. The service's
/// agent (SYSTEM) uses a folder the signed-in user can read.
pub fn file_dir(agent: bool, role: &str) -> PathBuf {
    if agent {
        let pd = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
        PathBuf::from(pd).join("Glass-clipboard")
    } else {
        std::env::temp_dir()
            .join("Glass")
            .join(format!("clipboard-{role}"))
    }
}

/// Files that came over a Glass clipboard (on either side of a same-PC
/// test): never offered back.
fn from_glass(paths: &[PathBuf]) -> bool {
    let roots = [file_dir(true, ""), std::env::temp_dir().join("Glass")];
    paths.iter().all(|p| roots.iter().any(|r| p.starts_with(r)))
}

/// The results line: what crossed, and why a copy of files wasn't offered.
pub fn summary(p: &glass_core::clipboard::ClipProgress, other: &str) -> String {
    let why = match p.not_sent_why {
        1 => " (last: over 10 000 files)",
        2 => " (last: a file couldn't be read, e.g. permissions or a cloud-only file)",
        _ => "",
    };
    let fails = READ_FAILS.load(Ordering::Relaxed);
    let fails = if fails == 0 {
        String::new()
    } else {
        format!(
            " | couldn't read this PC's clipboard {fails} times (last: {})",
            LAST_FAIL.lock().unwrap_or_else(|e| e.into_inner())
        )
    };
    format!(
        "text and pictures: {} sent to the {other}, {} received | files: {} copies offered, {} pasted on the {other}, {} pasted here | not offered: {}{why} | changes seen here: {}{fails}",
        p.items_sent,
        p.items_received,
        p.files_offered,
        p.files_sent,
        p.files_received,
        p.not_sent,
        CHANGES.load(Ordering::Relaxed)
    )
}

fn hash(item: &ClipItem) -> u64 {
    let mut h = DefaultHasher::new();
    item.hash(&mut h);
    h.finish()
}

#[derive(Clone, Copy)]
struct Formats {
    png: u32,
    drop_effect: u32,
    /// Marks a promise as Glass's: never read back (reading it would fetch).
    glass: u32,
    /// Asks clipboard history and other watchers to leave it alone (they'd
    /// fetch it too).
    no_monitor: u32,
}

/// What this PC's clipboard holds that's ours to read.
enum Copied {
    Item(ClipItem),
    Files(Vec<PathBuf>),
    /// Files are there but the plain read failed: ask through OLE.
    FilesViaOle,
}

/// For the results: clipboard changes noticed, and reads that failed (a
/// copy that couldn't be read was otherwise silent: m5c-test4, files copied
/// on a PC with the service never crossed).
static CHANGES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static READ_FAILS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static LAST_FAIL: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn note_fail(what: String) {
    READ_FAILS.fetch_add(1, Ordering::Relaxed);
    *LAST_FAIL.lock().unwrap_or_else(|e| e.into_inner()) = what;
}

struct State {
    link: Arc<ClipLink>,
    stop: Arc<AtomicBool>,
    fm: Formats,
    /// The clipboard's sequence number when last looked at.
    seen: u32,
    /// The last thing sent or put on the clipboard: an app that sets the
    /// same contents again isn't news, and what arrived isn't sent back.
    last: u64,
    /// The other side's offer of a picture this PC's clipboard promises,
    /// and what a paste fetched for it.
    offer: Option<Offer>,
    got: Option<Fetched>,
    /// The virtual files on the clipboard (kept alive here too).
    vfiles: Option<windows::Win32::System::Com::IDataObject>,
    vfm: super::vfiles::Formats,
}

thread_local! {
    static ST: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn seq() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

fn run(link: Arc<ClipLink>, send_current: bool, stop: Arc<AtomicBool>) {
    unsafe {
        // Virtual files are a COM object: this thread is its apartment.
        let _ = windows::Win32::System::Ole::OleInitialize(None);
        let inst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("GlassClipboard");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc); // fails harmlessly if already registered
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Glass clipboard"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(inst.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                note_fail(format!(
                    "the clipboard thread couldn't start: {}",
                    e.message()
                ));
                return;
            }
        };
        let fm = Formats {
            png: RegisterClipboardFormatW(w!("PNG")),
            drop_effect: RegisterClipboardFormatW(w!("Preferred DropEffect")),
            glass: RegisterClipboardFormatW(w!("Glass promise")),
            no_monitor: RegisterClipboardFormatW(w!(
                "ExcludeClipboardContentFromMonitorProcessing"
            )),
        };
        let vfm = super::vfiles::Formats::register(fm.glass, fm.no_monitor, fm.drop_effect);
        ST.with(|s| {
            *s.borrow_mut() = Some(State {
                link,
                stop,
                fm,
                vfiles: None,
                vfm,
                seen: if send_current { 0 } else { seq() },
                last: 0,
                offer: None,
                got: None,
            })
        });
        SetTimer(Some(hwnd), 1, 100, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
        // Files still promised can't be pasted once this thread is gone.
        if let Some(st) = ST.with(|s| s.borrow_mut().take()) {
            if let Some(v) = &st.vfiles {
                if windows::Win32::System::Ole::OleIsCurrentClipboard(v).is_ok() {
                    let _ = windows::Win32::System::Ole::OleSetClipboard(None);
                }
            }
        }
        windows::Win32::System::Ole::OleUninitialize();
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            tick(hwnd);
            LRESULT(0)
        }
        WM_RENDERFORMAT => {
            render(wparam.0 as u32);
            LRESULT(0)
        }
        // Glass is going: what wasn't pasted goes with it (fetching it all
        // now would send everything for nothing).
        WM_RENDERALLFORMATS => LRESULT(0),
        WM_DESTROYCLIPBOARD => {
            ST.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.offer = None;
                    st.got = None;
                }
            });
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Every 100 ms: what arrived goes on the clipboard, what was copied here
/// goes (or is offered) to the other side. The state isn't borrowed while
/// the clipboard is used: Windows may call back into this window.
fn tick(hwnd: HWND) {
    let Some((link, stop, fm, vfm, mut seen, mut last)) = ST.with(|s| {
        s.borrow().as_ref().map(|st| {
            (
                st.link.clone(),
                st.stop.clone(),
                st.fm,
                st.vfm,
                st.seen,
                st.last,
            )
        })
    }) else {
        return;
    };
    if stop.load(Ordering::Relaxed) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return;
    }
    let mut promised = None;
    let mut vfiles = None;
    if let Some(item) = link.take_item() {
        if set_text(hwnd, &item) {
            last = hash(&item);
            seen = seq();
        }
    }
    if let Some(offer) = link.take_offer() {
        if offer.kind == CLIP_FILES {
            let obj = super::vfiles::VirtualFiles::object(link.clone(), offer, vfm);
            if unsafe { windows::Win32::System::Ole::OleSetClipboard(&obj) }.is_ok() {
                seen = seq();
                vfiles = Some(obj);
            }
        } else if promise(hwnd, &offer, &fm) {
            seen = seq();
            promised = Some(offer);
        }
    }
    let now = seq();
    if now != seen {
        CHANGES.fetch_add(1, Ordering::Relaxed);
        match read(&fm) {
            Some(copied) => {
                seen = now;
                let item = match &copied {
                    Copied::Item(i) => i.clone(),
                    Copied::FilesViaOle => unreachable!("read() resolves it"),
                    Copied::Files(p) => ClipItem {
                        kind: CLIP_FILES,
                        data: p
                            .iter()
                            .flat_map(|p| p.to_string_lossy().into_owned().into_bytes())
                            .collect(),
                    },
                };
                let h = hash(&item);
                if h != last {
                    last = h;
                    match copied {
                        Copied::Files(p) if !from_glass(&p) => link.send_files(p),
                        Copied::Files(_) | Copied::FilesViaOle => {}
                        Copied::Item(i) => link.send_item(i),
                    }
                }
            }
            None if !busy() => seen = now, // nothing we share, or empty
            None => {}                     // another app has it open: next time
        }
    }
    ST.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.seen = seen;
            st.last = last;
            if let Some(o) = promised {
                st.offer = Some(o);
                st.got = None;
            }
            if vfiles.is_some() {
                st.vfiles = vfiles;
            }
        }
    });
}

/// An app is pasting a promised format: fetch the offer (once) and hand it
/// over. This thread waits meanwhile; the viewer shows the progress.
fn render(format: u32) {
    if std::env::var_os("GLASS_DEBUG_CLIP").is_some() {
        use windows::Win32::System::DataExchange::GetOpenClipboardWindow;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        let w = unsafe { GetOpenClipboardWindow() }.unwrap_or_default();
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(w, Some(&mut pid)) };
        eprintln!(
            "clipboard: format {format} asked for by pid {pid} (window {:?})",
            w.0
        );
    }
    let Some((link, fm, offer, got)) = ST.with(|s| {
        s.borrow()
            .as_ref()
            .map(|st| (st.link.clone(), st.fm, st.offer.clone(), st.got.clone()))
    }) else {
        return;
    };
    let Some(offer) = offer else { return };
    let got = match got {
        Some(g) => g,
        None => {
            let Some(g) = link.fetch_wait(offer.id, fetch_timeout(offer.size)) else {
                return;
            };
            ST.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.got = Some(g.clone());
                }
            });
            g
        }
    };
    let data = match (&got, format) {
        (Fetched::Png(d), f) if f == fm.png => Some(d.clone()),
        (Fetched::Png(d), CF_DIB) => png_to_dib(d),
        _ => None,
    };
    if let Some(g) = data.and_then(|d| unsafe { global(&d) }) {
        unsafe {
            if SetClipboardData(format, Some(HANDLE(g.0))).is_err() {
                let _ = GlobalFree(Some(g));
            }
        }
    }
    // Handing data over counts as a change: not news.
    let now = seq();
    ST.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.seen = now;
        }
    });
}

/// Opens the clipboard, retrying briefly (other apps hold it for a moment).
fn open(owner: Option<HWND>) -> bool {
    for _ in 0..5 {
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn busy() -> bool {
    if open(None) {
        unsafe {
            let _ = CloseClipboard();
        }
        false
    } else {
        true
    }
}

/// Copies a clipboard handle's bytes.
unsafe fn bytes(h: HANDLE) -> Option<Vec<u8>> {
    let g = HGLOBAL(h.0);
    let n = GlobalSize(g);
    let p = GlobalLock(g) as *const u8;
    if p.is_null() || n == 0 || n > CLIP_MAX + 4096 {
        let _ = GlobalUnlock(g);
        return None;
    }
    let v = std::slice::from_raw_parts(p, n).to_vec();
    let _ = GlobalUnlock(g);
    Some(v)
}

/// The file names in a `DROPFILES` block (wide names only: Explorer's).
fn drop_files(b: &[u8]) -> Option<Vec<PathBuf>> {
    let at = u32::from_le_bytes(b.get(0..4)?.try_into().ok()?) as usize;
    let wide = u32::from_le_bytes(b.get(16..20)?.try_into().ok()?) != 0;
    if !wide || at > b.len() {
        return None;
    }
    let u: Vec<u16> = b[at..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let paths: Vec<PathBuf> = u
        .split(|&c| c == 0)
        .take_while(|s| !s.is_empty())
        .map(|s| PathBuf::from(String::from_utf16_lossy(s)))
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// A `DROPFILES` block for these paths (the test's other half).
#[cfg(test)]
fn dropfiles_for(paths: &[PathBuf]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(DROPFILES_SIZE as u32).to_le_bytes());
    b.extend_from_slice(&[0; 12]); // point, non-client flag
    b.extend_from_slice(&1u32.to_le_bytes()); // wide names
    for p in paths {
        let s = p.to_string_lossy();
        b.extend(s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
    }
    b.extend_from_slice(&[0, 0]);
    b
}

/// What's on the clipboard now, if it's something we share: files first
/// (Explorer), then text (a spreadsheet's cells also come as a picture),
/// then a picture.
fn read(fm: &Formats) -> Option<Copied> {
    let png_format = fm.png;
    if !open(None) {
        return None;
    }
    let got = unsafe {
        let has = |f: u32| f != 0 && IsClipboardFormatAvailable(f).is_ok();
        if has(fm.glass) {
            None // a Glass promise (this one's, or another Glass's on this PC)
        } else if has(CF_HDROP) {
            match GetClipboardData(CF_HDROP) {
                Ok(h) => match bytes(h).and_then(|b| drop_files(&b)) {
                    Some(p) => Some(Copied::Files(p)),
                    None => {
                        note_fail("a file list that was empty or unreadable".into());
                        Some(Copied::FilesViaOle)
                    }
                },
                Err(e) => {
                    note_fail(format!("the file list: {}", e.message()));
                    Some(Copied::FilesViaOle)
                }
            }
        } else if has(CF_UNICODETEXT) {
            GetClipboardData(CF_UNICODETEXT)
                .ok()
                .and_then(|h| bytes(h))
                .and_then(|b| {
                    let u: Vec<u16> = b
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .take_while(|&c| c != 0)
                        .collect();
                    let s = String::from_utf16_lossy(&u).replace("\r\n", "\n");
                    (!s.is_empty()).then(|| {
                        Copied::Item(ClipItem {
                            kind: CLIP_TEXT,
                            data: s.into_bytes(),
                        })
                    })
                })
        } else if png_format != 0 && has(png_format) {
            GetClipboardData(png_format)
                .ok()
                .and_then(|h| bytes(h))
                .map(|data| {
                    Copied::Item(ClipItem {
                        kind: CLIP_PNG,
                        data,
                    })
                })
        } else if has(CF_DIB) {
            // Encoded after the clipboard is closed.
            GetClipboardData(CF_DIB)
                .ok()
                .and_then(|h| bytes(h))
                .map(|data| Copied::Item(ClipItem { kind: 0, data }))
        } else {
            None
        }
    };
    unsafe {
        let _ = CloseClipboard();
    }
    match got {
        // The copying app (Explorer) renders the list for OLE callers too.
        Some(Copied::FilesViaOle) => match ole_files() {
            Ok(p) => Some(Copied::Files(p)),
            Err(e) => {
                note_fail(format!("the file list, also through OLE: {e}"));
                None
            }
        },
        Some(Copied::Item(ClipItem { kind: 0, data })) => dib_to_png(&data).map(|data| {
            Copied::Item(ClipItem {
                kind: CLIP_PNG,
                data,
            })
        }),
        Some(Copied::Item(i)) if i.data.len() > CLIP_MAX => None,
        other => other,
    }
}

/// The copied files' list asked for through OLE (the copying app's own data
/// object), when the plain clipboard read failed.
fn ole_files() -> Result<Vec<PathBuf>, String> {
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Ole::{OleGetClipboard, ReleaseStgMedium};
    unsafe {
        let d = OleGetClipboard().map_err(|e| e.message())?;
        let f = FORMATETC {
            cfFormat: CF_HDROP as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let mut m = d.GetData(&f).map_err(|e| e.message())?;
        let got = if m.tymed == TYMED_HGLOBAL.0 as u32 {
            bytes(HANDLE(m.u.hGlobal.0)).and_then(|b| drop_files(&b))
        } else {
            None
        };
        ReleaseStgMedium(&mut m);
        got.ok_or_else(|| "no file list in it".to_string())
    }
}

/// Moves `data` into a new global block for `SetClipboardData`.
unsafe fn global(data: &[u8]) -> Option<HGLOBAL> {
    let g = GlobalAlloc(GMEM_MOVEABLE, data.len()).ok()?;
    let p = GlobalLock(g) as *mut u8;
    if p.is_null() {
        let _ = GlobalFree(Some(g));
        return None;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    let _ = GlobalUnlock(g);
    Some(g)
}

/// Puts text from the other side on the clipboard.
fn set_text(hwnd: HWND, item: &ClipItem) -> bool {
    if item.kind != CLIP_TEXT {
        return false;
    }
    let s = String::from_utf8_lossy(&item.data)
        .replace("\r\n", "\n")
        .replace('\n', "\r\n");
    let b: Vec<u8> = s
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    if !open(Some(hwnd)) {
        return false;
    }
    let mut ok = false;
    unsafe {
        if EmptyClipboard().is_ok() {
            if let Some(g) = global(&b) {
                if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(g.0))).is_ok() {
                    ok = true; // the clipboard owns the block now
                } else {
                    let _ = GlobalFree(Some(g));
                }
            }
        }
        let _ = CloseClipboard();
    }
    ok
}

/// Puts a promise of the other side's offer on the clipboard: its formats,
/// with the data to come when pasted.
fn promise(hwnd: HWND, offer: &Offer, fm: &Formats) -> bool {
    let formats: Vec<u32> = match offer.kind {
        CLIP_PNG if fm.png != 0 => vec![fm.png, CF_DIB],
        CLIP_PNG => vec![CF_DIB],
        _ => return false,
    };
    if !open(Some(hwnd)) {
        return false;
    }
    let mut ok = false;
    unsafe {
        if EmptyClipboard().is_ok() {
            ok = true;
            for f in formats {
                // No data yet: Windows asks (WM_RENDERFORMAT) when pasted.
                // (A null handle reads as an error here, but isn't.)
                let _ = SetClipboardData(f, None);
            }
            for f in [fm.glass, fm.no_monitor] {
                if let Some(g) = (f != 0).then(|| global(&[0; 4])).flatten() {
                    if SetClipboardData(f, Some(HANDLE(g.0))).is_err() {
                        let _ = GlobalFree(Some(g));
                    }
                }
            }
        }
        let _ = CloseClipboard();
    }
    ok
}

/// A 24- or 32-bit DIB (BITMAPINFOHEADER and pixels) as a PNG.
fn dib_to_png(d: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |i: usize| u32::from_le_bytes(d[i..i + 4].try_into().unwrap());
    if d.len() < 40 {
        return None;
    }
    let header = u32_at(0) as usize;
    let w = u32_at(4) as i32;
    let h = u32_at(8) as i32;
    let bpp = u16::from_le_bytes([d[14], d[15]]) as usize;
    let compression = u32_at(16);
    let palette = u32_at(32) as usize;
    if w <= 0 || h == 0 || !(bpp == 24 || bpp == 32) || !(compression == 0 || compression == 3) {
        return None;
    }
    let (w, rows, top_down) = (w as usize, h.unsigned_abs() as usize, h < 0);
    if w * rows > MAX_PIXELS {
        return None;
    }
    // BI_BITFIELDS with a plain header: three masks follow it (assumed the
    // usual BGRA order, which is what Windows produces).
    let masks = if compression == 3 && header == 40 {
        12
    } else {
        0
    };
    let start = header + masks + palette * 4;
    let px = bpp / 8;
    let stride = (w * px + 3) & !3;
    if d.len() < start + stride * rows {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * rows * 4);
    for y in 0..rows {
        let row = if top_down { y } else { rows - 1 - y };
        let line = &d[start + row * stride..][..w * px];
        for p in line.chunks_exact(px) {
            rgba.extend_from_slice(&[p[2], p[1], p[0], if px == 4 { p[3] } else { 255 }]);
        }
    }
    // Most 32-bit DIBs leave alpha at 0 meaning "opaque".
    if px == 4 && rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        rgba.as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|p| p[3] = 255);
    }
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, w as u32, rows as u32);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.set_compression(png::Compression::Fast);
        let mut wr = e.write_header().ok()?;
        wr.write_image_data(&rgba).ok()?;
    }
    (out.len() <= CLIP_MAX).then_some(out)
}

/// A PNG as a 32-bit bottom-up DIB (what most apps paste).
fn png_to_dib(data: &[u8]) -> Option<Vec<u8>> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = dec.read_info().ok()?;
    let (w, h) = {
        let i = r.info();
        (i.width as usize, i.height as usize)
    };
    if w == 0 || h == 0 || w * h > MAX_PIXELS {
        return None;
    }
    let mut buf = vec![0; r.output_buffer_size()?];
    let info = r.next_frame(&mut buf).ok()?;
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return None, // expanded by the transformations
    };
    let mut dib = Vec::with_capacity(40 + w * h * 4);
    let le32 = |v: u32| v.to_le_bytes();
    dib.extend_from_slice(&le32(40));
    dib.extend_from_slice(&le32(w as u32));
    dib.extend_from_slice(&le32(h as u32)); // positive: bottom-up
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&le32(0)); // BI_RGB
    dib.extend_from_slice(&le32((w * h * 4) as u32));
    dib.extend_from_slice(&[0; 16]); // resolution, palette
    for y in (0..h).rev() {
        let line = &buf[y * info.line_size..][..w * channels];
        for p in line.chunks_exact(channels) {
            let (r, g, b, a) = match channels {
                1 => (p[0], p[0], p[0], 255),
                2 => (p[0], p[0], p[0], p[1]),
                3 => (p[0], p[1], p[2], 255),
                _ => (p[0], p[1], p[2], p[3]),
            };
            dib.extend_from_slice(&[b, g, r, a]);
        }
    }
    Some(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_lists_survive_dropfiles() {
        let p = vec![PathBuf::from(r"C:\a b\café.txt"), PathBuf::from(r"D:\x")];
        assert_eq!(drop_files(&dropfiles_for(&p)), Some(p));
    }

    #[test]
    fn images_survive_dib_and_png() {
        // 3 x 2, bottom-up 24-bit DIB with row padding.
        let (w, h) = (3usize, 2usize);
        let stride = (w * 3 + 3) & !3;
        let mut d = vec![0u8; 40];
        d[0..4].copy_from_slice(&40u32.to_le_bytes());
        d[4..8].copy_from_slice(&(w as u32).to_le_bytes());
        d[8..12].copy_from_slice(&(h as u32).to_le_bytes());
        d[12..14].copy_from_slice(&1u16.to_le_bytes());
        d[14..16].copy_from_slice(&24u16.to_le_bytes());
        for y in 0..h {
            let mut line = vec![0u8; stride];
            for x in 0..w {
                line[x * 3..x * 3 + 3].copy_from_slice(&[x as u8, y as u8, 200]);
                // BGR
            }
            d.extend_from_slice(&line);
        }
        let png = dib_to_png(&d).expect("png");
        let dib = png_to_dib(&png).expect("dib");
        // Back as 32-bit bottom-up: same pixels, opaque.
        for y in 0..h {
            for x in 0..w {
                let i = 40 + (y * w + x) * 4;
                assert_eq!(&dib[i..i + 4], &[x as u8, y as u8, 200, 255]);
            }
        }
    }
}

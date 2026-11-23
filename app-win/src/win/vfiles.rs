//! Files from the other side, on this PC's clipboard as *virtual files*
//! (M5c), the way Remote Desktop does it. The clipboard holds a list of
//! names and sizes ("FileGroupDescriptorW"), and each file's contents
//! ("FileContents") come only when an app asks while pasting. Explorer
//! then copies them with its own progress window.
//!
//! A plain file list (CF_HDROP) can't wait for the paste: Explorer reads it
//! the moment it's on the clipboard, to dim files that were cut
//! (m5c-test3), which would fetch every copy at once.
//!
//! The first request for contents fetches the whole copy into Glass's
//! folder (core `ClipLink::fetch`), and each file is then handed over as a
//! stream on its local copy. Calls come in on the clipboard thread (it's
//! the object's COM apartment), which waits meanwhile.

use glass_core::clipboard::{safe_path, ClipLink, Offer};
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::{implement, Ref, BOOL, HRESULT};
use windows::Win32::Foundation::{
    GlobalFree, DATA_S_SAMEFORMATETC, DV_E_FORMATETC, E_NOTIMPL, OLE_E_ADVISENOTSUPPORTED,
    STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION, STG_E_READFAULT, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
    ISequentialStream_Impl, IStream, IStream_Impl, DATADIR_GET, DVASPECT_CONTENT, FORMATETC,
    LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK,
    STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::ReleaseStgMedium;
use windows::Win32::UI::Shell::{
    SHCreateStdEnumFmtEtc, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FILEDESCRIPTORW,
};

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// The formats the object offers.
#[derive(Clone, Copy)]
pub struct Formats {
    pub descriptor: u16,
    pub contents: u16,
    pub drop_effect: u16,
    /// Marks it as Glass's (never read back).
    pub glass: u16,
    /// Asks clipboard history and other watchers to leave it alone.
    pub no_monitor: u16,
}

impl Formats {
    pub fn register(glass: u32, no_monitor: u32, drop_effect: u32) -> Self {
        unsafe {
            Self {
                descriptor: RegisterClipboardFormatW(windows::core::w!("FileGroupDescriptorW"))
                    as u16,
                contents: RegisterClipboardFormatW(windows::core::w!("FileContents")) as u16,
                drop_effect: drop_effect as u16,
                glass: glass as u16,
                no_monitor: no_monitor as u16,
            }
        }
    }
}

#[implement(IDataObject)]
pub struct VirtualFiles {
    link: Arc<ClipLink>,
    offer: Offer,
    fm: Formats,
}

impl VirtualFiles {
    pub fn object(link: Arc<ClipLink>, offer: Offer, fm: Formats) -> IDataObject {
        Self { link, offer, fm }.into()
    }

    /// The file list: names relative to the copy, `\`-separated.
    fn descriptor(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(self.offer.entries.len() as u32).to_le_bytes());
        for e in &self.offer.entries {
            let mut d = FILEDESCRIPTORW {
                dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0) as u32,
                dwFileAttributes: if e.dir {
                    FILE_ATTRIBUTE_DIRECTORY
                } else {
                    FILE_ATTRIBUTE_NORMAL
                },
                nFileSizeHigh: (e.size >> 32) as u32,
                nFileSizeLow: e.size as u32,
                ..Default::default()
            };
            let name: Vec<u16> = safe_path(&e.path)
                .map(|p| p.to_string_lossy().replace('/', "\\"))
                .unwrap_or_default()
                .encode_utf16()
                .take(259)
                .collect();
            // The struct is byte-packed: the name goes in whole.
            let mut file_name = [0u16; 260];
            file_name[..name.len()].copy_from_slice(&name);
            d.cFileName = file_name;
            let raw = unsafe {
                std::slice::from_raw_parts(
                    &d as *const FILEDESCRIPTORW as *const u8,
                    std::mem::size_of::<FILEDESCRIPTORW>(),
                )
            };
            b.extend_from_slice(raw);
        }
        b
    }

    /// A stream on file `index` of the list that hands over its bytes as
    /// they arrive (the first one starts the fetch).
    fn stream(&self, index: i32) -> Option<IStream> {
        let index = usize::try_from(index).ok()?;
        let e = self.offer.entries.get(index)?;
        if e.dir {
            return None;
        }
        let base = self.offer.files.first()?.parent()?;
        let path = base.join(safe_path(&e.path)?);
        self.link.fetch(self.offer.id);
        Some(
            Incoming {
                link: self.link.clone(),
                id: self.offer.id,
                index,
                path,
                size: e.size,
                pos: AtomicU64::new(0),
                file: Mutex::new(None),
            }
            .into(),
        )
    }

    fn formats(&self) -> Vec<FORMATETC> {
        let f = |cf: u16, tymed: i32, lindex: i32| FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed: tymed as u32,
        };
        let mut v = vec![
            f(self.fm.descriptor, TYMED_HGLOBAL.0, -1),
            f(self.fm.contents, TYMED_ISTREAM.0, -1),
        ];
        for cf in [self.fm.drop_effect, self.fm.glass, self.fm.no_monitor] {
            if cf != 0 {
                v.push(f(cf, TYMED_HGLOBAL.0, -1));
            }
        }
        v
    }
}

fn hglobal(data: &[u8]) -> windows::core::Result<STGMEDIUM> {
    unsafe {
        let g = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1))?;
        let p = GlobalLock(g) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(g));
            return Err(E_NOTIMPL.into());
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        let _ = GlobalUnlock(g);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: g },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}

impl IDataObject_Impl for VirtualFiles_Impl {
    fn GetData(&self, f: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let f = unsafe { f.as_ref() }.ok_or(DV_E_FORMATETC)?;
        let hg = f.tymed & TYMED_HGLOBAL.0 as u32 != 0;
        if f.cfFormat == self.fm.descriptor && hg {
            hglobal(&self.descriptor())
        } else if f.cfFormat == self.fm.contents && f.tymed & TYMED_ISTREAM.0 as u32 != 0 {
            let stream = self.stream(f.lindex).ok_or(DV_E_FORMATETC)?;
            Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            })
        } else if f.cfFormat == self.fm.drop_effect && hg {
            hglobal(&1u32.to_le_bytes()) // DROPEFFECT_COPY: never a move
        } else if (f.cfFormat == self.fm.glass || f.cfFormat == self.fm.no_monitor) && hg {
            hglobal(&0u32.to_le_bytes())
        } else {
            Err(DV_E_FORMATETC.into())
        }
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
        let Some(f) = (unsafe { f.as_ref() }) else {
            return DV_E_FORMATETC;
        };
        let ok = self
            .formats()
            .iter()
            .any(|g| g.cfFormat == f.cfFormat && g.tymed & f.tymed != 0);
        if ok {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        _: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> windows::core::Result<()> {
        // Explorer reports how the paste went ("Performed DropEffect"):
        // accepted and ignored.
        if release.as_bool() && !medium.is_null() {
            unsafe { ReleaseStgMedium(medium as *mut STGMEDIUM) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction == DATADIR_GET.0 as u32 {
            unsafe { SHCreateStdEnumFmtEtc(&self.formats()) }
        } else {
            Err(E_NOTIMPL.into())
        }
    }

    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// One pasted file's contents, read as they arrive: Explorer's copy window
/// moves with the transfer instead of waiting for all of it.
#[implement(IStream)]
struct Incoming {
    link: Arc<ClipLink>,
    id: u32,
    index: usize,
    path: PathBuf,
    size: u64,
    pos: AtomicU64,
    file: Mutex<Option<std::fs::File>>,
}

/// A read gives up after this long without a single new byte.
const STALL: std::time::Duration = std::time::Duration::from_secs(60);

impl ISequentialStream_Impl for Incoming_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        use std::io::{Read, Seek, SeekFrom};
        let pos = self.pos.load(Ordering::Relaxed);
        let n = (cb as u64).min(self.size.saturating_sub(pos));
        let done = |got: u32| {
            if !pcbread.is_null() {
                unsafe { *pcbread = got };
            }
        };
        if n == 0 {
            done(0);
            return S_FALSE; // the end
        }
        // Wait for the bytes (they come in order).
        let (mut seen, mut since) = (0, std::time::Instant::now());
        loop {
            match self.link.file_progress(self.id, self.index) {
                None => return STG_E_READFAULT, // the transfer failed
                Some((have, _)) if have >= pos + n => break,
                Some((have, _)) => {
                    if have != seen {
                        (seen, since) = (have, std::time::Instant::now());
                    } else if since.elapsed() > STALL {
                        return STG_E_READFAULT;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
        let mut g = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            match std::fs::File::open(&self.path) {
                Ok(f) => *g = Some(f),
                Err(_) => return STG_E_READFAULT,
            }
        }
        let f = g.as_mut().unwrap();
        let buf = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, n as usize) };
        if f.seek(SeekFrom::Start(pos)).is_err() || f.read_exact(buf).is_err() {
            return STG_E_READFAULT;
        }
        self.pos.store(pos + n, Ordering::Relaxed);
        done(n as u32);
        S_OK
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for Incoming_Impl {
    fn Seek(&self, mv: i64, origin: STREAM_SEEK, newpos: *mut u64) -> windows::core::Result<()> {
        let from = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => self.pos.load(Ordering::Relaxed) as i64,
            STREAM_SEEK_END => self.size as i64,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let p = from
            .checked_add(mv)
            .filter(|p| *p >= 0)
            .ok_or(STG_E_INVALIDFUNCTION)? as u64;
        self.pos.store(p, Ordering::Relaxed);
        if !newpos.is_null() {
            unsafe { *newpos = p };
        }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
    fn CopyTo(
        &self,
        _: Ref<IStream>,
        _: u64,
        _: *mut u64,
        _: *mut u64,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }
    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn Stat(&self, st: *mut STATSTG, _: &STATFLAG) -> windows::core::Result<()> {
        if let Some(st) = unsafe { st.as_mut() } {
            *st = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.size,
                ..Default::default()
            };
        }
        Ok(())
    }
    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

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

use glass_core::clipboard::{fetch_timeout, safe_path, ClipLink, Fetched, Offer};
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use windows::core::{implement, Ref, BOOL, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    GlobalFree, DATA_S_SAMEFORMATETC, DV_E_FORMATETC, E_NOTIMPL, OLE_E_ADVISENOTSUPPORTED, S_OK,
};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, DATADIR_GET,
    DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::ReleaseStgMedium;
use windows::Win32::UI::Shell::{
    SHCreateStdEnumFmtEtc, SHCreateStreamOnFileEx, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI,
    FILEDESCRIPTORW,
};

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const STGM_SHARE_DENY_WRITE: u32 = 0x20;

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
    /// The folder the copy was fetched into, once it has been.
    base: Mutex<Option<PathBuf>>,
}

impl VirtualFiles {
    pub fn object(link: Arc<ClipLink>, offer: Offer, fm: Formats) -> IDataObject {
        Self {
            link,
            offer,
            fm,
            base: Mutex::new(None),
        }
        .into()
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

    /// Where file `index` of the list is once the copy is here (fetching
    /// it the first time).
    fn file(&self, index: i32) -> Option<PathBuf> {
        let e = self.offer.entries.get(usize::try_from(index).ok()?)?;
        if e.dir {
            return None;
        }
        let mut base = self.base.lock().unwrap_or_else(|e| e.into_inner());
        if base.is_none() {
            let timeout = fetch_timeout(self.offer.size);
            if let Some(Fetched::Files(tops)) = self.link.fetch_wait(self.offer.id, timeout) {
                *base = tops
                    .first()
                    .and_then(|t| t.parent())
                    .map(|p| p.to_path_buf());
            }
        }
        Some(base.as_ref()?.join(safe_path(&e.path)?))
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
            let path = self.file(f.lindex).ok_or(DV_E_FORMATETC)?;
            let wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain([0]).collect();
            let stream = unsafe {
                SHCreateStreamOnFileEx(
                    PCWSTR(wide.as_ptr()),
                    STGM_SHARE_DENY_WRITE,
                    0,
                    false,
                    None,
                )?
            };
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

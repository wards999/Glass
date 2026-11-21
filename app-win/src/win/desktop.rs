//! Following the input desktop (M3f). Windows shows the lock screen, the
//! sign-in screen and UAC prompts on separate desktops ("Winlogon", the
//! secure desktop). A thread can only capture or inject input on the desktop
//! it's attached to, and only a SYSTEM process may attach to Winlogon. The
//! agent (run by the Glass service as SYSTEM in the PC's console session)
//! calls `follow` on its capture and input threads whenever the input
//! desktop may have changed.

use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

/// Bumped by the capture thread when it moved to another desktop; other
/// threads (input) re-attach when they see a new value.
pub static GENERATION: AtomicU64 = AtomicU64::new(0);

/// GENERIC_ALL: what attaching for capture and input needs.
const ALL: u32 = 0x1000_0000;

fn name_of(d: HDESK) -> String {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    unsafe {
        let _ = GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(d.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        );
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// The input desktop's name ("Default", "Winlogon", ...), or None if this
/// process may not open it (not SYSTEM and the secure desktop is up).
pub fn input_name() -> Option<String> {
    unsafe {
        let d = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0)).ok()?;
        let n = name_of(d);
        let _ = CloseDesktop(d);
        Some(n)
    }
}

/// Attaches the calling thread to the current input desktop if it isn't
/// already. True if it moved (capture must then start again). Fails quietly
/// when not allowed (not SYSTEM, or the thread owns windows).
pub fn follow() -> bool {
    unsafe {
        let Ok(input) =
            OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(ALL))
        else {
            return false;
        };
        let current = GetThreadDesktop(GetCurrentThreadId()).ok();
        let same = current.is_some_and(|c| name_of(c) == name_of(input));
        if same {
            let _ = CloseDesktop(input);
            return false;
        }
        if SetThreadDesktop(input).is_ok() {
            // The handle stays open while the thread uses it.
            true
        } else {
            let _ = CloseDesktop(input);
            false
        }
    }
}

/// For threads other than capture: re-attach if the capture thread moved.
pub struct Follower {
    seen: u64,
}

impl Follower {
    pub fn new() -> Self {
        Self { seen: u64::MAX }
    }

    pub fn check(&mut self) {
        let g = GENERATION.load(Ordering::Relaxed);
        if g != self.seen {
            self.seen = g;
            follow();
        }
    }
}

impl Default for Follower {
    fn default() -> Self {
        Self::new()
    }
}

/// Called by the capture thread after it moved desktops.
pub fn bump() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

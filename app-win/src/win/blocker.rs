//! Detects when Windows will silently drop injected input: an elevated
//! (administrator) window in front while the host isn't elevated (UIPI), or
//! the secure desktop (UAC prompt, lock screen, Ctrl+Alt+Del). `SendInput`
//! reports success in both cases, so this is the only way to tell the user.

use windows::core::PWSTR;
use windows::Win32::Foundation::POINT;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
    SwitchToThisWindow, WindowFromPoint, GA_ROOT,
};

/// What is blocking input right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    None,
    /// Elevated window in front; holds its program's file name.
    Elevated(String),
    /// UAC prompt, lock screen or similar.
    SecureDesktop,
}

impl Blocker {
    /// `HostNotice` code for the viewer.
    pub fn code(&self) -> u8 {
        match self {
            Blocker::None => glass_core::protocol::NOTICE_NONE,
            Blocker::Elevated(_) => glass_core::protocol::NOTICE_INPUT_BLOCKED_ELEVATED,
            Blocker::SecureDesktop => glass_core::protocol::NOTICE_INPUT_BLOCKED_SECURE_DESKTOP,
        }
    }
}

fn token_elevated(token: HANDLE) -> Option<bool> {
    let mut e = TOKEN_ELEVATION::default();
    let mut len = 0;
    unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut e as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .ok()?;
    }
    Some(e.TokenIsElevated != 0)
}

/// Whether this process (the host) runs elevated.
pub fn self_elevated() -> bool {
    let mut token = HANDLE::default();
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let r = token_elevated(token).unwrap_or(false);
        let _ = CloseHandle(token);
        r
    }
}

fn input_desktop_is_default() -> bool {
    unsafe {
        let Ok(desk) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0))
        else {
            return false; // can't even open it: the secure desktop is up
        };
        let mut name = [0u16; 64];
        let mut needed = 0;
        let ok = GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut needed),
        )
        .is_ok();
        let _ = CloseDesktop(desk);
        let len = name.iter().position(|&c| c == 0).unwrap_or(0);
        !ok || String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case("Default")
    }
}

/// The foreground window's process if it's elevated (file name).
fn elevated_foreground() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 260];
        let mut n = buf.len() as u32;
        let name = if QueryFullProcessImageNameW(
            proc,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut n,
        )
        .is_ok()
        {
            let full = String::from_utf16_lossy(&buf[..n as usize]);
            full.rsplit('\\').next().unwrap_or(&full).to_string()
        } else {
            format!("process {pid}")
        };
        let mut token = HANDLE::default();
        // Opening an elevated process's token is denied to a non-elevated
        // caller: that itself means "elevated".
        let elevated = match OpenProcessToken(proc, TOKEN_QUERY, &mut token) {
            Ok(()) => {
                let e = token_elevated(token).unwrap_or(false);
                let _ = CloseHandle(token);
                e
            }
            Err(_) => true,
        };
        let _ = CloseHandle(proc);
        elevated.then_some(name)
    }
}

/// Best-effort escape while an admin window is in front and the host isn't
/// elevated: bring the top-level window at `(x, y)` (screen pixels) to the
/// front, as the click would have. Windows may refuse (foreground lock);
/// then nothing changes. Returns whether it is now in front.
pub fn try_focus_at((x, y): (i32, i32)) -> bool {
    unsafe {
        let hit = WindowFromPoint(POINT { x, y });
        if hit.is_invalid() {
            return false;
        }
        let top = GetAncestor(hit, GA_ROOT);
        if top.is_invalid() || top == GetForegroundWindow() {
            return false; // clicked the admin window itself
        }
        if SetForegroundWindow(top).as_bool() {
            return true;
        }
        SwitchToThisWindow(top, true);
        GetForegroundWindow() == top
    }
}

/// Checks at most every `interval_us`; cheap between checks.
pub struct BlockerWatch {
    host_elevated: bool,
    last_check_us: u64,
    pub current: Blocker,
}

impl BlockerWatch {
    pub fn new() -> Self {
        Self {
            host_elevated: self_elevated(),
            last_check_us: 0,
            current: Blocker::None,
        }
    }

    /// Re-checks if due. Returns the new state when it changed.
    pub fn poll(&mut self, now_us: u64, interval_us: u64) -> Option<Blocker> {
        if now_us.saturating_sub(self.last_check_us) < interval_us {
            return None;
        }
        self.last_check_us = now_us;
        let b = if !input_desktop_is_default() {
            Blocker::SecureDesktop
        } else if self.host_elevated {
            Blocker::None
        } else {
            elevated_foreground().map_or(Blocker::None, Blocker::Elevated)
        };
        if b != self.current {
            self.current = b.clone();
            Some(b)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_run_on_a_normal_desktop() {
        // CI and dev machines run tests on the normal desktop.
        assert!(input_desktop_is_default());
        let mut w = BlockerWatch::new();
        let _ = w.poll(1, 0);
        assert_ne!(w.current, Blocker::SecureDesktop);
    }
}

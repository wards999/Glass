//! Named pipes between the broker (the Glass service, which keeps the
//! viewer's connection) and its helpers (ADR 0017).
//!
//! Each helper gets two one-way pipes, `<name>-up` (helper → broker) and
//! `<name>-down` (broker → helper). A synchronous pipe handle serialises its
//! reads and writes, so one duplex pipe would stall a write behind a blocked
//! read.
//!
//! Security, since a screen helper runs as SYSTEM and injects input:
//! - Explicit DACLs: SYSTEM, plus the given SIDs (the signed-in user for
//!   the clipboard helper; the current user in local tests).
//! - Both ends check that the process at the other end is this same Glass
//!   program (image path).
//! - The broker always keeps a listening instance, created with
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE`, so another program can't take the name.

use super::Res;
use std::fs::File;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::PathBuf;
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND,
    PIPE_ACCESS_OUTBOUND,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    GetNamedPipeServerSessionId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// One helper's two pipes: `rx` to read, `tx` to write.
pub struct Link {
    pub rx: File,
    pub tx: File,
}

fn path(name: &str, dir: &str) -> String {
    format!(r"\\.\pipe\{name}-{dir}")
}

/// The full path of the program behind process `pid`.
fn image_of(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n);
        let _ = CloseHandle(h);
        ok.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])))
    }
}

/// Is the process at the other end this same Glass program?
fn same_program(pid: u32) -> bool {
    let me = std::env::current_exe().ok();
    match (image_of(pid), me) {
        (Some(a), Some(b)) => {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        }
        _ => false,
    }
}

/// This process's user, as a SID string (for local tests' DACLs).
pub fn current_user_sid() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let sid = token_sid(token);
        let _ = CloseHandle(token);
        sid
    }
}

/// The user process `pid` runs as, as a SID string.
pub fn process_user_sid(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut token = HANDLE::default();
        let ok = OpenProcessToken(h, TOKEN_QUERY, &mut token);
        let _ = CloseHandle(h);
        ok.ok()?;
        let sid = token_sid(token);
        let _ = CloseHandle(token);
        sid
    }
}

/// The user of `token`, as a SID string.
pub fn token_sid(token: HANDLE) -> Option<String> {
    unsafe {
        let mut need = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut need);
        let mut buf = vec![0u8; need as usize];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut _),
            need,
            &mut need,
        )
        .ok()?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).ok()?;
        let out = s.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(s.0 as _)));
        out
    }
}

/// The broker's end: hands out one helper connection at a time, always
/// keeping the next listening instance (so the name stays the broker's).
pub struct Server {
    name: String,
    sddl: String,
    first: bool,
}

impl Server {
    /// `allow`: SIDs besides SYSTEM that may connect.
    pub fn new(name: &str, allow: &[String]) -> Self {
        let mut sddl = String::from("D:P(A;;GA;;;SY)");
        for s in allow {
            sddl.push_str(&format!("(A;;GA;;;{s})"));
        }
        Self {
            name: name.to_string(),
            sddl,
            first: true,
        }
    }

    fn create(&mut self, dir: &str, access: FILE_FLAGS_AND_ATTRIBUTES) -> Res<HANDLE> {
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(self.sddl.as_str()),
                1,
                &mut sd,
                None,
            )?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            let flags = if self.first {
                access | FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                access
            };
            let h = CreateNamedPipeW(
                &HSTRING::from(path(&self.name, dir)),
                flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                1 << 20,
                1 << 20,
                0,
                Some(&sa),
            );
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            if h == INVALID_HANDLE_VALUE {
                return Err(format!(
                    "couldn't open pipe {} (another program may have it): {}",
                    path(&self.name, dir),
                    windows::core::Error::from_thread()
                )
                .into());
            }
            Ok(h)
        }
    }

    /// Waits for the next helper. Connections from any other program, or
    /// any process `check` turns down, are dropped.
    pub fn accept(&mut self, check: &dyn Fn(u32) -> bool) -> Res<Link> {
        loop {
            let up = self.create("up", PIPE_ACCESS_INBOUND)?;
            let down = match self.create("down", PIPE_ACCESS_OUTBOUND) {
                Ok(d) => d,
                Err(e) => {
                    unsafe {
                        let _ = CloseHandle(up);
                    }
                    return Err(e);
                }
            };
            self.first = false;
            let rx = unsafe { File::from_raw_handle(up.0 as _) };
            let tx = unsafe { File::from_raw_handle(down.0 as _) };
            // A client already connected between create and connect counts.
            let connected = |h: HANDLE| match unsafe { ConnectNamedPipe(h, None) } {
                Ok(()) => true,
                Err(e) => e.code() == windows::Win32::Foundation::ERROR_PIPE_CONNECTED.to_hresult(),
            };
            if !connected(up) || !connected(down) {
                continue;
            }
            let mut pid = 0u32;
            let ok =
                unsafe { GetNamedPipeClientProcessId(up, &mut pid) }.is_ok() && same_program(pid);
            let mut pid2 = 0u32;
            let ok2 =
                unsafe { GetNamedPipeClientProcessId(down, &mut pid2) }.is_ok() && pid2 == pid;
            if ok && ok2 && check(pid) {
                return Ok(Link { rx, tx });
            }
            // Not Glass: drop it (closing the handles disconnects it).
        }
    }
}

/// The helper's end: connects to the broker's pipes, waiting up to
/// `wait_ms` for them, and checks the broker is this same Glass program.
pub fn connect(name: &str, wait_ms: u64) -> Res<Link> {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    let open = |dir: &str, write: bool| -> std::io::Result<File> {
        std::fs::OpenOptions::new()
            .read(!write)
            .write(write)
            .open(path(name, dir))
    };
    let tx = loop {
        match open("up", true) {
            Ok(f) => break f,
            Err(_) if std::time::Instant::now() < until => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            Err(e) => return Err(format!("the Glass service isn't answering ({e})").into()),
        }
    };
    let rx = loop {
        match open("down", false) {
            Ok(f) => break f,
            Err(_) if std::time::Instant::now() < until => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            Err(e) => return Err(format!("the Glass service isn't answering ({e})").into()),
        }
    };
    let mut pid = 0u32;
    let h = HANDLE(tx.as_raw_handle() as _);
    let glass = unsafe { GetNamedPipeServerProcessId(h, &mut pid) }.is_ok() && same_program(pid);
    // A user can't look into the SYSTEM service's process; but only
    // services run in session 0, which no ordinary program can start in.
    let mut session = u32::MAX;
    let service = unsafe { GetNamedPipeServerSessionId(h, &mut session) }.is_ok() && session == 0;
    if !glass && !service {
        return Err("the pipe isn't the Glass service's".into());
    }
    Ok(Link { rx, tx })
}

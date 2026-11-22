//! The Glass service (M3f): unattended access, including the lock screen,
//! the sign-in screen and UAC prompts.
//!
//! Split so the service itself stays small and rarely changes (ADR 0015):
//! - `glass-cli service run` is the service (LocalSystem, auto start). It
//!   only keeps one agent running in the PC's console session (the one on its
//!   physical screen), starting a new one when that session changes (sign-in,
//!   sign-out, fast user switching) or the agent stops.
//! - `glass-cli agent` (started by the service as SYSTEM in that session) does
//!   the sharing: Desktop Duplication that follows the input desktop, input
//!   on whichever desktop is showing, this PC's identity kept with
//!   machine-scope DPAPI in a folder only SYSTEM and administrators can read.
//!   It writes a small status file (who's connected, a pairing code) that the
//!   Glass window shows.
//! - `glass-cli service install` / `uninstall` (as administrator): copies
//!   this build to Program Files, moves this PC's identity over so pairings
//!   carry on, registers the service, allows it through Windows Firewall on
//!   private networks, starts it. Installing again updates it.
//!
//! LATER(M3f): Ctrl+Alt+Del (SendSAS) for PCs set to require it; updates
//! through the service without an admin prompt.

use super::Res;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
    TOKEN_ALL_ACCESS, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::Services::*;
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
    SetEvent, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_INFORMATION, STARTUPINFOW,
};

pub const NAME: &str = "Glass";

/// `C:\Program Files\Glass`.
pub fn install_dir() -> PathBuf {
    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into()))
        .join("Glass")
}

/// `C:\ProgramData\Glass`: the status file and the service log
/// (`GLASS_DATA` overrides it for local checks).
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("GLASS_DATA") {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into()))
        .join("Glass")
}

/// The agent's identity and paired devices (SYSTEM and administrators only).
pub fn host_dir() -> PathBuf {
    data_dir().join("host")
}

pub fn status_path() -> PathBuf {
    data_dir().join("status.txt")
}

/// Where the Glass window asks the agent to end the current session (any
/// user on the PC may write here; the agent only looks for one file).
fn control_dir() -> PathBuf {
    data_dir().join("control")
}

/// Sharing is switched off (the Glass window's Share switch): the service
/// stays installed and running but runs no agent, across restarts too.
pub fn sharing_off() -> bool {
    control_dir().join("sharing-off").exists()
}

/// The Glass window's Clipboard setting, for the service's agent (M5c).
pub fn clipboard_off() -> bool {
    control_dir().join("no-clipboard").exists()
}

/// Passes the Clipboard setting on to the service (if it's installed).
pub fn set_clipboard(on: bool) {
    let f = control_dir().join("no-clipboard");
    if on {
        let _ = std::fs::remove_file(f);
    } else if control_dir().is_dir() {
        let _ = std::fs::write(f, b"1");
    }
}

/// The Share switch, when the service is installed.
pub fn set_sharing(on: bool) -> bool {
    let f = control_dir().join("sharing-off");
    if on {
        std::fs::remove_file(&f).is_ok() || !f.exists()
    } else {
        std::fs::write(&f, b"1").is_ok()
    }
}

/// The window's End session for a session the service hosts.
pub fn request_end() -> bool {
    std::fs::write(control_dir().join("end-session"), b"1").is_ok()
}

/// The agent: watches for End session from the window.
pub fn watch_end(end: std::sync::Arc<AtomicBool>) {
    std::thread::spawn(move || loop {
        let f = control_dir().join("end-session");
        if f.exists() {
            let _ = std::fs::remove_file(&f);
            end.store(true, Ordering::Relaxed);
        }
        if sharing_off() {
            // Switched off: end any session properly (the viewer is told,
            // so it doesn't reconnect), then stop.
            end.store(true, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(1200));
            let _ = std::fs::remove_file(status_path());
            std::process::exit(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    });
}

fn log(line: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(data_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir().join("service.log"))
    {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let _ = writeln!(f, "{t} {line}");
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- the service

static STOP_EVENT: AtomicIsize = AtomicIsize::new(0);
static CHECK_NOW: AtomicBool = AtomicBool::new(false);
static STATUS: AtomicIsize = AtomicIsize::new(0);

fn set_state(state: SERVICE_STATUS_CURRENT_STATE, exit: u32) {
    let h = SERVICE_STATUS_HANDLE(STATUS.load(Ordering::Relaxed) as *mut _);
    let s = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
        } else {
            0
        },
        dwWin32ExitCode: exit,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 3000,
    };
    unsafe {
        let _ = SetServiceStatus(h, &s);
    }
}

unsafe extern "system" fn handler(
    control: u32,
    _event: u32,
    _data: *mut core::ffi::c_void,
    _ctx: *mut core::ffi::c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_state(SERVICE_STOP_PENDING, 0);
            let _ = SetEvent(HANDLE(STOP_EVENT.load(Ordering::Relaxed) as *mut _));
        }
        SERVICE_CONTROL_SESSIONCHANGE => CHECK_NOW.store(true, Ordering::Relaxed),
        _ => {}
    }
    0 // NO_ERROR
}

/// A running agent.
struct Agent {
    process: HANDLE,
    session: u32,
}

impl Agent {
    fn exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_OBJECT_0 }
    }

    fn kill(self) {
        unsafe {
            let _ = TerminateProcess(self.process, 0);
            let _ = WaitForSingleObject(self.process, 3000);
            let _ = CloseHandle(self.process);
        }
    }
}

/// Starts `glass-cli agent` as SYSTEM in `session` (on its interactive
/// window station, so it can reach that session's desktops).
fn launch_agent(session: u32) -> Res<Agent> {
    unsafe {
        let mut me = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
            &mut me,
        )?;
        let mut token = HANDLE::default();
        let dup = DuplicateTokenEx(
            me,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        );
        let _ = CloseHandle(me);
        dup?;
        SetTokenInformation(token, TokenSessionId, &session as *const u32 as *const _, 4)?;
        let exe = std::env::current_exe()?;
        let app = HSTRING::from(exe.as_os_str());
        let mut cmd = wide(&format!("\"{}\" agent", exe.display()));
        let mut desk = wide("winsta0\\default");
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desk.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let dir = HSTRING::from(exe.parent().unwrap_or(Path::new(".")).as_os_str());
        let r = CreateProcessAsUserW(
            Some(token),
            &app,
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            None,
            &dir,
            &si,
            &mut pi,
        );
        let _ = CloseHandle(token);
        r?;
        let _ = CloseHandle(pi.hThread);
        Ok(Agent {
            process: pi.hProcess,
            session,
        })
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let Ok(h) = RegisterServiceCtrlHandlerExW(w!("Glass"), Some(handler), None) else {
        return;
    };
    STATUS.store(h.0 as isize, Ordering::Relaxed);
    let Ok(stop) = CreateEventW(None, true, false, None) else {
        set_state(SERVICE_STOPPED, 1);
        return;
    };
    STOP_EVENT.store(stop.0 as isize, Ordering::Relaxed);
    set_state(SERVICE_RUNNING, 0);
    log("service started");
    let mut agent: Option<Agent> = None;
    let mut failures = 0u32;
    loop {
        // Every second, or at once after a session change.
        let waited = WaitForSingleObject(
            stop,
            if CHECK_NOW.swap(false, Ordering::Relaxed) {
                0
            } else {
                1000
            },
        );
        if waited == WAIT_OBJECT_0 {
            break;
        }
        // Switched off in the window: no agent until it's switched on.
        if sharing_off() {
            if let Some(a) = agent.take() {
                // The agent ends the session and stops by itself; wait a
                // moment before making sure.
                let _ = WaitForSingleObject(a.process, 3000);
                a.kill();
                let _ = std::fs::remove_file(status_path());
                log("sharing switched off");
            }
            failures = 0;
            continue;
        }
        let session = WTSGetActiveConsoleSessionId();
        if session == u32::MAX {
            continue; // between sessions (switching users)
        }
        let stale = agent
            .as_ref()
            .is_some_and(|a| a.session != session || a.exited());
        if stale {
            if let Some(a) = agent.take() {
                a.kill();
            }
        }
        if agent.is_none() {
            // Back off if it keeps failing (no GPU, a bad build): 1 s up to 30 s.
            if failures > 0 {
                let wait = 1000 * failures.min(30);
                if WaitForSingleObject(stop, wait) == WAIT_OBJECT_0 {
                    break;
                }
            }
            match launch_agent(session) {
                Ok(a) => {
                    log(&format!("agent started in session {session}"));
                    agent = Some(a);
                    failures = 0;
                }
                Err(e) => {
                    log(&format!("couldn't start the agent: {e}"));
                    failures += 1;
                }
            }
        } else if agent.as_ref().is_some_and(|a| a.exited()) {
            failures += 1;
        }
    }
    if let Some(a) = agent.take() {
        a.kill();
    }
    let _ = std::fs::remove_file(status_path());
    log("service stopped");
    set_state(SERVICE_STOPPED, 0);
}

/// `glass-cli service run`: what the Service Control Manager starts.
pub fn run() -> Res<()> {
    let mut name = wide(NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.map_err(|e| {
        format!("not started by Windows as a service ({e}); use `glass-cli service install`").into()
    })
}

// ---------------------------------------------------------------- the agent's status

/// What the agent tells the Glass window (`C:\ProgramData\Glass\status.txt`,
/// readable by everyone on the PC, writable only by SYSTEM).
#[derive(Default, Clone, PartialEq)]
pub struct Status {
    /// The latest status line ("Waiting for a viewer", "Viewer connected: ...").
    pub line: String,
    /// A pairing code to show and who asked, while it's valid.
    pub pin: Option<(String, String)>,
    /// Someone is viewing this PC.
    pub connected: bool,
}

impl Status {
    pub fn write(&self) {
        let mut s = format!("connected={}\nline={}\n", self.connected as u8, self.line);
        if let Some((pin, who)) = &self.pin {
            s.push_str(&format!("pin={pin}\nwho={who}\n"));
        }
        let _ = std::fs::create_dir_all(data_dir());
        let tmp = status_path().with_extension("tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, status_path());
        }
    }

    /// The agent's status, if it's running (written in the last minute).
    pub fn read() -> Option<Status> {
        let p = status_path();
        let age = std::fs::metadata(&p)
            .ok()?
            .modified()
            .ok()?
            .elapsed()
            .ok()?;
        if age > std::time::Duration::from_secs(60) {
            return None;
        }
        let text = std::fs::read_to_string(p).ok()?;
        let mut st = Status::default();
        let (mut pin, mut who) = (None, String::new());
        for l in text.lines() {
            match l.split_once('=') {
                Some(("connected", v)) => st.connected = v == "1",
                Some(("line", v)) => st.line = v.to_string(),
                Some(("pin", v)) => pin = Some(v.to_string()),
                Some(("who", v)) => who = v.to_string(),
                _ => {}
            }
        }
        st.pin = pin.map(|p| (p, who));
        Some(st)
    }
}

/// `glass-cli agent`: sets up the agent's identity (machine-scope DPAPI, in
/// the protected folder). Called before the host starts.
pub fn agent_env() {
    if std::env::var_os("GLASS_HOME").is_none() {
        std::env::set_var("GLASS_HOME", host_dir());
    }
    super::dpapi::set_machine_scope();
}

// ---------------------------------------------------------------- state for the window

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    NotInstalled,
    Stopped,
    Running,
}

/// The service's state (any user may ask).
pub fn state() -> State {
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_CONNECT) else {
            return State::NotInstalled;
        };
        let st = match OpenServiceW(scm, w!("Glass"), SERVICE_QUERY_STATUS) {
            Ok(svc) => {
                let mut s = SERVICE_STATUS::default();
                let running =
                    QueryServiceStatus(svc, &mut s).is_ok() && s.dwCurrentState == SERVICE_RUNNING;
                let _ = CloseServiceHandle(svc);
                if running {
                    State::Running
                } else {
                    State::Stopped
                }
            }
            Err(_) => State::NotInstalled,
        };
        let _ = CloseServiceHandle(scm);
        st
    }
}

/// This process runs in a Remote Desktop session (whose screen no capture
/// API can see; the PC's own screen is locked meanwhile).
pub fn in_remote_session() -> bool {
    unsafe {
        let mut s = 0u32;
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut s);
        s != WTSGetActiveConsoleSessionId()
    }
}

/// Hands this Remote Desktop session to the PC's own screen (`tscon`, as
/// administrator: Windows asks). Remote Desktop disconnects; the session
/// carries on, unlocked, on the PC's screen. True if it was started.
pub fn move_session_to_console() -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let mut s = 0u32;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut s);
        let args = wide(&format!("/c tscon {s} /dest:console"));
        let r = ShellExecuteW(
            None,
            w!("runas"),
            w!("cmd.exe"),
            PCWSTR(args.as_ptr()),
            None,
            SW_HIDE,
        );
        r.0 as isize > 32
    }
}

/// Runs `glass-cli service <verb>` as administrator (Windows asks); for the
/// Glass window's Install / Remove. True if it was started.
pub fn run_elevated(verb: &str) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let Some(cli) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("glass-cli.exe")))
    else {
        return false;
    };
    let exe = wide(&cli.to_string_lossy());
    let args = wide(&format!("service {verb} --pause"));
    unsafe {
        let r = ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(exe.as_ptr()),
            PCWSTR(args.as_ptr()),
            None,
            SW_SHOWNORMAL,
        );
        r.0 as isize > 32
    }
}

// ---------------------------------------------------------------- install

fn sh(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn stop_service(wait_s: u64) {
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_CONNECT) else {
            return;
        };
        if let Ok(svc) = OpenServiceW(scm, w!("Glass"), SERVICE_STOP | SERVICE_QUERY_STATUS) {
            let mut s = SERVICE_STATUS::default();
            let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut s);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(wait_s);
            while std::time::Instant::now() < until {
                if QueryServiceStatus(svc, &mut s).is_ok() && s.dwCurrentState == SERVICE_STOPPED {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            let _ = CloseServiceHandle(svc);
        }
        let _ = CloseServiceHandle(scm);
    }
}

/// Moves this user's Glass identity to the service (re-encrypted for the
/// machine), so devices paired with this PC keep working. Only if the
/// service has none yet.
fn migrate_identity() -> Res<String> {
    let dst = host_dir();
    if dst.join("identity.key").exists() {
        return Ok("kept the service's existing identity and paired devices".into());
    }
    let Some(src) = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Glass")) else {
        return Ok("no user identity found: the service makes a new one (pair again)".into());
    };
    let key = src.join("identity.key");
    let Ok(stored) = std::fs::read(&key) else {
        return Ok("no user identity found: the service makes a new one (pair again)".into());
    };
    let machine = super::dpapi::user_to_machine(&stored)?;
    std::fs::write(dst.join("identity.key"), machine)?;
    for e in std::fs::read_dir(&src)?.flatten() {
        let p = e.path();
        if p.is_file() && p.file_name().is_some_and(|n| n != "identity.key") {
            let _ = std::fs::copy(&p, dst.join(p.file_name().unwrap()));
        }
    }
    Ok("moved this PC's identity and paired devices to the service".into())
}

fn install() -> Res<()> {
    if !super::blocker::self_elevated() {
        return Err("run as administrator (right-click > Run as administrator, or Glass's Settings > Install)".into());
    }
    let here = std::env::current_exe()?
        .parent()
        .ok_or("no folder")?
        .to_path_buf();
    let dst = install_dir();
    println!("Installing the Glass service to {} ...", dst.display());
    stop_service(15);
    std::fs::create_dir_all(&dst)?;
    if here != dst {
        for e in std::fs::read_dir(&here)?.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .map(|x| x.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if p.is_file() && matches!(ext.as_str(), "exe" | "dll" | "txt" | "cmd") {
                let name = p.file_name().unwrap();
                if name.to_string_lossy().starts_with("results-") {
                    continue;
                }
                std::fs::copy(&p, dst.join(name))
                    .map_err(|e| format!("copying {}: {e}", name.to_string_lossy()))?;
            }
        }
    }
    // The agent's identity: SYSTEM and administrators only (well-known SIDs,
    // so it works in any Windows language).
    let hd = host_dir();
    std::fs::create_dir_all(&hd)?;
    let hds = hd.to_string_lossy().to_string();
    if !sh(
        "icacls",
        &[
            &hds,
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
        ],
    ) {
        return Err("couldn't restrict the service's identity folder".into());
    }
    println!("  {}", migrate_identity()?);
    // End session from the window: users may drop a file here.
    let cd = control_dir();
    std::fs::create_dir_all(&cd)?;
    let cds = cd.to_string_lossy().to_string();
    sh("icacls", &[&cds, "/grant", "*S-1-5-32-545:(OI)(CI)M"]);
    let cli = dst.join("glass-cli.exe");
    let bin = format!("\"{}\" service run", cli.display());
    let svc_exists = sh("sc.exe", &["query", NAME]);
    let was_off = svc_exists && is_off();
    let ok = if svc_exists {
        let start = if was_off { "demand" } else { "auto" };
        sh(
            "sc.exe",
            &["config", NAME, "binPath=", &bin, "start=", start],
        )
    } else {
        sh(
            "sc.exe",
            &[
                "create",
                NAME,
                "binPath=",
                &bin,
                "start=",
                "auto",
                "DisplayName=",
                "Glass",
            ],
        )
    };
    if !ok {
        return Err("couldn't register the service".into());
    }
    sh(
        "sc.exe",
        &[
            "description",
            NAME,
            "Glass unattended access: lets paired devices view and control this PC, including the lock screen and UAC prompts.",
        ],
    );
    sh(
        "sc.exe",
        &[
            "failure",
            NAME,
            "reset=",
            "86400",
            "actions=",
            "restart/2000/restart/5000/restart/10000",
        ],
    );
    // Windows Firewall: the installed glass-cli.exe on private networks.
    let prog = format!("program={}", cli.display());
    sh(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            "name=Glass (service)",
        ],
    );
    sh(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "add",
            "rule",
            "name=Glass (service)",
            "dir=in",
            "action=allow",
            &prog,
            "profile=private",
            "enable=yes",
        ],
    );
    if was_off {
        println!("Updated. The service stays off (turn it on in Glass's Settings).");
        log("updated (off)");
        return Ok(());
    }
    if !sh("sc.exe", &["start", NAME]) {
        return Err(
            "installed, but the service didn't start (see C:\\ProgramData\\Glass\\service.log)"
                .into(),
        );
    }
    println!("Done. This PC can now be viewed and controlled by paired devices at any time,");
    println!(
        "including the lock screen and UAC prompts. Remove it with: glass-cli service uninstall"
    );
    log("installed");
    Ok(())
}

/// Turned off: installed, but set not to start with Windows (`sc config
/// start= demand`).
fn is_off() -> bool {
    std::process::Command::new("sc.exe")
        .args(["qc", NAME])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("DEMAND_START"))
}

/// `glass-cli service off`: stops it and keeps it off after a restart;
/// everything stays installed.
fn turn_off() -> Res<()> {
    if !super::blocker::self_elevated() {
        return Err("run as administrator".into());
    }
    stop_service(15);
    if !sh("sc.exe", &["config", NAME, "start=", "demand"]) {
        return Err("couldn't change the service (is it installed?)".into());
    }
    let _ = std::fs::remove_file(status_path());
    println!(
        "The Glass service is off: this PC can't be reached from far away until you turn it on."
    );
    log("turned off");
    Ok(())
}

/// `glass-cli service on`: starts with Windows again, and now.
fn turn_on() -> Res<()> {
    if !super::blocker::self_elevated() {
        return Err("run as administrator".into());
    }
    if !sh("sc.exe", &["config", NAME, "start=", "auto"]) {
        return Err("couldn't change the service (is it installed?)".into());
    }
    set_sharing(true);
    if !sh("sc.exe", &["start", NAME]) && state() != State::Running {
        return Err("the service didn't start (see C:\\ProgramData\\Glass\\service.log)".into());
    }
    println!("The Glass service is on: paired devices can reach this PC at any time.");
    log("turned on");
    Ok(())
}

fn uninstall(purge: bool) -> Res<()> {
    if !super::blocker::self_elevated() {
        return Err("run as administrator".into());
    }
    println!("Removing the Glass service ...");
    stop_service(15);
    sh("sc.exe", &["delete", NAME]);
    sh(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            "name=Glass (service)",
        ],
    );
    let dst = install_dir();
    let running_from_it = std::env::current_exe()
        .ok()
        .is_some_and(|e| e.starts_with(&dst));
    if !running_from_it {
        let _ = std::fs::remove_dir_all(&dst);
    }
    let _ = std::fs::remove_file(status_path());
    if purge {
        let _ = std::fs::remove_dir_all(data_dir());
        println!("  removed the service's identity and paired devices too");
    } else {
        println!(
            "  kept the service's identity in {} (add --purge to remove it)",
            host_dir().display()
        );
    }
    if running_from_it {
        println!(
            "  {} is in use; delete it after this window closes",
            dst.display()
        );
    }
    println!("Done.");
    Ok(())
}

/// `glass-cli service install|uninstall [--purge]|run|status`.
pub fn command(args: &[String]) -> Res<()> {
    let pause = args.iter().any(|a| a == "--pause");
    let r = match args.first().map(String::as_str) {
        Some("run") => return run(),
        Some("install") => install(),
        Some("uninstall") => uninstall(args.iter().any(|a| a == "--purge")),
        Some("on") => turn_on(),
        Some("off") => turn_off(),
        Some("status") => {
            println!("service: {:?}", state());
            if let Some(s) = Status::read() {
                println!(
                    "agent: {}{}",
                    s.line,
                    if s.connected { " (connected)" } else { "" }
                );
            }
            Ok(())
        }
        _ => {
            Err("usage: glass-cli service install | on | off | uninstall [--purge] | status".into())
        }
    };
    if let Err(e) = &r {
        println!("error: {e}");
    }
    if pause {
        // Started from the Glass window: keep the result on screen.
        println!("\nPress Enter to close.");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }
    r
}

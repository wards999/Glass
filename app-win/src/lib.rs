//! Glass for Windows: host and client roles. Two programs share this code:
//! `Glass.exe` (the window you double-click, no console) and `glass-cli.exe`
//! (the command line, used by host.cmd / connect.cmd and CI).
//!
//! Commands (glass-cli):
//!   glass host [opts]                  stream a monitor live to a paired viewer
//!   glass connect [HOST] [opts]        view and control a paired host (a Mac)
//!   glass devices                      this PC's identity and paired devices
//!   glass forget NAME|FINGERPRINT      unpair a device
//!   glass probe HOST [opts]            headless viewer: receive and time, no decode
//!   glass serve FILE.glsr              stream a recording as if live (tests)
//!   glass list                         list monitors and GPUs
//!   glass record [opts] out.glsr       capture a monitor and encode HEVC to a recording
//!   glass dump in.glsr out.h265        write the raw Annex-B stream (for ffplay)
//!

/// Build version: the test-build tag when packaged, else the crate version.
const VERSION: &str = match option_env!("GLASS_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[cfg(windows)]
pub mod connect;
pub mod probe;
#[cfg(windows)]
pub mod win;

use glass_core::identity::{default_dir, fingerprint_short, Device, ROLE_HOST, ROLE_VIEWER};
use glass_core::net::HostEvent;
use std::sync::Arc;

/// This PC's identity and paired devices (`%APPDATA%\Glass`, or `GLASS_HOME`).
/// The private key is DPAPI-encrypted on Windows.
pub fn device() -> Result<Arc<Device>, Box<dyn std::error::Error>> {
    let dir = default_dir().ok_or("no per-user app-data folder (set GLASS_HOME)")?;
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Glass PC".into());
    #[cfg(windows)]
    let protector = Some(&win::dpapi::PROTECTOR);
    #[cfg(not(windows))]
    let protector = None;
    Device::open(&dir, &name, protector)
        .map_err(|e| format!("device identity in {}: {e}", dir.display()).into())
}

/// Host-side events as console lines.
pub fn print_event(e: HostEvent) {
    match e {
        HostEvent::PairingPin {
            device,
            pin,
            new,
            addr,
        } => {
            let (a, b) = pin.split_at(3);
            println!(
                "\n*** Pairing request from \"{device}\" ({}). {} PIN:  {a} {b}  ***\n    Type it on that device. Valid for 2 minutes. Only pair devices you own.",
                addr.ip(),
                if new { "Its" } else { "Same" }
            );
        }
        HostEvent::Paired { device } => {
            println!("Paired with \"{device}\". It can now view and control this PC (`glass forget` to undo).")
        }
        HostEvent::WrongPin { device, left } => println!(
            "Wrong PIN from \"{device}\".{}",
            if left == 0 {
                " A new PIN will be shown on the next try.".to_string()
            } else {
                format!(" {left} more tries with this PIN.")
            }
        ),
        HostEvent::PairingLocked => println!(
            "Pairing is switched off after too many wrong PINs. Restart host.cmd to pair a device."
        ),
        HostEvent::Refused { fingerprint, addr } => println!(
            "Refused a device that isn't paired with this PC ({}, id {fingerprint}). Pair it from the device first.",
            addr.ip()
        ),
        HostEvent::Denied { device } => println!("Did not allow \"{device}\" to connect."),
        HostEvent::Loop { device } => println!(
            "Refused \"{device}\": this PC is already viewing it (directly or through another machine), so it would be a loop."
        ),
    }
}

/// `glass devices`
fn devices() -> Result<(), Box<dyn std::error::Error>> {
    let d = device()?;
    println!(
        "{}\nThis PC: \"{}\", id {}",
        banner(),
        d.name,
        fingerprint_short(&d.fingerprint())
    );
    let peers = d.listed_peers();
    if peers.is_empty() {
        println!("No paired devices. Run host.cmd and connect from the Mac to pair.");
    }
    for p in peers {
        let mut roles = Vec::new();
        if p.roles & ROLE_VIEWER != 0 {
            roles.push("may view and control this PC");
        }
        if p.roles & ROLE_HOST != 0 {
            roles.push("this PC may view it");
        }
        println!(
            "  \"{}\"  id {}  ({})",
            p.name,
            fingerprint_short(&p.fingerprint),
            roles.join("; ")
        );
    }
    Ok(())
}

/// `glass forget NAME|FINGERPRINT`
fn forget(what: &str) -> Result<(), Box<dyn std::error::Error>> {
    let d = device()?;
    let found = d.find(what);
    match found.as_slice() {
        [] => Err(format!("no paired device matches \"{what}\" (see `glass devices`)").into()),
        [p] => {
            d.forget(&p.fingerprint)?;
            println!(
                "Forgot \"{}\" (id {}). It can't connect until it pairs again.",
                p.name,
                fingerprint_short(&p.fingerprint)
            );
            Ok(())
        }
        _ => Err(format!(
            "\"{what}\" matches {} devices; use more of the id from `glass devices`",
            found.len()
        )
        .into()),
    }
}

/// `glass find`: Glass hosts on the local network (3 s of listening).
#[cfg(windows)]
/// `glass-cli results NAME|ADDRESS[:PORT]`: a paired machine's newest
/// session logs (a results-only connection, v16: no session starts there),
/// printed and saved to `%LOCALAPPDATA%\Glass\fetched\<name>.txt`.
#[cfg(windows)]
fn results(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let want = args
        .first()
        .ok_or("usage: glass results NAME|ADDRESS[:PORT]")?;
    let (addr, port, name) = match want.rsplit_once(':') {
        Some((a, p)) if p.parse::<u16>().is_ok() => (a.to_string(), p.parse()?, a.to_string()),
        _ if want.parse::<std::net::Ipv4Addr>().is_ok() => (
            want.clone(),
            glass_core::protocol::DEFAULT_PORT,
            want.clone(),
        ),
        _ => {
            win::browse::start()?;
            let mut found = None;
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                found = win::browse::list()
                    .into_iter()
                    .find(|f| f.name.eq_ignore_ascii_case(want));
                if found.is_some() {
                    break;
                }
            }
            win::browse::stop();
            let f = found
                .ok_or_else(|| format!("{want} isn't sharing on this network (try its address)"))?;
            (f.address, f.port, f.name)
        }
    };
    let text = glass_core::net::Client::fetch_results(&device()?, &addr, port)
        .map_err(|e| format!("{name}: {e:?}"))?;
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Glass")
        .join("fetched");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!(
        "{}.txt",
        name.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_")
    ));
    std::fs::write(&file, &text)?;
    print!("{text}");
    eprintln!("(saved to {})", file.display());
    Ok(())
}

fn find() -> Result<(), Box<dyn std::error::Error>> {
    win::browse::start()?;
    std::thread::sleep(std::time::Duration::from_secs(3));
    let found = win::browse::list();
    if found.is_empty() {
        println!("No Glass hosts found (they may hide behind router settings; typing an address still works).");
    }
    for f in found {
        println!(
            "{:<32} {}:{}  {}  id {}  protocol {}",
            f.name,
            f.address,
            f.port,
            f.os.as_deref().unwrap_or("?"),
            f.fp.as_deref().unwrap_or("?"),
            f.version.as_deref().unwrap_or("?")
        );
    }
    win::browse::stop();
    Ok(())
}

/// "Glass (Windows) <version> | core <v> | protocol v<n>"
pub fn banner() -> String {
    format!(
        "Glass (Windows) {} | core {} | protocol v{} | build {}",
        VERSION,
        glass_core::VERSION,
        glass_core::protocol::PROTOCOL_VERSION,
        glass_core::BUILD
    )
}

fn usage() -> ! {
    eprintln!(
        "{}

usage:
  glass host [--monitor N] [--chroma 420|444] [--mbps M] [--port P] [--log FILE] [--ask]
  glass connect [HOST[:PORT]] [--pin PIN] [--windowed] [--keys pc|labelled] [--seconds S] [--log FILE]
  glass devices
  glass forget NAME|FINGERPRINT
  glass probe HOST [--port P] [--pin PIN] [--seconds S] [--verify FILE.glsr] [--min-frames N]
  glass serve FILE.glsr [--port P] [--pin PIN]
  glass list
  glass find                         Glass hosts on the local network
  glass results NAME|ADDRESS[:PORT]  a paired machine's newest session results
  glass vdisplay install|uninstall|status   the virtual display (as administrator)
  glass record [--monitor N] [--seconds S] [--chroma 420|444] [--mbps M] out.glsr
  glass dump in.glsr out.h265
  glass pack in.h265 out.glsr WIDTH HEIGHT FPS 420|444

Identity and paired devices: %APPDATA%\\Glass (or GLASS_HOME).",
        banner()
    );
    std::process::exit(2)
}

fn dump(input: &str, output: &str) -> Result<(), Box<dyn std::error::Error>> {
    let buf = std::fs::read(input)?;
    let r = glass_core::recording::RecordingReader::new(&buf).map_err(|e| format!("{e:?}"))?;
    let h = r.header;
    let mut out = Vec::with_capacity(buf.len());
    let mut n = 0;
    for f in r {
        let (_, data) = f.map_err(|e| format!("{e:?}"))?;
        out.extend_from_slice(data);
        n += 1;
    }
    std::fs::write(output, out)?;
    println!(
        "{n} frames, {}x{} chroma={} -> {output}",
        h.width,
        h.height,
        if h.chroma == 1 { "444" } else { "420" }
    );
    Ok(())
}

/// Packs a raw HEVC Annex-B stream (e.g. from ffmpeg) into a `.glsr` with
/// synthetic timestamps. Used to make CI fixtures from test patterns.
fn pack(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use glass_core::annexb;
    use glass_core::recording::*;
    let [input, output, w, h, fps, chroma] = args else {
        return Err("usage: glass pack in.h265 out.glsr WIDTH HEIGHT FPS 420|444".into());
    };
    let data = std::fs::read(input)?;
    let fps: u32 = fps.parse()?;
    let header = RecordingHeader {
        codec: CODEC_HEVC,
        chroma: if chroma == "444" {
            CHROMA_444
        } else {
            CHROMA_420
        },
        width: w.parse()?,
        height: h.parse()?,
        fps,
    };
    let mut wr = RecordingWriter::new(Vec::new(), &header)?;
    let mut n = 0u32;
    for au in annexb::access_units(&data) {
        let key = annexb::nal_units(au).any(annexb::is_irap);
        let meta = FrameMeta {
            frame_id: n,
            flags: if key {
                glass_core::protocol::FLAG_IDR
            } else {
                0
            },
            capture_ts_us: n as u64 * 1_000_000 / fps as u64,
            encode_us: 0,
            len: au.len() as u32,
        };
        wr.write_frame(&meta, au)?;
        n += 1;
    }
    std::fs::write(output, wr.into_inner())?;
    println!("packed {n} frames -> {output}");
    Ok(())
}

/// The command line (`glass-cli.exe`).
/// Writes any panic to `crash.txt` next to the program (or in the temp
/// folder): a panic inside a window callback ends the program at once with
/// no message, and this is the only trace (m5c-test3: the viewer vanished
/// after a Wi-Fi drop).
fn crash_log() {
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let text = format!(
            "{}\nthread {:?}: {info}\n{}\n\n",
            banner(),
            std::thread::current().name().unwrap_or("?"),
            std::backtrace::Backtrace::force_capture()
        );
        let next_to_exe = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("crash.txt")));
        for path in next_to_exe
            .into_iter()
            .chain([std::env::temp_dir().join("Glass-crash.txt")])
        {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                if f.write_all(text.as_bytes()).is_ok() {
                    break;
                }
            }
        }
        eprintln!("{text}");
    }));
}

pub fn cli_main() {
    crash_log();
    #[cfg(windows)]
    win::register_results();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        #[cfg(windows)]
        Some("app") => win::app::run(args.iter().any(|a| a == "--share")),
        Some("dump") if args.len() == 3 => dump(&args[1], &args[2]),
        Some("pack") => pack(&args[1..]),
        Some("probe") => probe::probe(&args[1..]),
        Some("serve") => probe::serve(&args[1..]),
        Some("devices") => devices(),
        Some("forget") if args.len() == 2 => forget(&args[1]),
        #[cfg(windows)]
        Some("host") => win::host(&args[1..]),
        #[cfg(windows)]
        Some("agent") => win::agent(&args[1..]),
        Some("clip-agent") => win::clip_agent(&args[1..]),
        Some("broker") => win::broker_cli(&args[1..]),
        #[cfg(windows)]
        Some("debug-keys") => connect::debug_keys(&args[1..]),
        #[cfg(windows)]
        Some("service") => win::service::command(&args[1..]),
        #[cfg(windows)]
        Some("vdisplay") => win::vdisplay::command(&args[1..]),
        #[cfg(windows)]
        Some("nvenc-test") => win::nvenc_test(&args[1..]),
        #[cfg(windows)]
        Some("decode444-test") => win::decode444_test(&args[1..]),
        #[cfg(windows)]
        Some("connect") => connect::connect(&args[1..]),
        #[cfg(windows)]
        Some("list") => win::list(),
        #[cfg(windows)]
        Some("find") => find(),
        #[cfg(windows)]
        Some("results") => results(&args[1..]),
        #[cfg(windows)]
        Some("record") => win::record(&args[1..]),
        Some("--version") => {
            println!("{}", banner());
            Ok(())
        }
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
pub fn win_message(text: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let t: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(t.as_ptr()),
            windows::core::w!("Glass"),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// The Glass window (`Glass.exe`). `--share` starts sharing at once (after
/// "Restart as administrator").
#[cfg(windows)]
pub fn gui_main() {
    crash_log();
    win::register_results();
    let share = std::env::args().any(|a| a == "--share");
    if let Err(e) = win::app::run(share) {
        win_message(&format!("Glass couldn't start: {e}"));
    }
}

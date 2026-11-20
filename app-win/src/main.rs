//! Glass for Windows: host and client roles in one app.
//!
//! Commands:
//!   glass host [opts]                  stream a monitor live to a paired viewer
//!   glass devices                      this PC's identity and paired devices
//!   glass forget NAME|FINGERPRINT      unpair a device
//!   glass probe HOST [opts]            headless viewer: receive and time, no decode
//!   glass serve FILE.glsr              stream a recording as if live (tests)
//!   glass list                         list monitors and GPUs
//!   glass record [opts] out.glsr       capture a monitor and encode HEVC to a recording
//!   glass dump in.glsr out.h265        write the raw Annex-B stream (for ffplay)
//!
//! LATER(M3b): client decode (D3D11VA) -> flip-model present, Raw Input.
//! LATER: tray shell with the device list and pairing UI.

/// Build version: the test-build tag when packaged, else the crate version.
const VERSION: &str = match option_env!("GLASS_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

mod probe;
#[cfg(windows)]
mod win;

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
    let peers = d.peers();
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

/// "Glass (Windows) <version> | core <v> | protocol v<n>"
pub fn banner() -> String {
    format!(
        "Glass (Windows) {} | core {} | protocol v{}",
        VERSION,
        glass_core::VERSION,
        glass_core::protocol::PROTOCOL_VERSION
    )
}

fn usage() -> ! {
    eprintln!(
        "{}

usage:
  glass host [--monitor N] [--chroma 420|444] [--mbps M] [--port P] [--log FILE] [--ask]
  glass devices
  glass forget NAME|FINGERPRINT
  glass probe HOST [--port P] [--pin PIN] [--seconds S] [--verify FILE.glsr] [--min-frames N]
  glass serve FILE.glsr [--port P] [--pin PIN]
  glass list
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("dump") if args.len() == 3 => dump(&args[1], &args[2]),
        Some("pack") => pack(&args[1..]),
        Some("probe") => probe::probe(&args[1..]),
        Some("serve") => probe::serve(&args[1..]),
        Some("devices") => devices(),
        Some("forget") if args.len() == 2 => forget(&args[1]),
        #[cfg(windows)]
        Some("host") => win::host(&args[1..]),
        #[cfg(windows)]
        Some("list") => win::list(),
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

//! Glass for Windows: host and client roles in one app.
//!
//! M1 commands:
//!   glass list                         list monitors and GPUs
//!   glass record [opts] out.glsr       capture a monitor and encode HEVC to a recording
//!   glass dump in.glsr out.h265        write the raw Annex-B stream (for ffplay)
//!
//! LATER(M2): live UDP stream instead of a file.
//! LATER(M3b): client decode (D3D11VA) -> flip-model present, Raw Input.
//! LATER(M6): tray shell, device list, pairing UI.

/// Build version: the test-build tag when packaged, else the crate version.
const VERSION: &str = match option_env!("GLASS_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[cfg(windows)]
mod win;

fn usage() -> ! {
    eprintln!(
        "Glass (Windows) {ver} | core {core} | protocol v{proto}

usage:
  glass list
  glass record [--monitor N] [--seconds S] [--chroma 420|444] [--mbps M] out.glsr
  glass dump in.glsr out.h265
  glass pack in.h265 out.glsr WIDTH HEIGHT FPS 420|444",
        ver = VERSION,
        core = glass_core::VERSION,
        proto = glass_core::protocol::PROTOCOL_VERSION,
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
        #[cfg(windows)]
        Some("list") => win::list(),
        #[cfg(windows)]
        Some("record") => win::record(&args[1..]),
        Some("--version") => {
            println!(
                "Glass (Windows) {} | core {} | protocol v{}",
                VERSION,
                glass_core::VERSION,
                glass_core::protocol::PROTOCOL_VERSION
            );
            Ok(())
        }
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

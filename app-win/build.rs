//! Copies the FFmpeg DLLs next to the built exe so `glass.exe` runs in place.

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }
    let Ok(dir) = std::env::var("FFMPEG_DIR") else {
        return;
    };
    let bin = std::path::Path::new(&dir).join("bin");
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let profile_dir = out.ancestors().nth(3).unwrap();
    let Ok(entries) = std::fs::read_dir(&bin) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")) {
            let _ = std::fs::copy(&p, profile_dir.join(p.file_name().unwrap()));
        }
    }
}

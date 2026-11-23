//! Copies the FFmpeg DLLs next to the built exe so the programs run in place.

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-env-changed=GLASS_VERSION");
    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }
    // Icon and manifest (M3d).
    println!("cargo:rerun-if-changed=glass.rc");
    println!("cargo:rerun-if-changed=glass.manifest");
    println!("cargo:rerun-if-changed=packaging/glass.ico");
    println!("cargo:rerun-if-changed=glass-cli.rc");
    println!("cargo:rerun-if-changed=packaging/glass-cli.ico");
    println!("cargo:rerun-if-env-changed=GLASS_BUILD");
    // Each exe its own icon (Glass.exe the app's, glass-cli.exe with a
    // terminal badge) and version information: product, description,
    // version and build, as Windows shows in Properties > Details. Unsigned
    // programs without it look more suspicious to antivirus (ADR 0008:
    // Defender removed Glass.exe m4-test1 as "severe").
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for (rc, bin, description) in [
        ("glass.rc", "Glass", "Glass remote desktop"),
        (
            "glass-cli.rc",
            "glass-cli",
            "Glass remote desktop: command line and service",
        ),
    ] {
        let path = out.join(rc);
        let _ = std::fs::write(&path, versioned_rc(rc, bin, description));
        let _ = embed_resource::compile_for(&path, [bin], embed_resource::NONE);
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

/// `rc` (from this folder) plus a VERSIONINFO block for `bin`.
fn versioned_rc(rc: &str, bin: &str, description: &str) -> String {
    let here = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // Paths in the original are relative to this folder: make them absolute.
    let base = here.to_string_lossy().replace('\\', "/");
    let original = std::fs::read_to_string(here.join(rc))
        .unwrap_or_default()
        .replace("\"packaging/", &format!("\"{base}/packaging/"))
        .replace("\"glass.manifest\"", &format!("\"{base}/glass.manifest\""));
    let version = env!("CARGO_PKG_VERSION");
    let mut parts: Vec<u16> = version.split('.').filter_map(|p| p.parse().ok()).collect();
    parts.resize(4, 0);
    let numbers = parts
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let build = std::env::var("GLASS_BUILD").unwrap_or_else(|_| "local".into());
    let full = format!("{version} ({build})");
    format!(
        r#"{original}
1 VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "080904b0"
    BEGIN
      VALUE "CompanyName", "Glass"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{full}"
      VALUE "InternalName", "{bin}"
      VALUE "OriginalFilename", "{bin}.exe"
      VALUE "ProductName", "Glass"
      VALUE "ProductVersion", "{full}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x809, 1200
  END
END
"#
    )
}

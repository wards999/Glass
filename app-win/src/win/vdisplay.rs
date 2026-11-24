//! The virtual display on a Windows host (M5, ADR 0019, ADR 0030): the
//! open-source Virtual Display Driver (MIT, signed by SignPath Foundation),
//! shipped in Glass's `vdd` folder and installed from Settings, once, as
//! administrator (`glass-cli vdisplay install`). No control app: Glass
//! creates the device itself, gives the driver a settings folder the user's
//! Glass can write, and (LATER(M5)) sets the screen to the viewer's size
//! and refresh per session through the driver's own pipe, without admin.
//!
//! The driver reads `vdd_settings.xml` from the folder in
//! `HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver\VDDPATH`.

use super::Res;
use std::path::PathBuf;
use windows::core::{w, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW,
    SetupDiSetDeviceRegistryPropertyW, UpdateDriverForPlugAndPlayDevicesW, DICD_GENERATE_ID,
    DIF_REGISTERDEVICE, DIF_REMOVE, DIGCF_PRESENT, GUID_DEVCLASS_DISPLAY, HDEVINFO,
    INSTALLFLAG_FORCE, SPDRP_HARDWAREID, SP_DEVINFO_DATA,
};

/// The driver's hardware id (its INF).
const HWID: &str = "Root\\MttVDD";

/// The bundled driver: `vdd\MttVDD.inf` next to Glass.exe.
fn driver_inf() -> Option<PathBuf> {
    let p = std::env::current_exe()
        .ok()?
        .parent()?
        .join("vdd")
        .join("MttVDD.inf");
    p.exists().then_some(p)
}

/// Where the driver reads its settings, writable by the user's Glass.
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
    base.join("Glass").join("vdd")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The Virtual Display Driver's devices present on this PC.
fn devices() -> Vec<(HDEVINFO, SP_DEVINFO_DATA)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(set) = SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_DISPLAY),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT,
        ) else {
            return out;
        };
        let mut i = 0;
        loop {
            let mut d = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(set, i, &mut d).is_err() {
                break;
            }
            i += 1;
            let mut buf = [0u8; 512];
            if SetupDiGetDeviceRegistryPropertyW(
                set,
                &d,
                SPDRP_HARDWAREID,
                None,
                Some(&mut buf),
                None,
            )
            .is_ok()
            {
                let ids: Vec<u16> = buf
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                let text = String::from_utf16_lossy(&ids);
                if text.split('\0').any(|id| id.eq_ignore_ascii_case(HWID)) {
                    out.push((set, d));
                }
            }
        }
        if out.is_empty() {
            let _ = SetupDiDestroyDeviceInfoList(set);
        }
    }
    out
}

/// The virtual display's device instances as they appear in an adapter's
/// device path (`ROOT#DISPLAY#0001`), to tell its screens apart.
pub fn instance_keys() -> Vec<String> {
    instance_ids()
        .iter()
        .map(|id| id.to_ascii_uppercase().replace('\\', "#"))
        .collect()
}

/// The virtual display's device instance ids (`ROOT\DISPLAY\0001`).
fn instance_ids() -> Vec<String> {
    let devs = devices();
    let mut keys = Vec::new();
    for (set, d) in &devs {
        let mut buf = [0u16; 200];
        let mut need = 0u32;
        unsafe {
            if SetupDiGetDeviceInstanceIdW(*set, d, Some(&mut buf), Some(&mut need)).is_ok() {
                let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                keys.push(String::from_utf16_lossy(&buf[..len]));
            }
        }
    }
    if let Some((set, _)) = devs.first() {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(*set);
        }
    }
    keys
}

/// Adds `w`×`h` (at `hz` and 60 Hz) to the sizes in the driver's settings,
/// newest first, up to 24; no reload (the driver reads them when it
/// starts). The folder is writable by the PC's users.
pub fn note_mode(w: u32, h: u32, hz: u32) -> Res<()> {
    let path = config_dir().join("vdd_settings.xml");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut modes: Vec<(u32, u32, u32)> = vec![(w, h, hz)];
    if hz != 60 {
        modes.push((w, h, 60));
    }
    for line in old.lines().filter(|l| l.contains("<resolution>")) {
        let tag = |t: &str| -> Option<u32> {
            let a = line.find(&format!("<{t}>"))? + t.len() + 2;
            let b = line[a..].find('<')? + a;
            line[a..b].trim().parse().ok()
        };
        if let (Some(a), Some(b), Some(c)) = (tag("width"), tag("height"), tag("refresh_rate")) {
            if !modes.contains(&(a, b, c)) {
                modes.push((a, b, c));
            }
        }
    }
    modes.truncate(24);
    let res: String = modes
        .iter()
        .map(|(a, b, c)| {
            format!("    <resolution><width>{a}</width><height>{b}</height><refresh_rate>{c}</refresh_rate></resolution>\n")
        })
        .collect();
    let xml = settings_xml(&res, &[]);
    if xml != old {
        std::fs::write(&path, xml)?;
    }
    Ok(())
}

/// Glass may restart the driver here (as administrator: the service).
pub fn can_restart() -> bool {
    super::blocker::self_elevated()
}

/// Restarts the driver so it reads its settings (`pnputil /restart-device`),
/// then gives its screen a moment to come back.
pub fn restart_driver() -> Res<()> {
    let ids = instance_ids();
    if ids.is_empty() {
        return Err("the virtual display isn't installed".into());
    }
    for id in &ids {
        run("pnputil", &["/restart-device", id])?;
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));
    Ok(())
}

/// Installed and present.
pub fn installed() -> bool {
    let d = devices();
    if let Some((set, _)) = d.first() {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(*set);
        }
        true
    } else {
        false
    }
}

/// The settings the driver starts with: one screen; Glass sets the
/// viewer's own size and refresh per session.
fn default_settings() -> String {
    let mut res = String::new();
    for (w, h) in [
        (1280, 720),
        (1920, 1080),
        (1920, 1200),
        (2560, 1440),
        (2560, 1600),
        (3840, 2160),
    ] {
        res.push_str(&format!(
            "    <resolution><width>{w}</width><height>{h}</height><refresh_rate>60</refresh_rate></resolution>\n"
        ));
    }
    settings_xml(&res, &[60, 120, 144, 165])
}

/// The driver's settings file: one screen, `res` (`<resolution>` lines),
/// `global` refresh rates added to every resolution.
fn settings_xml(res: &str, global: &[u32]) -> String {
    let g: String = global
        .iter()
        .map(|r| format!("    <g_refresh_rate>{r}</g_refresh_rate>\n"))
        .collect();
    format!(
        "<?xml version='1.0' encoding='utf-8'?>\n<vdd_settings>\n  <monitors><count>1</count></monitors>\n  <gpu><friendlyname>default</friendlyname></gpu>\n  <global>\n{g}  </global>\n  <resolutions>\n{res}  </resolutions>\n  <options>\n    <CustomEdid>false</CustomEdid>\n    <PreventSpoof>false</PreventSpoof>\n    <EdidCeaOverride>false</EdidCeaOverride>\n    <HardwareCursor>true</HardwareCursor>\n    <SDR10bit>false</SDR10bit>\n    <HDRPlus>false</HDRPlus>\n    <logging>false</logging>\n    <debuglogging>false</debuglogging>\n  </options>\n</vdd_settings>\n"
    )
}

fn run(program: &str, args: &[&str]) -> Res<String> {
    let out = std::process::Command::new(program).args(args).output()?;
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("{program} {}: {}", args.join(" "), text.trim()).into())
    }
}

/// `glass-cli vdisplay install`, as administrator.
fn install() -> Res<()> {
    if !super::blocker::self_elevated() {
        return Err("run as administrator (Glass's Settings > Virtual display > Install)".into());
    }
    let inf = driver_inf().ok_or("the driver isn't next to Glass.exe (the vdd folder)")?;
    // 1. The settings folder, writable by the PC's users (Glass sets the
    //    screen size there per session, without admin).
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let xml = dir.join("vdd_settings.xml");
    if !xml.exists() {
        std::fs::write(&xml, default_settings())?;
    }
    let d = dir.to_string_lossy().to_string();
    // *S-1-5-32-545: the Users group, whatever the language of Windows.
    run(
        "icacls",
        &[&d, "/grant", "*S-1-5-32-545:(OI)(CI)M", "/T", "/Q"],
    )?;
    run(
        "reg",
        &[
            "add",
            "HKLM\\SOFTWARE\\MikeTheTech\\VirtualDisplayDriver",
            "/v",
            "VDDPATH",
            "/t",
            "REG_SZ",
            "/d",
            &d,
            "/f",
        ],
    )?;
    println!("Settings folder: {d}");
    // 2. The device (once), then the driver on it. Windows may ask whether
    //    to trust the driver's publisher (SignPath Foundation).
    unsafe {
        if !installed() {
            let set = SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_DISPLAY), None)?;
            let mut d = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            let made = (|| -> Res<()> {
                SetupDiCreateDeviceInfoW(
                    set,
                    w!("Display"),
                    &GUID_DEVCLASS_DISPLAY,
                    PCWSTR::null(),
                    None,
                    DICD_GENERATE_ID,
                    Some(&mut d),
                )?;
                // REG_MULTI_SZ: the id, then an empty string.
                let ids: Vec<u8> = HWID
                    .encode_utf16()
                    .chain([0, 0])
                    .flat_map(|c| c.to_le_bytes())
                    .collect();
                SetupDiSetDeviceRegistryPropertyW(set, &mut d, SPDRP_HARDWAREID, Some(&ids))?;
                SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, Some(&d))?;
                Ok(())
            })();
            let _ = SetupDiDestroyDeviceInfoList(set);
            made.map_err(|e| format!("couldn't create the virtual display device: {e}"))?;
            println!("Created the virtual display device.");
        }
        let inf_w = wide(&inf.to_string_lossy());
        let hwid_w = wide(HWID);
        let mut reboot = windows::core::BOOL(0);
        UpdateDriverForPlugAndPlayDevicesW(
            None,
            PCWSTR(hwid_w.as_ptr()),
            PCWSTR(inf_w.as_ptr()),
            INSTALLFLAG_FORCE,
            Some(&mut reboot),
        )
        .map_err(|e| format!("Windows didn't install the driver: {e}"))?;
        println!(
            "Installed the Virtual Display Driver{}.",
            if reboot.as_bool() {
                " (restart the PC to finish)"
            } else {
                ""
            }
        );
    }
    // Off until a session needs it: Windows turns a new screen on.
    std::thread::sleep(std::time::Duration::from_secs(2));
    match super::vscreen::detach_saved() {
        Ok(()) => println!("The virtual display stays off until someone views this PC."),
        Err(e) => println!("Couldn't switch the virtual display off for now: {e}"),
    }
    Ok(())
}

/// `glass-cli vdisplay uninstall`, as administrator: the device, the driver
/// package and its registry key (the settings folder stays, harmlessly).
fn uninstall() -> Res<()> {
    if !super::blocker::self_elevated() {
        return Err("run as administrator (Glass's Settings > Virtual display > Remove)".into());
    }
    let devs = devices();
    let set = devs.first().map(|(s, _)| *s);
    for (s, d) in &devs {
        unsafe {
            SetupDiCallClassInstaller(DIF_REMOVE, *s, Some(d))
                .map_err(|e| format!("couldn't remove the device: {e}"))?;
        }
    }
    if let Some(s) = set {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(s);
        }
    }
    println!("Removed {} virtual display device(s).", devs.len());
    // The driver package: its published name (oemNN.inf) from pnputil.
    let list = run("pnputil", &["/enum-drivers", "/class", "Display"]).unwrap_or_default();
    let mut published = None;
    for block in list.split("\r\n\r\n").chain(list.split("\n\n")) {
        if block.to_ascii_lowercase().contains("mttvdd.inf") {
            published = block.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                (k.trim() == "Published Name").then(|| v.trim().to_string())
            });
            if published.is_some() {
                break;
            }
        }
    }
    if let Some(p) = published {
        let _ = run("pnputil", &["/delete-driver", &p, "/uninstall", "/force"]);
        println!("Removed the driver package {p}.");
    }
    let _ = run(
        "reg",
        &[
            "delete",
            "HKLM\\SOFTWARE\\MikeTheTech\\VirtualDisplayDriver",
            "/f",
        ],
    );
    Ok(())
}

/// `glass-cli vdisplay install|uninstall|status [--pause]`.
pub fn command(args: &[String]) -> Res<()> {
    let pause = args.iter().any(|a| a == "--pause");
    let r = match args.first().map(String::as_str) {
        Some("install") => install(),
        Some("uninstall") => uninstall(),
        Some("try") => super::vscreen::try_cli(&args[1..]),
        Some("status") => {
            println!(
                "virtual display: {}",
                if installed() {
                    "installed"
                } else {
                    "not installed"
                }
            );
            Ok(())
        }
        _ => Err("usage: glass-cli vdisplay install | uninstall | status".into()),
    };
    if let Err(e) = &r {
        println!("error: {e}");
    }
    if pause {
        // Started from Settings: keep the outcome on screen.
        println!("\nPress Enter to close.");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }
    r
}

/// Runs `glass-cli vdisplay <verb>` as administrator (Windows asks).
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
    let args = wide(&format!("vdisplay {verb} --pause"));
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

/// Bundled with this build (the `vdd` folder next to Glass.exe).
pub fn bundled() -> bool {
    driver_inf().is_some()
}

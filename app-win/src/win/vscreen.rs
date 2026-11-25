//! Using the virtual display during a session (ADR 0030, option 1 of the
//! user's choice): the viewer's screen size and refresh are added to the
//! driver's settings (the driver restarted to read them where Glass may);
//! then the virtual display becomes the only screen on this PC, so the whole
//! desktop moves to it and the PC's own monitor goes dark while viewed. At
//! the end Windows' saved layout comes back.
//!
//! The saved layout never includes the virtual display: `detach_saved`
//! takes it out (after install, and before each session), and the session's
//! own layout is applied without saving, so a crash or restart brings the
//! PC's own screens back.

use super::vdisplay;
use super::Res;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, SetDisplayConfig,
    DISPLAYCONFIG_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ALL_PATHS, QDC_ONLY_ACTIVE_PATHS,
    QUERY_DISPLAY_CONFIG_FLAGS, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_SAVE_TO_DATABASE,
    SDC_USE_DATABASE_CURRENT, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, LUID};

const PATH_ACTIVE: u32 = 0x1;
const NO_MODE: u32 = u32::MAX;

fn query(
    flags: QUERY_DISPLAY_CONFIG_FLAGS,
) -> Option<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> {
    unsafe {
        for _ in 0..3 {
            let (mut np, mut nm) = (0u32, 0u32);
            if GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) != ERROR_SUCCESS {
                return None;
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
            let r = QueryDisplayConfig(
                flags,
                &mut np,
                paths.as_mut_ptr(),
                &mut nm,
                modes.as_mut_ptr(),
                None,
            );
            if r == ERROR_SUCCESS {
                paths.truncate(np as usize);
                modes.truncate(nm as usize);
                return Some((paths, modes));
            }
        }
        None
    }
}

/// The adapter's device path (`\\?\ROOT#DISPLAY#0001#{…}`), upper-case.
fn adapter_path(id: LUID) -> String {
    let mut n = DISPLAYCONFIG_ADAPTER_NAME::default();
    n.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME;
    n.header.size = std::mem::size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32;
    n.header.adapterId = id;
    unsafe {
        if DisplayConfigGetDeviceInfo(&mut n.header) != 0 {
            return String::new();
        }
    }
    let len = n
        .adapterDevicePath
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(128);
    String::from_utf16_lossy(&n.adapterDevicePath[..len]).to_ascii_uppercase()
}

/// Paths whose target is the virtual display's adapter.
fn is_virtual(p: &DISPLAYCONFIG_PATH_INFO, keys: &[String]) -> bool {
    let a = adapter_path(p.targetInfo.adapterId);
    keys.iter().any(|k| a.contains(k.as_str()))
}

/// Takes the virtual display out of Windows' saved layout (the PC's own
/// screens only), and applies that. After install, and before a session.
pub fn detach_saved() -> Res<()> {
    let keys = vdisplay::instance_keys();
    if keys.is_empty() {
        return Ok(());
    }
    let (paths, modes) = query(QDC_ONLY_ACTIVE_PATHS).ok_or("couldn't read the screen layout")?;
    if !paths.iter().any(|p| is_virtual(p, &keys)) {
        return Ok(());
    }
    let kept: Vec<_> = paths
        .iter()
        .filter(|p| !is_virtual(p, &keys))
        .copied()
        .collect();
    if kept.is_empty() {
        return Err("the virtual display is the only screen: leaving it on".into());
    }
    // Only the modes the kept paths use, re-indexed.
    let mut new_modes = Vec::new();
    let mut out = Vec::new();
    for mut p in kept {
        unsafe {
            for idx in [
                &mut p.sourceInfo.Anonymous.modeInfoIdx,
                &mut p.targetInfo.Anonymous.modeInfoIdx,
            ] {
                if *idx != NO_MODE && (*idx as usize) < modes.len() {
                    new_modes.push(modes[*idx as usize]);
                    *idx = (new_modes.len() - 1) as u32;
                } else {
                    *idx = NO_MODE;
                }
            }
        }
        out.push(p);
    }
    let r = unsafe {
        SetDisplayConfig(
            Some(&out),
            Some(&new_modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_SAVE_TO_DATABASE | SDC_ALLOW_CHANGES,
        )
    };
    if r != 0 {
        return Err(
            format!("couldn't take the virtual display out of the layout (error {r})").into(),
        );
    }
    Ok(())
}

/// Back to Windows' saved layout (the PC's own screens).
pub fn restore() {
    unsafe {
        let _ = SetDisplayConfig(None, None, SDC_APPLY | SDC_USE_DATABASE_CURRENT);
    }
}

/// The virtual display's GDI name (`\.\DISPLAYn`) while it's an active
/// screen: found by its own adapter, never by elimination (QUIETPC22: when it
/// dropped out mid-reload, "the only screen left" was the real monitor).
fn vdd_gdi_name(keys: &[String]) -> Option<String> {
    let (paths, _) = query(QDC_ONLY_ACTIVE_PATHS)?;
    for p in paths.iter().filter(|p| is_virtual(p, keys)) {
        let mut n = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        n.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        n.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        n.header.adapterId = p.sourceInfo.adapterId;
        n.header.id = p.sourceInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut n.header) } == 0 {
            let len = n
                .viewGdiDeviceName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(32);
            return Some(String::from_utf16_lossy(&n.viewGdiDeviceName[..len]));
        }
    }
    None
}

/// The virtual display on as the only screen: its target, fed by a source
/// on its own adapter, tried every 250 ms for up to 6 s (it may still be
/// starting).
fn activate(keys: &[String]) -> Res<()> {
    let same = |p: &DISPLAYCONFIG_PATH_INFO| {
        let (s, t) = (p.sourceInfo.adapterId, p.targetInfo.adapterId);
        s.LowPart == t.LowPart && s.HighPart == t.HighPart
    };
    let until = std::time::Instant::now() + std::time::Duration::from_secs(6);
    let mut last = 0;
    let mut seen = false;
    loop {
        let (paths, _) = query(QDC_ALL_PATHS).ok_or("couldn't read the screen layout")?;
        let candidates: Vec<_> = paths
            .iter()
            .filter(|p| is_virtual(p, keys) && p.targetInfo.targetAvailable.as_bool())
            .copied()
            .collect();
        seen |= !candidates.is_empty();
        if let Some(c) = candidates.iter().find(|p| same(p)).or(candidates.first()) {
            let mut p = *c;
            p.flags = PATH_ACTIVE;
            p.sourceInfo.Anonymous.modeInfoIdx = NO_MODE;
            p.targetInfo.Anonymous.modeInfoIdx = NO_MODE;
            last = unsafe {
                SetDisplayConfig(
                    Some(&[p]),
                    None,
                    SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
                )
            };
            if last == 0 && vdd_gdi_name(keys).is_some() {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= until {
            return Err(if seen {
                format!("couldn't switch to the virtual display (error {last})")
            } else {
                "the virtual display isn't available".to_string()
            }
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// The viewer's size on the virtual display (and only on it), at its
/// refresh or else 60 Hz; tried for up to 2 s. False if the driver doesn't
/// offer that size.
fn take_mode(keys: &[String], w: u32, h: u32, hz: u32) -> bool {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(name) = vdd_gdi_name(keys) {
            let took = [hz, 60].iter().any(|&r| {
                super::display::switch(
                    &name,
                    super::display::Mode {
                        width: w,
                        height: h,
                        hz: r,
                    },
                )
            });
            if took {
                return true;
            }
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// The virtual display on as the only screen, for one session; back to the
/// PC's own screens when dropped.
pub struct Session {
    /// Its GDI name (`\.\DISPLAYn`).
    pub name: String,
}

impl Session {
    /// `w`×`h` at `hz`: the size the viewer's screen looks like. The driver
    /// reads its sizes when it starts: a new one is added to its settings,
    /// then the driver is restarted where Glass may (the service, as
    /// administrator); otherwise it's there from the next restart. Never
    /// the driver's own reload command (QUIETPC22: repeated reloads crashed
    /// it, and its screen came and went at unpredictable times).
    pub fn start(w: u32, h: u32, hz: u32, gpu: &super::d3d::Gpu, same_gpu: bool) -> Res<Self> {
        let keys = vdisplay::instance_keys();
        if keys.is_empty() {
            return Err("the virtual display isn't installed".into());
        }
        let _ = detach_saved();
        vdisplay::note_mode(w, h, hz, &gpu.name)?;
        let r = (|| -> Res<Self> {
            activate(&keys)?;
            if !take_mode(&keys, w, h, hz) {
                if !vdisplay::can_restart() {
                    return Err(format!(
                        "it doesn't have {w}x{h} yet; added for when the driver next starts (a PC restart, or a session through the Glass service)"
                    )
                    .into());
                }
                restore();
                vdisplay::restart_driver()?;
                activate(&keys)?;
                if !take_mode(&keys, w, h, hz) {
                    let offered: Vec<String> = vdd_gdi_name(&keys)
                        .map(|n| super::display::modes(&n))
                        .unwrap_or_default()
                        .iter()
                        .map(|m| format!("{}x{}@{}", m.width, m.height, m.hz))
                        .collect();
                    return Err(format!(
                        "it didn't take {w}x{h} after a restart (it offers {})",
                        offered.join(", ")
                    )
                    .into());
                }
            }
            // Desktop Duplication (the service) captures only screens on its
            // own GPU: if the driver drew elsewhere (QUIETPC: another GPU),
            // restart it, now told Glass's GPU by name, and look again.
            if same_gpu {
                let here = |keys: &[String]| {
                    vdd_gdi_name(keys).is_some_and(|n| super::dda::on_gpu(&gpu.device, &n))
                };
                if !here(&keys) {
                    if !vdisplay::can_restart() {
                        return Err(format!("it draws on another GPU than {}", gpu.name).into());
                    }
                    restore();
                    vdisplay::restart_driver()?;
                    activate(&keys)?;
                    if !take_mode(&keys, w, h, hz) || !here(&keys) {
                        return Err(format!(
                            "it still draws on another GPU than {} after a restart",
                            gpu.name
                        )
                        .into());
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            let name = vdd_gdi_name(&keys).ok_or("its screen went away")?;
            Ok(Self { name })
        })();
        if r.is_err() {
            restore();
        }
        r
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        restore();
        let _ = detach_saved();
    }
}

/// The size a viewer's screen looks like: its pixels ÷ its scaling (a
/// Retina Mac at 200 %: 2940×1912 → 1470×956), even numbers; and its
/// refresh (30–240, 60 if unknown).
pub fn viewer_size(w: u16, h: u16, scale_pct: u16, hz: u16) -> Option<(u32, u32, u32)> {
    if w == 0 || h == 0 {
        return None;
    }
    let s = if scale_pct >= 125 {
        scale_pct as u32
    } else {
        100
    };
    let even = |v: u32| (v * 100 / s) & !1;
    let hz = if hz == 0 {
        60
    } else {
        (hz as u32).clamp(30, 240)
    };
    Some((even(w as u32).max(640), even(h as u32).max(480), hz))
}

/// The virtual display for view `mode` (W, H, Hz), always at the viewer's
/// refresh: Fit is the viewer's screen as it looks (its scaling), More
/// space all its pixels, Larger a third bigger than Fit; the host's own
/// screen is this PC's own size and shape (`own`), for games: they keep
/// their 16:9 and the viewer letterboxes it, and at the viewer's refresh
/// each game frame is one refresh there (SPEC §0 B).
pub fn size_for(
    mode: u8,
    v: &glass_core::net::ViewerHello,
    own: (u32, u32),
) -> Option<(u32, u32, u32)> {
    use glass_core::protocol::{MODE_FIT, MODE_FIT_LARGER, MODE_FIT_MORE_SPACE};
    let scale = v.scale_pct.max(100);
    match mode {
        MODE_FIT => viewer_size(v.display_w, v.display_h, v.scale_pct, v.refresh_hz),
        MODE_FIT_MORE_SPACE => viewer_size(v.display_w, v.display_h, 100, v.refresh_hz),
        MODE_FIT_LARGER => viewer_size(
            v.display_w,
            v.display_h,
            (scale as u32 * 4 / 3).min(u16::MAX as u32) as u16,
            v.refresh_hz,
        ),
        _ => {
            let hz = if v.refresh_hz == 0 {
                60
            } else {
                (v.refresh_hz as u32).clamp(30, 240)
            };
            Some((own.0 & !1, own.1 & !1, hz))
        }
    }
}

/// The view mode's name for messages and the results.
pub fn mode_label(mode: u8) -> &'static str {
    use glass_core::protocol::{MODE_FIT, MODE_FIT_LARGER, MODE_FIT_MORE_SPACE};
    match mode {
        MODE_FIT => "Fit",
        MODE_FIT_MORE_SPACE => "More space",
        MODE_FIT_LARGER => "Larger",
        _ => "this PC's own screen (for games)",
    }
}

/// `glass-cli vdisplay try [W H HZ] [--seconds S] [--dda]` (`--dda`: as the
/// service, which needs the virtual display on its own GPU): a session's switch
/// without a viewer, to see what Windows says: the virtual display on as
/// the only screen, both capture methods tried on it, then the PC's own
/// screens back.
pub fn try_cli(args: &[String]) -> Res<()> {
    let num = |i: usize, d: u32| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let (w, h, hz) = (num(0, 1470), num(1, 956), num(2, 60));
    let secs = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3);
    super::d3d::init_process();
    println!("before: {:?}", names());
    for l in super::d3d::describe_adapters() {
        println!("  {l}");
    }
    let t = std::time::Instant::now();
    let gpu = super::d3d::Gpu::new_nvidia()?;
    let dda = args.iter().any(|a| a == "--dda");
    let vs = Session::start(w, h, hz, &gpu, dda)?;
    println!(
        "on in {:.1} s: {} | now: {:?}",
        t.elapsed().as_secs_f64(),
        vs.name,
        names()
    );
    let mon = super::capture::monitors()?
        .into_iter()
        .find(|m| m.name == vs.name)
        .ok_or("its screen isn't listed")?;
    println!(
        "  {}x{} @ {} Hz at ({}, {})",
        mon.width, mon.height, mon.refresh_hz, mon.left, mon.top
    );
    println!("  adapters now (Glass encodes on: {}):", gpu.name);
    for l in super::d3d::describe_adapters() {
        println!("  {l}");
    }
    for (label, dda) in [
        ("Desktop Duplication", true),
        ("Windows.Graphics.Capture", false),
    ] {
        match super::capture::Capture::start(&gpu, &mon, dda) {
            Err(e) => println!("  {label}: start failed: {e}"),
            Ok(mut c) => {
                c.refresh();
                let mut got = None;
                for _ in 0..40 {
                    c.wait(50);
                    match c.next_frame() {
                        Ok(Some(f)) => {
                            got = Some(f.size);
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            println!("  {label}: frame error: {e}");
                            break;
                        }
                    }
                }
                println!("  {label}: size {:?}, first frame {:?}", c.size(), got);
            }
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(secs));
    drop(vs);
    std::thread::sleep(std::time::Duration::from_millis(800));
    println!("after: {:?}", names());
    Ok(())
}

fn names() -> Vec<String> {
    super::capture::monitors()
        .map(|m| {
            m.into_iter()
                .map(|m| format!("{} {}x{}", m.name, m.width, m.height))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retina_mac_looks_like_half() {
        assert_eq!(viewer_size(2940, 1912, 200, 60), Some((1470, 956, 60)));
        assert_eq!(viewer_size(2560, 1440, 100, 165), Some((2560, 1440, 165)));
        assert_eq!(viewer_size(2560, 1600, 0, 0), Some((2560, 1600, 60)));
        assert_eq!(viewer_size(0, 0, 200, 60), None);
    }
}

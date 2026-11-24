//! Using the virtual display during a session (ADR 0030, option 1 of the
//! user's choice): the viewer's screen size and refresh are written into the
//! driver's settings and the driver reloads them (its pipe, no admin); then
//! the virtual display becomes the only screen on this PC, so the whole
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
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, QDC_ALL_PATHS, QDC_ONLY_ACTIVE_PATHS,
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

/// The virtual display on as the only screen, for one session; back to the
/// PC's own screens when dropped.
pub struct Session {
    /// Its GDI name (`\\.\DISPLAYn`).
    pub name: String,
}

impl Session {
    /// `w`×`h` at `hz`: the size the viewer's screen looks like.
    pub fn start(w: u32, h: u32, hz: u32) -> Res<Self> {
        let keys = vdisplay::instance_keys();
        if keys.is_empty() {
            return Err("the virtual display isn't installed".into());
        }
        let before: Vec<String> = super::capture::monitors()?
            .into_iter()
            .map(|m| m.name)
            .collect();
        let _ = detach_saved();
        vdisplay::set_mode(w, h, hz)?;
        let r = (|| -> Res<Self> {
            let (paths, _) = query(QDC_ALL_PATHS).ok_or("couldn't read the screen layout")?;
            let mut p = *paths
                .iter()
                .find(|p| is_virtual(p, &keys) && p.targetInfo.targetAvailable.as_bool())
                .ok_or("the virtual display isn't available")?;
            p.flags = PATH_ACTIVE;
            p.sourceInfo.Anonymous.modeInfoIdx = NO_MODE;
            p.targetInfo.Anonymous.modeInfoIdx = NO_MODE;
            let r = unsafe {
                SetDisplayConfig(
                    Some(&[p]),
                    None,
                    SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
                )
            };
            if r != 0 {
                return Err(format!("couldn't switch to the virtual display (error {r})").into());
            }
            // Its GDI name: the screen that wasn't there before, or the
            // only one left.
            let mut name = None;
            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let now = super::capture::monitors()?;
                name = now
                    .iter()
                    .find(|m| !before.contains(&m.name))
                    .or(if now.len() == 1 { now.first() } else { None })
                    .map(|m| m.name.clone());
                if name.is_some() {
                    break;
                }
            }
            let name = name.ok_or("the virtual display didn't appear")?;
            let mut mode = super::display::Mode {
                width: w,
                height: h,
                hz,
            };
            if !super::display::switch(&name, mode) {
                mode.hz = 60;
                if !super::display::switch(&name, mode) {
                    return Err(format!("the virtual display didn't take {w}x{h}").into());
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
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

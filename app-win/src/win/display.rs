//! Matching the host's resolution to the viewer (M3f). While a viewer is
//! connected, the shared monitor switches to the mode it supports that best
//! fits the viewer's screen (same shape first, then the most pixels, never
//! more than the viewer's), so text is drawn at the size it's shown. The
//! original mode comes back when the session ends (and Windows drops the
//! temporary mode if Glass stops). Only modes the display or driver offers
//! can be used: NVIDIA DSR or a dummy plug add more; a virtual display at
//! exactly the viewer's size is M7.

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_FULLSCREEN, CDS_TYPE, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The modes `device` (e.g. `\\.\DISPLAY1`) offers at 32 bits per pixel.
pub fn modes(device: &str) -> Vec<Mode> {
    let d = wide(device);
    let mut out = Vec::new();
    for i in 0.. {
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !unsafe {
            EnumDisplaySettingsW(PCWSTR(d.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(i), &mut dm)
        }
        .as_bool()
        {
            break;
        }
        if dm.dmBitsPerPel == 32 {
            out.push(Mode {
                width: dm.dmPelsWidth,
                height: dm.dmPelsHeight,
                hz: dm.dmDisplayFrequency,
            });
        }
    }
    out
}

pub fn current(device: &str) -> Option<Mode> {
    let d = wide(device);
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe { EnumDisplaySettingsW(PCWSTR(d.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }
        .as_bool()
        .then_some(Mode {
            width: dm.dmPelsWidth,
            height: dm.dmPelsHeight,
            hz: dm.dmDisplayFrequency,
        })
}

/// The best mode for a viewer screen of `vw` x `vh` among `modes`: never
/// bigger than the viewer in either direction, the viewer's shape if
/// possible (within 3 %), then the most pixels, then the refresh closest to
/// `cur_hz` (not above it). None if nothing beats `cur`.
pub fn best(modes: &[Mode], vw: u32, vh: u32, cur: Mode) -> Option<Mode> {
    if vw == 0 || vh == 0 {
        return None;
    }
    let want = vw as f64 / vh as f64;
    let shape_ok = |m: &Mode| ((m.width as f64 / m.height as f64) / want - 1.0).abs() <= 0.03;
    let fits = |m: &Mode| m.width <= vw && m.height <= vh;
    // The most pixels that fit; the viewer's shape wins only among modes of
    // about the same size (a 10 % bonus), never over clearly more pixels.
    // (m3f-test2 picked 1280x720 over 1280x1024 for a 16:9 viewer.)
    let pick = modes
        .iter()
        .filter(|m| fits(m))
        .max_by_key(|m| {
            let area = m.width as u64 * m.height as u64;
            let score = if shape_ok(m) { area * 11 / 10 } else { area };
            // Prefer the current refresh, then the highest below it.
            let hz = if m.hz <= cur.hz { m.hz } else { 0 };
            (score, hz)
        })
        .copied()?;
    // Only ever switch to more pixels than now.
    let more = pick.width as u64 * pick.height as u64 > cur.width as u64 * cur.height as u64;
    more.then_some(pick)
}

/// Switches `device` to `m` for now (not saved). True if it worked.
pub fn switch(device: &str, m: Mode) -> bool {
    let d = wide(device);
    let dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        dmPelsWidth: m.width,
        dmPelsHeight: m.height,
        dmDisplayFrequency: m.hz,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY,
        ..Default::default()
    };
    unsafe {
        ChangeDisplaySettingsExW(PCWSTR(d.as_ptr()), Some(&dm), None, CDS_FULLSCREEN, None)
            == DISP_CHANGE_SUCCESSFUL
    }
}

/// Back to the mode saved in Windows' settings.
pub fn restore(device: &str) {
    let d = wide(device);
    unsafe {
        let _ = ChangeDisplaySettingsExW(PCWSTR(d.as_ptr()), None, None, CDS_TYPE(0), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(width: u32, height: u32, hz: u32) -> Mode {
        Mode { width, height, hz }
    }

    #[test]
    fn picks_the_viewers_shape_and_most_pixels() {
        let modes = [
            m(1280, 1024, 60),
            m(1920, 1080, 60),
            m(2560, 1440, 60),
            m(2560, 1440, 144),
            m(3840, 2160, 60),
        ];
        // A 2560x1440 viewer: 2560x1440, at the current refresh (60).
        assert_eq!(
            best(&modes, 2560, 1440, m(1280, 1024, 60)),
            Some(m(2560, 1440, 60))
        );
        // Never bigger than the viewer.
        assert_eq!(
            best(&modes, 1920, 1080, m(1280, 1024, 60)),
            Some(m(1920, 1080, 60))
        );
    }

    #[test]
    fn no_change_when_nothing_is_better() {
        // A 1280x1024 monitor (QUIETPC): nothing better to switch to.
        let modes = [
            m(800, 600, 60),
            m(1024, 768, 60),
            m(1280, 720, 60), // the viewer's shape but fewer pixels (m3f-test2)
            m(1280, 1024, 60),
            m(1280, 1024, 75),
        ];
        assert_eq!(best(&modes, 2560, 1440, m(1280, 1024, 60)), None);
        assert_eq!(best(&modes, 0, 0, m(1280, 1024, 60)), None);
    }

    #[test]
    fn uses_dsr_modes_when_offered() {
        // DSR adds 2560x2048 to a 1280x1024 monitor; a Mac viewer 2560x1664.
        let modes = [m(1280, 1024, 60), m(1920, 1536, 60), m(2560, 2048, 60)];
        // No mode fits the viewer's shape: the most pixels that fit.
        assert_eq!(
            best(&modes, 2560, 1664, m(1280, 1024, 60)),
            Some(m(1920, 1536, 60))
        );
    }
}

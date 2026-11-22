//! Input injection with `SendInput`. Building the `INPUT` structs is pure
//! (unit-tested); only [`Injector::inject`] touches the system.
//!
//! - Pointer: absolute, normalised video coordinates -> pixels on the captured
//!   monitor -> `MOUSEEVENTF_VIRTUALDESK` coordinates (multi-monitor safe).
//! - Keys: hardware scan codes (`KEYEVENTF_SCANCODE`), so the PC's own layout
//!   decides the character. Windows doesn't auto-repeat injected keys; the
//!   viewer sends repeats.
//!
//! Windows won't deliver injected input to elevated (admin) windows unless the
//! host runs elevated too; the secure desktop (UAC, lock screen) is out of
//! reach (LATER: SYSTEM service).

use glass_core::input::{
    InputAction, BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT,
};
use glass_core::keymap::hid_to_win_scan;

/// Marks input Glass injected, so a viewer on the same PC never takes it
/// back as typing (a loop) when checks run host and viewer together.
pub const GLASS_MARK: usize = 0x474C_4153; // "GLAS"
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

/// A pixel rectangle (left, top, width, height).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// Maps normalised video coordinates onto the captured monitor.
#[derive(Debug, Clone, Copy)]
pub struct Mapper {
    pub monitor: Rect,
    pub virtual_desk: Rect,
}

impl Mapper {
    pub fn for_monitor(monitor: Rect) -> Self {
        let m = |i| unsafe { GetSystemMetrics(i) };
        Self {
            monitor,
            virtual_desk: Rect {
                left: m(SM_XVIRTUALSCREEN),
                top: m(SM_YVIRTUALSCREEN),
                width: m(SM_CXVIRTUALSCREEN).max(1),
                height: m(SM_CYVIRTUALSCREEN).max(1),
            },
        }
    }

    /// Normalised (0..=65535) -> pixel on the monitor.
    pub fn pixel_of(self, x: u16, y: u16) -> (i32, i32) {
        let px = |n: u16, len: i32| (n as i64 * (len - 1).max(0) as i64 / 65535) as i32;
        (
            self.monitor.left + px(x, self.monitor.width),
            self.monitor.top + px(y, self.monitor.height),
        )
    }

    /// Pixel -> `MOUSEEVENTF_VIRTUALDESK` absolute coordinates (centre of the pixel).
    pub fn absolute_of(self, (px, py): (i32, i32)) -> (i32, i32) {
        let v = self.virtual_desk;
        let a = |p: i32, o: i32, len: i32| (((p - o) as i64 * 65536 + 32768) / len as i64) as i32;
        (a(px, v.left, v.width), a(py, v.top, v.height))
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: GLASS_MARK,
            },
        },
    }
}

fn key(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: GLASS_MARK,
            },
        },
    }
}

/// Builds the `INPUT`s for one action (at most 2: wheel + hwheel).
/// Returns how many of `out` were filled.
pub fn build(a: &InputAction, m: &Mapper, out: &mut [INPUT; 2]) -> usize {
    match *a {
        InputAction::MoveTo { x, y } => {
            let (ax, ay) = m.absolute_of(m.pixel_of(x, y));
            out[0] = mouse(
                ax,
                ay,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            );
            1
        }
        InputAction::Button { button, down } => {
            let (flags, data) = match (button, down) {
                (BUTTON_LEFT, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (BUTTON_LEFT, false) => (MOUSEEVENTF_LEFTUP, 0),
                (BUTTON_RIGHT, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (BUTTON_RIGHT, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (BUTTON_MIDDLE, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (BUTTON_MIDDLE, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (BUTTON_BACK, d) => (
                    if d {
                        MOUSEEVENTF_XDOWN
                    } else {
                        MOUSEEVENTF_XUP
                    },
                    XBUTTON1 as i32,
                ),
                (BUTTON_FORWARD, d) => (
                    if d {
                        MOUSEEVENTF_XDOWN
                    } else {
                        MOUSEEVENTF_XUP
                    },
                    XBUTTON2 as i32,
                ),
                _ => return 0,
            };
            out[0] = mouse(0, 0, data, flags);
            1
        }
        InputAction::Key { hid, down } => {
            let Some((scan, ext)) = hid_to_win_scan(hid) else {
                return 0;
            };
            let mut f = KEYEVENTF_SCANCODE;
            if ext {
                f |= KEYEVENTF_EXTENDEDKEY;
            }
            if !down {
                f |= KEYEVENTF_KEYUP;
            }
            out[0] = key(scan, f);
            1
        }
        InputAction::Scroll { dy, dx } => {
            let mut n = 0;
            if dy != 0 {
                out[n] = mouse(0, 0, dy as i32, MOUSEEVENTF_WHEEL);
                n += 1;
            }
            if dx != 0 {
                out[n] = mouse(0, 0, dx as i32, MOUSEEVENTF_HWHEEL);
                n += 1;
            }
            n
        }
    }
}

/// Injects actions for one session.
pub struct Injector {
    pub mapper: Mapper,
    pub failed: u64,
    /// Last pointer position in screen pixels.
    pub pointer: (i32, i32),
}

impl Injector {
    /// False if Windows refused it (UIPI: an elevated window has focus, or
    /// the secure desktop is up).
    pub fn inject(&mut self, a: &InputAction) -> bool {
        if let InputAction::MoveTo { x, y } = *a {
            self.pointer = self.mapper.pixel_of(x, y);
        }
        let mut buf = [mouse(0, 0, 0, MOUSE_EVENT_FLAGS(0)); 2];
        let n = build(a, &self.mapper, &mut buf);
        if n == 0 {
            return true;
        }
        let sent = unsafe { SendInput(&buf[..n], std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != n {
            self.failed += 1;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper() -> Mapper {
        // Monitor at the right of a 1920-wide primary; desk spans both.
        Mapper {
            monitor: Rect {
                left: 1920,
                top: 0,
                width: 2560,
                height: 1440,
            },
            virtual_desk: Rect {
                left: 0,
                top: 0,
                width: 1920 + 2560,
                height: 1440,
            },
        }
    }

    #[test]
    fn pointer_mapping() {
        let m = mapper();
        assert_eq!(m.pixel_of(0, 0), (1920, 0));
        assert_eq!(m.pixel_of(65535, 65535), (1920 + 2559, 1439));
        assert_eq!(m.pixel_of(32768, 32768), (1920 + 1279, 719));
        // Absolute coords land inside the intended pixel.
        let (ax, _) = m.absolute_of((1920, 0));
        let back = ax as i64 * (1920 + 2560) / 65536;
        assert_eq!(back, 1920);
    }

    #[test]
    fn builds_inputs() {
        let m = mapper();
        let mut out = [mouse(0, 0, 0, MOUSE_EVENT_FLAGS(0)); 2];
        assert_eq!(
            build(
                &InputAction::Key {
                    hid: 0x50,
                    down: false
                },
                &m,
                &mut out
            ),
            1
        );
        let ki = unsafe { out[0].Anonymous.ki };
        assert_eq!(ki.wScan, 0x4B);
        assert_eq!(
            ki.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
        assert_eq!(
            build(&InputAction::Key { hid: 0, down: true }, &m, &mut out),
            0
        );

        assert_eq!(
            build(&InputAction::Scroll { dy: -120, dx: 40 }, &m, &mut out),
            2
        );
        let (w, h) = unsafe { (out[0].Anonymous.mi, out[1].Anonymous.mi) };
        assert_eq!((w.dwFlags, w.mouseData as i32), (MOUSEEVENTF_WHEEL, -120));
        assert_eq!((h.dwFlags, h.mouseData as i32), (MOUSEEVENTF_HWHEEL, 40));

        let b = InputAction::Button {
            button: BUTTON_BACK,
            down: true,
        };
        assert_eq!(build(&b, &m, &mut out), 1);
        let mi = unsafe { out[0].Anonymous.mi };
        assert_eq!(
            (mi.dwFlags, mi.mouseData),
            (MOUSEEVENTF_XDOWN, XBUTTON1 as u32)
        );

        assert_eq!(
            build(&InputAction::MoveTo { x: 0, y: 65535 }, &m, &mut out),
            1
        );
        let mi = unsafe { out[0].Anonymous.mi };
        assert!(mi
            .dwFlags
            .contains(MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_ABSOLUTE));
    }
}

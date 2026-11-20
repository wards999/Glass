//! Physical key mapping. Keys travel as USB HID usage codes (keyboard page
//! 0x07), so the host's keyboard layout decides which character they type.
//!
//! - Mac virtual key code (`kVK_*`) -> HID, with the Ctrl/Cmd rule applied here
//!   on the viewer side so the host only sees plain HID keys.
//! - HID -> Windows set-1 scan code (+ extended flag) for `SendInput`.
//!
//! LATER(M3b): Windows scan code -> HID (Windows viewer), HID -> kVK (Mac host).

/// HID modifier usages.
pub const HID_LCTRL: u8 = 0xE0;
pub const HID_LSHIFT: u8 = 0xE1;
pub const HID_LALT: u8 = 0xE2;
pub const HID_LGUI: u8 = 0xE3;
pub const HID_RCTRL: u8 = 0xE4;
pub const HID_RSHIFT: u8 = 0xE5;
pub const HID_RALT: u8 = 0xE6;
pub const HID_RGUI: u8 = 0xE7;

pub fn is_modifier(hid: u8) -> bool {
    (HID_LCTRL..=HID_RGUI).contains(&hid)
}

/// How Mac modifiers land on a PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ModifierRule {
    /// ⌘ -> Ctrl, ⌃ -> Win, ⌥ -> Alt (Mac shortcuts keep working: ⌘C copies).
    MacStyle = 0,
    /// ⌘ -> Win, ⌃ -> Ctrl, ⌥ -> Alt (keys where a PC keyboard has them).
    PcStyle = 1,
}

/// Mac `kVK_*` -> HID usage, or 0 if the key isn't forwarded (fn, media keys).
/// `iso`: the Mac keyboard is ISO (e.g. UK); Apple reports its top-left key
/// and the key beside left Shift with swapped codes compared to a PC.
pub fn mac_to_hid(kvk: u16, iso: bool, rule: ModifierRule) -> u8 {
    let mac_style = rule == ModifierRule::MacStyle;
    match kvk {
        // Modifiers (rule applies here).
        0x37 => {
            if mac_style {
                HID_LCTRL
            } else {
                HID_LGUI
            }
        } // Command
        0x36 => {
            if mac_style {
                HID_RCTRL
            } else {
                HID_RGUI
            }
        } // Right Command
        0x3B => {
            if mac_style {
                HID_LGUI
            } else {
                HID_LCTRL
            }
        } // Control
        0x3E => {
            if mac_style {
                HID_RGUI
            } else {
                HID_RCTRL
            }
        } // Right Control
        0x38 => HID_LSHIFT,
        0x3C => HID_RSHIFT,
        0x3A => HID_LALT, // Option
        0x3D => HID_RALT,
        0x39 => 0x39, // Caps Lock
        // ISO/ANSI-dependent keys.
        0x0A => {
            if iso {
                0x35
            } else {
                0x64
            }
        } // § (ISO top-left) -> `
        0x32 => {
            if iso {
                0x64
            } else {
                0x35
            }
        } // ` (ISO: beside left Shift)
        _ => MAC_TO_HID
            .iter()
            .find(|(k, _)| *k == kvk)
            .map_or(0, |(_, h)| *h),
    }
}

/// Layout-independent Mac keys (ANSI positions) -> HID.
const MAC_TO_HID: &[(u16, u8)] = &[
    (0x00, 0x04), // A
    (0x0B, 0x05), // B
    (0x08, 0x06), // C
    (0x02, 0x07), // D
    (0x0E, 0x08), // E
    (0x03, 0x09), // F
    (0x05, 0x0A), // G
    (0x04, 0x0B), // H
    (0x22, 0x0C), // I
    (0x26, 0x0D), // J
    (0x28, 0x0E), // K
    (0x25, 0x0F), // L
    (0x2E, 0x10), // M
    (0x2D, 0x11), // N
    (0x1F, 0x12), // O
    (0x23, 0x13), // P
    (0x0C, 0x14), // Q
    (0x0F, 0x15), // R
    (0x01, 0x16), // S
    (0x11, 0x17), // T
    (0x20, 0x18), // U
    (0x09, 0x19), // V
    (0x0D, 0x1A), // W
    (0x07, 0x1B), // X
    (0x10, 0x1C), // Y
    (0x06, 0x1D), // Z
    (0x12, 0x1E), // 1
    (0x13, 0x1F), // 2
    (0x14, 0x20), // 3
    (0x15, 0x21), // 4
    (0x17, 0x22), // 5
    (0x16, 0x23), // 6
    (0x1A, 0x24), // 7
    (0x1C, 0x25), // 8
    (0x19, 0x26), // 9
    (0x1D, 0x27), // 0
    (0x24, 0x28), // Return
    (0x35, 0x29), // Escape
    (0x33, 0x2A), // Delete (Backspace)
    (0x30, 0x2B), // Tab
    (0x31, 0x2C), // Space
    (0x1B, 0x2D), // -
    (0x18, 0x2E), // =
    (0x21, 0x2F), // [
    (0x1E, 0x30), // ]
    (0x2A, 0x31), // \ (ISO: # beside Return; same PC scan code)
    (0x29, 0x33), // ;
    (0x27, 0x34), // '
    (0x2B, 0x36), // ,
    (0x2F, 0x37), // .
    (0x2C, 0x38), // /
    (0x7A, 0x3A), // F1
    (0x78, 0x3B), // F2
    (0x63, 0x3C), // F3
    (0x76, 0x3D), // F4
    (0x60, 0x3E), // F5
    (0x61, 0x3F), // F6
    (0x62, 0x40), // F7
    (0x64, 0x41), // F8
    (0x65, 0x42), // F9
    (0x6D, 0x43), // F10
    (0x67, 0x44), // F11
    (0x6F, 0x45), // F12
    (0x69, 0x68), // F13
    (0x6B, 0x69), // F14
    (0x71, 0x6A), // F15
    (0x6A, 0x6B), // F16
    (0x40, 0x6C), // F17
    (0x4F, 0x6D), // F18
    (0x50, 0x6E), // F19
    (0x5A, 0x6F), // F20
    (0x72, 0x49), // Help -> Insert (same position)
    (0x73, 0x4A), // Home
    (0x74, 0x4B), // Page Up
    (0x75, 0x4C), // Forward Delete
    (0x77, 0x4D), // End
    (0x79, 0x4E), // Page Down
    (0x7C, 0x4F), // Right
    (0x7B, 0x50), // Left
    (0x7D, 0x51), // Down
    (0x7E, 0x52), // Up
    (0x47, 0x53), // Keypad Clear -> Num Lock
    (0x4B, 0x54), // Keypad /
    (0x43, 0x55), // Keypad *
    (0x4E, 0x56), // Keypad -
    (0x45, 0x57), // Keypad +
    (0x4C, 0x58), // Keypad Enter
    (0x53, 0x59), // Keypad 1
    (0x54, 0x5A), // Keypad 2
    (0x55, 0x5B), // Keypad 3
    (0x56, 0x5C), // Keypad 4
    (0x57, 0x5D), // Keypad 5
    (0x58, 0x5E), // Keypad 6
    (0x59, 0x5F), // Keypad 7
    (0x5B, 0x60), // Keypad 8
    (0x5C, 0x61), // Keypad 9
    (0x52, 0x62), // Keypad 0
    (0x41, 0x63), // Keypad .
    (0x51, 0x67), // Keypad =
    (0x6E, 0x65), // Context menu (Application)
    (0x5E, 0x87), // JIS _ (International1)
    (0x5D, 0x89), // JIS ¥ (International3)
    (0x5F, 0x85), // JIS keypad ,
    (0x68, 0x90), // Kana (Lang1)
    (0x66, 0x91), // Eisu (Lang2)
];

/// HID usage -> Windows set-1 scan code and whether it's an E0 (extended) key.
pub fn hid_to_win_scan(hid: u8) -> Option<(u16, bool)> {
    let (scan, ext) = match hid {
        0x04 => (0x1E, false),
        0x05 => (0x30, false),
        0x06 => (0x2E, false),
        0x07 => (0x20, false),
        0x08 => (0x12, false),
        0x09 => (0x21, false),
        0x0A => (0x22, false),
        0x0B => (0x23, false),
        0x0C => (0x17, false),
        0x0D => (0x24, false),
        0x0E => (0x25, false),
        0x0F => (0x26, false),
        0x10 => (0x32, false),
        0x11 => (0x31, false),
        0x12 => (0x18, false),
        0x13 => (0x19, false),
        0x14 => (0x10, false),
        0x15 => (0x13, false),
        0x16 => (0x1F, false),
        0x17 => (0x14, false),
        0x18 => (0x16, false),
        0x19 => (0x2F, false),
        0x1A => (0x11, false),
        0x1B => (0x2D, false),
        0x1C => (0x15, false),
        0x1D => (0x2C, false),
        0x1E..=0x26 => (hid as u16 - 0x1E + 0x02, false), // 1..9
        0x27 => (0x0B, false),                            // 0
        0x28 => (0x1C, false),                            // Enter
        0x29 => (0x01, false),                            // Esc
        0x2A => (0x0E, false),                            // Backspace
        0x2B => (0x0F, false),                            // Tab
        0x2C => (0x39, false),                            // Space
        0x2D => (0x0C, false),
        0x2E => (0x0D, false),
        0x2F => (0x1A, false),
        0x30 => (0x1B, false),
        0x31 | 0x32 => (0x2B, false), // \ and non-US # share a scan code
        0x33 => (0x27, false),
        0x34 => (0x28, false),
        0x35 => (0x29, false),
        0x36 => (0x33, false),
        0x37 => (0x34, false),
        0x38 => (0x35, false),
        0x39 => (0x3A, false),                            // Caps Lock
        0x3A..=0x43 => (hid as u16 - 0x3A + 0x3B, false), // F1..F10
        0x44 => (0x57, false),                            // F11
        0x45 => (0x58, false),                            // F12
        0x46 => (0x37, true),                             // Print Screen
        0x47 => (0x46, false),                            // Scroll Lock
        0x49 => (0x52, true),                             // Insert
        0x4A => (0x47, true),                             // Home
        0x4B => (0x49, true),                             // Page Up
        0x4C => (0x53, true),                             // Delete
        0x4D => (0x4F, true),                             // End
        0x4E => (0x51, true),                             // Page Down
        0x4F => (0x4D, true),                             // Right
        0x50 => (0x4B, true),                             // Left
        0x51 => (0x50, true),                             // Down
        0x52 => (0x48, true),                             // Up
        0x53 => (0x45, false),                            // Num Lock
        0x54 => (0x35, true),                             // Keypad /
        0x55 => (0x37, false),                            // Keypad *
        0x56 => (0x4A, false),
        0x57 => (0x4E, false),
        0x58 => (0x1C, true), // Keypad Enter
        0x59 => (0x4F, false),
        0x5A => (0x50, false),
        0x5B => (0x51, false),
        0x5C => (0x4B, false),
        0x5D => (0x4C, false),
        0x5E => (0x4D, false),
        0x5F => (0x47, false),
        0x60 => (0x48, false),
        0x61 => (0x49, false),
        0x62 => (0x52, false),
        0x63 => (0x53, false),
        0x64 => (0x56, false), // non-US \ (beside left Shift on ISO)
        0x65 => (0x5D, true),  // Application
        0x67 => (0x59, false), // Keypad =
        0x68..=0x6E => (hid as u16 - 0x68 + 0x64, false), // F13..F19
        0x6F => (0x6B, false), // F20
        0x85 => (0x7E, false), // Keypad , (Brazil/JIS)
        0x87 => (0x73, false), // International1 (Ro)
        0x89 => (0x7D, false), // International3 (Yen)
        0x90 => (0x72, false), // Lang1
        0x91 => (0x71, false), // Lang2
        HID_LCTRL => (0x1D, false),
        HID_LSHIFT => (0x2A, false),
        HID_LALT => (0x38, false),
        HID_LGUI => (0x5B, true),
        HID_RCTRL => (0x1D, true),
        HID_RSHIFT => (0x36, false),
        HID_RALT => (0x38, true),
        HID_RGUI => (0x5C, true),
        _ => return None,
    };
    Some((scan, ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_edit_keys() {
        let r = ModifierRule::MacStyle;
        assert_eq!(mac_to_hid(0x00, false, r), 0x04); // A
        assert_eq!(mac_to_hid(0x06, false, r), 0x1D); // Z
        assert_eq!(mac_to_hid(0x1D, false, r), 0x27); // 0
        assert_eq!(mac_to_hid(0x33, false, r), 0x2A); // Delete -> Backspace
        assert_eq!(mac_to_hid(0x75, false, r), 0x4C); // Fwd Delete -> Delete
        assert_eq!(mac_to_hid(0x3F, false, r), 0); // fn: not forwarded
        assert_eq!(hid_to_win_scan(0x04), Some((0x1E, false))); // A
        assert_eq!(hid_to_win_scan(0x1E), Some((0x02, false))); // 1
        assert_eq!(hid_to_win_scan(0x26), Some((0x0A, false))); // 9
        assert_eq!(hid_to_win_scan(0x3A), Some((0x3B, false))); // F1
        assert_eq!(hid_to_win_scan(0x43), Some((0x44, false))); // F10
        assert_eq!(hid_to_win_scan(0x50), Some((0x4B, true))); // Left
        assert_eq!(hid_to_win_scan(0x00), None);
    }

    #[test]
    fn modifier_rules() {
        let (m, p) = (ModifierRule::MacStyle, ModifierRule::PcStyle);
        assert_eq!(mac_to_hid(0x37, false, m), HID_LCTRL); // ⌘ -> Ctrl
        assert_eq!(mac_to_hid(0x3B, false, m), HID_LGUI); // ⌃ -> Win
        assert_eq!(mac_to_hid(0x37, false, p), HID_LGUI);
        assert_eq!(mac_to_hid(0x3B, false, p), HID_LCTRL);
        assert_eq!(mac_to_hid(0x3A, false, m), HID_LALT); // ⌥ -> Alt
        assert!(is_modifier(HID_RGUI) && !is_modifier(0x39));
        assert_eq!(hid_to_win_scan(HID_RCTRL), Some((0x1D, true)));
        assert_eq!(hid_to_win_scan(HID_LGUI), Some((0x5B, true)));
    }

    #[test]
    fn iso_keys_map_by_position() {
        let r = ModifierRule::MacStyle;
        // UK Mac: § top-left behaves like a PC's ` key, ` beside Shift like \.
        assert_eq!(mac_to_hid(0x0A, true, r), 0x35);
        assert_eq!(mac_to_hid(0x32, true, r), 0x64);
        assert_eq!(mac_to_hid(0x32, false, r), 0x35); // ANSI: ` is top-left
        assert_eq!(hid_to_win_scan(0x64), Some((0x56, false)));
    }

    #[test]
    fn every_mapped_mac_key_has_a_scan_code() {
        for iso in [false, true] {
            for kvk in 0..0x80u16 {
                let h = mac_to_hid(kvk, iso, ModifierRule::MacStyle);
                if h != 0 {
                    assert!(hid_to_win_scan(h).is_some(), "kVK {kvk:#x} -> HID {h:#x}");
                }
            }
        }
        // No two Mac keys map to the same HID key (except deliberate ISO swaps).
        let mut seen = std::collections::HashMap::new();
        for kvk in 0..0x80u16 {
            let h = mac_to_hid(kvk, false, ModifierRule::MacStyle);
            if h != 0 {
                if let Some(prev) = seen.insert(h, kvk) {
                    panic!("kVK {prev:#x} and {kvk:#x} both map to HID {h:#x}");
                }
            }
        }
    }
}

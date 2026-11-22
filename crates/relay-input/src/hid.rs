//! USB HID keyboard usages (page 0x07) and their native equivalents.
//!
//! The wire format carries physical keys as HID usages (FR-14). Each Host
//! backend maps them to its native code: macOS virtual keycodes (`kVK_*`)
//! and Windows set-1 scancodes (`KEYEVENTF_SCANCODE`, with the E0 prefix
//! flag for extended keys).

/// HID usage names used across the crate.
pub mod usage {
    pub const A: u16 = 0x04;
    pub const Z: u16 = 0x1D;
    pub const DIGIT1: u16 = 0x1E;
    pub const DIGIT0: u16 = 0x27;
    pub const ENTER: u16 = 0x28;
    pub const ESCAPE: u16 = 0x29;
    pub const BACKSPACE: u16 = 0x2A;
    pub const TAB: u16 = 0x2B;
    pub const SPACE: u16 = 0x2C;
    pub const MINUS: u16 = 0x2D;
    pub const EQUAL: u16 = 0x2E;
    pub const BRACKET_LEFT: u16 = 0x2F;
    pub const BRACKET_RIGHT: u16 = 0x30;
    pub const BACKSLASH: u16 = 0x31;
    pub const NON_US_HASH: u16 = 0x32;
    pub const SEMICOLON: u16 = 0x33;
    pub const QUOTE: u16 = 0x34;
    pub const BACKQUOTE: u16 = 0x35;
    pub const COMMA: u16 = 0x36;
    pub const PERIOD: u16 = 0x37;
    pub const SLASH: u16 = 0x38;
    pub const CAPS_LOCK: u16 = 0x39;
    pub const F1: u16 = 0x3A;
    pub const F12: u16 = 0x45;
    pub const PRINT_SCREEN: u16 = 0x46;
    pub const SCROLL_LOCK: u16 = 0x47;
    pub const PAUSE: u16 = 0x48;
    pub const INSERT: u16 = 0x49;
    pub const HOME: u16 = 0x4A;
    pub const PAGE_UP: u16 = 0x4B;
    pub const DELETE: u16 = 0x4C;
    pub const END: u16 = 0x4D;
    pub const PAGE_DOWN: u16 = 0x4E;
    pub const ARROW_RIGHT: u16 = 0x4F;
    pub const ARROW_LEFT: u16 = 0x50;
    pub const ARROW_DOWN: u16 = 0x51;
    pub const ARROW_UP: u16 = 0x52;
    pub const NUM_LOCK: u16 = 0x53;
    pub const NUMPAD_DIVIDE: u16 = 0x54;
    pub const NUMPAD_MULTIPLY: u16 = 0x55;
    pub const NUMPAD_SUBTRACT: u16 = 0x56;
    pub const NUMPAD_ADD: u16 = 0x57;
    pub const NUMPAD_ENTER: u16 = 0x58;
    pub const NUMPAD1: u16 = 0x59;
    pub const NUMPAD9: u16 = 0x61;
    pub const NUMPAD0: u16 = 0x62;
    pub const NUMPAD_DECIMAL: u16 = 0x63;
    pub const INTL_BACKSLASH: u16 = 0x64;
    pub const CONTEXT_MENU: u16 = 0x65;
    pub const POWER: u16 = 0x66;
    pub const NUMPAD_EQUAL: u16 = 0x67;
    pub const F13: u16 = 0x68;
    pub const F24: u16 = 0x73;
    pub const MUTE: u16 = 0x7F;
    pub const VOLUME_UP: u16 = 0x80;
    pub const VOLUME_DOWN: u16 = 0x81;
    pub const INTL_RO: u16 = 0x87;
    pub const KANA_MODE: u16 = 0x88;
    pub const INTL_YEN: u16 = 0x89;
    pub const CONVERT: u16 = 0x8A;
    pub const NON_CONVERT: u16 = 0x8B;
    pub const LANG1: u16 = 0x90;
    pub const LANG2: u16 = 0x91;
    pub use crate::keymap::{
        LEFT_ALT, LEFT_CTRL, LEFT_META, LEFT_SHIFT, RIGHT_ALT, RIGHT_CTRL, RIGHT_META, RIGHT_SHIFT,
    };
}

use usage::*;

/// macOS virtual keycode (`kVK_*`, Carbon `Events.h`).
pub fn to_mac_keycode(u: u16) -> Option<u16> {
    // Letters A..Z in HID order.
    const LETTERS: [u16; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F,
        0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06,
    ];
    // Digits 1..9, 0 in HID order.
    const DIGITS: [u16; 10] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D];
    // F1..F12.
    const FKEYS: [u16; 12] = [
        0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
    ];
    // F13..F20 (macOS has no F21..F24 keycodes).
    const FKEYS_HI: [u16; 8] = [0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A];
    // Numpad 1..9, 0.
    const NUMPAD: [u16; 10] = [0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5B, 0x5C, 0x52];
    Some(match u {
        A..=Z => LETTERS[(u - A) as usize],
        DIGIT1..=DIGIT0 => DIGITS[(u - DIGIT1) as usize],
        F1..=F12 => FKEYS[(u - F1) as usize],
        F13..=0x6F => FKEYS_HI[(u - F13) as usize],
        NUMPAD1..=NUMPAD0 => NUMPAD[(u - NUMPAD1) as usize],
        ENTER => 0x24,
        ESCAPE => 0x35,
        BACKSPACE => 0x33,
        TAB => 0x30,
        SPACE => 0x31,
        MINUS => 0x1B,
        EQUAL => 0x18,
        BRACKET_LEFT => 0x21,
        BRACKET_RIGHT => 0x1E,
        BACKSLASH | NON_US_HASH => 0x2A,
        SEMICOLON => 0x29,
        QUOTE => 0x27,
        BACKQUOTE => 0x32,
        COMMA => 0x2B,
        PERIOD => 0x2F,
        SLASH => 0x2C,
        CAPS_LOCK => 0x39,
        INSERT => 0x72, // Help
        HOME => 0x73,
        PAGE_UP => 0x74,
        DELETE => 0x75,
        END => 0x77,
        PAGE_DOWN => 0x79,
        ARROW_RIGHT => 0x7C,
        ARROW_LEFT => 0x7B,
        ARROW_DOWN => 0x7D,
        ARROW_UP => 0x7E,
        NUM_LOCK => 0x47, // Keypad Clear
        NUMPAD_DIVIDE => 0x4B,
        NUMPAD_MULTIPLY => 0x43,
        NUMPAD_SUBTRACT => 0x4E,
        NUMPAD_ADD => 0x45,
        NUMPAD_ENTER => 0x4C,
        NUMPAD_DECIMAL => 0x41,
        NUMPAD_EQUAL => 0x51,
        INTL_BACKSLASH => 0x0A, // ISO section
        CONTEXT_MENU => 0x6E,
        MUTE => 0x4A,
        VOLUME_UP => 0x48,
        VOLUME_DOWN => 0x49,
        INTL_RO => 0x5E,
        INTL_YEN => 0x5D,
        LANG1 | KANA_MODE => 0x68,
        LANG2 => 0x66,
        LEFT_CTRL => 0x3B,
        LEFT_SHIFT => 0x38,
        LEFT_ALT => 0x3A,
        LEFT_META => 0x37,
        RIGHT_CTRL => 0x3E,
        RIGHT_SHIFT => 0x3C,
        RIGHT_ALT => 0x3D,
        RIGHT_META => 0x36,
        _ => return None,
    })
}

/// Windows set-1 scancode and whether it needs the E0 (extended) prefix.
/// Pause has no single scancode and is sent as a virtual key instead.
pub fn to_win_scancode(u: u16) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ];
    const FKEYS_HI: [u16; 12] = [
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x76,
    ];
    const NUMPAD: [u16; 10] = [0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49, 0x52];
    let n = |s: u16| Some((s, false));
    let e = |s: u16| Some((s, true));
    match u {
        A..=Z => n(LETTERS[(u - A) as usize]),
        DIGIT1..=DIGIT0 => n(0x02 + (u - DIGIT1)),
        F1..=0x43 => n(0x3B + (u - F1)), // F1..F10
        0x44 => n(0x57),                 // F11
        F12 => n(0x58),
        F13..=F24 => n(FKEYS_HI[(u - F13) as usize]),
        NUMPAD1..=NUMPAD0 => n(NUMPAD[(u - NUMPAD1) as usize]),
        ENTER => n(0x1C),
        ESCAPE => n(0x01),
        BACKSPACE => n(0x0E),
        TAB => n(0x0F),
        SPACE => n(0x39),
        MINUS => n(0x0C),
        EQUAL => n(0x0D),
        BRACKET_LEFT => n(0x1A),
        BRACKET_RIGHT => n(0x1B),
        BACKSLASH | NON_US_HASH => n(0x2B),
        SEMICOLON => n(0x27),
        QUOTE => n(0x28),
        BACKQUOTE => n(0x29),
        COMMA => n(0x33),
        PERIOD => n(0x34),
        SLASH => n(0x35),
        CAPS_LOCK => n(0x3A),
        SCROLL_LOCK => n(0x46),
        NUM_LOCK => n(0x45),
        NUMPAD_MULTIPLY => n(0x37),
        NUMPAD_SUBTRACT => n(0x4A),
        NUMPAD_ADD => n(0x4E),
        NUMPAD_DECIMAL => n(0x53),
        NUMPAD_EQUAL => n(0x59),
        INTL_BACKSLASH => n(0x56),
        INTL_RO => n(0x73),
        INTL_YEN => n(0x7D),
        KANA_MODE => n(0x70),
        CONVERT => n(0x79),
        NON_CONVERT => n(0x7B),
        LANG1 => n(0x72),
        LANG2 => n(0x71),
        LEFT_CTRL => n(0x1D),
        LEFT_SHIFT => n(0x2A),
        LEFT_ALT => n(0x38),
        RIGHT_SHIFT => n(0x36),
        NUMPAD_ENTER => e(0x1C),
        RIGHT_CTRL => e(0x1D),
        NUMPAD_DIVIDE => e(0x35),
        PRINT_SCREEN => e(0x37),
        RIGHT_ALT => e(0x38),
        HOME => e(0x47),
        ARROW_UP => e(0x48),
        PAGE_UP => e(0x49),
        ARROW_LEFT => e(0x4B),
        ARROW_RIGHT => e(0x4D),
        END => e(0x4F),
        ARROW_DOWN => e(0x50),
        PAGE_DOWN => e(0x51),
        INSERT => e(0x52),
        DELETE => e(0x53),
        LEFT_META => e(0x5B),
        RIGHT_META => e(0x5C),
        CONTEXT_MENU => e(0x5D),
        POWER => e(0x5E),
        MUTE => e(0x20),
        VOLUME_DOWN => e(0x2E),
        VOLUME_UP => e(0x30),
        _ => None,
    }
}

/// Whether `u` is a modifier key, and its [`crate::Modifiers`] bit.
pub fn modifier_bit(u: u16) -> Option<u32> {
    use crate::Modifiers as M;
    Some(match u {
        LEFT_CTRL | RIGHT_CTRL => M::CTRL,
        LEFT_SHIFT | RIGHT_SHIFT => M::SHIFT,
        LEFT_ALT | RIGHT_ALT => M::ALT,
        LEFT_META | RIGHT_META => M::META,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_checks() {
        // Q W E R T Y on mac: 0x0C 0x0D 0x0E 0x0F 0x11 0x10.
        let qwerty = [0x14u16, 0x1A, 0x08, 0x15, 0x17, 0x1C];
        let mac: Vec<_> = qwerty.iter().map(|&u| to_mac_keycode(u).unwrap()).collect();
        assert_eq!(mac, [0x0C, 0x0D, 0x0E, 0x0F, 0x11, 0x10]);
        let win: Vec<_> = qwerty
            .iter()
            .map(|&u| to_win_scancode(u).unwrap().0)
            .collect();
        assert_eq!(win, [0x10, 0x11, 0x12, 0x13, 0x14, 0x15]);
        assert_eq!(to_win_scancode(0x27), Some((0x0B, false))); // 0
        assert_eq!(to_win_scancode(0x43), Some((0x44, false))); // F10
        assert_eq!(to_win_scancode(ARROW_UP), Some((0x48, true)));
        assert_eq!(to_mac_keycode(ARROW_UP), Some(0x7E));
        assert_eq!(to_mac_keycode(LEFT_META), Some(0x37));
        assert_eq!(to_mac_keycode(0x6F), Some(0x5A)); // F20
        assert_eq!(to_mac_keycode(0x70), None); // F21: no mac key
    }

    #[test]
    fn every_common_key_maps_on_both() {
        let common = (A..=Z)
            .chain(DIGIT1..=DIGIT0)
            .chain(ENTER..=CAPS_LOCK)
            .chain(F1..=F12)
            .chain(INSERT..=ARROW_UP)
            .chain(NUMPAD_DIVIDE..=NUMPAD_DECIMAL)
            .chain(LEFT_CTRL..=RIGHT_META);
        for u in common {
            assert!(to_mac_keycode(u).is_some(), "mac missing {u:#x}");
            assert!(to_win_scancode(u).is_some(), "win missing {u:#x}");
        }
    }

    #[test]
    fn mac_codes_unique() {
        let mut seen = std::collections::HashMap::new();
        for u in 0u16..0xE8 {
            if matches!(u, NON_US_HASH | KANA_MODE) {
                continue; // deliberate aliases
            }
            if let Some(k) = to_mac_keycode(u) {
                assert!(
                    seen.insert(k, u).is_none(),
                    "{u:#x} and {:#x} share {k:#x}",
                    seen[&k]
                );
            }
        }
    }
}

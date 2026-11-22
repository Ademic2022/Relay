//! USB HID usage constants and ⌘ ↔ Ctrl mapping (FR-15).

use crate::{InputEvent, Modifiers, Os};

pub const LEFT_CTRL: u16 = 0xE0;
pub const LEFT_SHIFT: u16 = 0xE1;
pub const LEFT_ALT: u16 = 0xE2;
pub const LEFT_META: u16 = 0xE3;
pub const RIGHT_CTRL: u16 = 0xE4;
pub const RIGHT_SHIFT: u16 = 0xE5;
pub const RIGHT_ALT: u16 = 0xE6;
pub const RIGHT_META: u16 = 0xE7;
pub const DELETE: u16 = 0x4C;

/// Swap ⌘ and Ctrl when Controller and Host run different OSes, so a Mac
/// user's ⌘C copies on Windows and a Windows user's Ctrl+C copies on Mac.
#[derive(Debug, Clone, Copy)]
pub struct ModifierMapper {
    swap: bool,
}

impl ModifierMapper {
    pub fn new(controller: Os, host: Os, enabled: bool) -> Self {
        Self {
            swap: enabled && controller != host,
        }
    }

    pub fn apply(&self, ev: InputEvent) -> InputEvent {
        if !self.swap {
            return ev;
        }
        match ev {
            InputEvent::Key {
                usage,
                down,
                modifiers,
            } => InputEvent::Key {
                usage: swap_usage(usage),
                down,
                modifiers: swap_mods(modifiers),
            },
            other => other,
        }
    }
}

fn swap_usage(u: u16) -> u16 {
    match u {
        LEFT_CTRL => LEFT_META,
        LEFT_META => LEFT_CTRL,
        RIGHT_CTRL => RIGHT_META,
        RIGHT_META => RIGHT_CTRL,
        other => other,
    }
}

fn swap_mods(m: Modifiers) -> Modifiers {
    let mut out = m.0 & !(Modifiers::CTRL | Modifiers::META);
    if m.contains(Modifiers::CTRL) {
        out |= Modifiers::META;
    }
    if m.contains(Modifiers::META) {
        out |= Modifiers::CTRL;
    }
    Modifiers(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(usage: u16, mods: u32) -> InputEvent {
        InputEvent::Key {
            usage,
            down: true,
            modifiers: Modifiers(mods),
        }
    }

    #[test]
    fn mac_controls_windows() {
        let m = ModifierMapper::new(Os::MacOs, Os::Windows, true);
        assert_eq!(
            m.apply(key(LEFT_META, Modifiers::META | Modifiers::SHIFT)),
            key(LEFT_CTRL, Modifiers::CTRL | Modifiers::SHIFT)
        );
        assert_eq!(m.apply(key(0x06, 0)), key(0x06, 0), "C is untouched");
    }

    #[test]
    fn same_os_or_disabled_is_identity() {
        for m in [
            ModifierMapper::new(Os::MacOs, Os::MacOs, true),
            ModifierMapper::new(Os::MacOs, Os::Windows, false),
        ] {
            assert_eq!(
                m.apply(key(LEFT_META, Modifiers::META)),
                key(LEFT_META, Modifiers::META)
            );
        }
    }
}

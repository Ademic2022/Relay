//! Input model shared by Controller capture and Host injection
//! (FR-13 to FR-16).
//!
//! Keys travel as USB HID usages (page 0x07): a physical-key code that is
//! the same on every OS, so layouts behave correctly (FR-14). Each backend
//! maps HID usage to its native scancode / virtual keycode.

pub mod hid;
pub mod hotkey;
pub mod keymap;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod mapping;
pub mod mock;
#[cfg(target_os = "windows")]
pub mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Modifier bitmask, carried with every key event so the Host can resync
/// stuck modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers(pub u32);

impl Modifiers {
    pub const SHIFT: u32 = 1 << 0;
    pub const CTRL: u32 = 1 << 1;
    pub const ALT: u32 = 1 << 2;
    /// ⌘ on macOS, Windows key on Windows.
    pub const META: u32 = 1 << 3;

    pub fn contains(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// Normalised 0..1 position within the selected Host display.
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Logical-pixel deltas; `precise` = trackpad (FR-13).
    Scroll {
        dx: f32,
        dy: f32,
        precise: bool,
    },
    Key {
        usage: u16,
        down: bool,
        modifiers: Modifiers,
    },
    /// "Translate characters" mode (FR-14).
    Text(String),
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("accessibility permission not granted")]
    PermissionDenied,
    #[error("key {0:#x} has no mapping on this OS")]
    UnmappedKey(u16),
    #[error("not supported on this platform/build")]
    Unsupported,
    #[error("platform error: {0}")]
    Platform(String),
}

/// The Host display that normalised pointer coordinates refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayTarget {
    /// Platform display ID (CGDirectDisplayID / capturer output ID).
    pub id: u32,
    /// Virtual-desktop rectangle in physical pixels (Windows) or points
    /// (macOS; backends re-query the OS where it knows better).
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Scroll deltas on the wire are logical pixels with the macOS sign
/// convention: positive `dy` scrolls toward the top of the content,
/// positive `dx` toward the left. Wheel notches are sent as this many px.
pub const PX_PER_WHEEL_NOTCH: f32 = 40.0;

/// Host-side injector. Called on a dedicated real-time thread (§6 rule 8).
pub trait InputInjector: Send {
    /// Display that subsequent `MouseMove` coordinates map onto.
    fn set_display(&mut self, target: DisplayTarget);
    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError>;
    /// Release every key and button held by the remote side. Called on
    /// disconnect so nothing stays stuck.
    fn release_all(&mut self) -> Result<(), InputError>;
}

/// The injector for this OS.
pub fn platform_injector() -> Result<Box<dyn InputInjector>, InputError> {
    #[cfg(target_os = "macos")]
    return macos::CgEventInjector::new().map(|i| Box::new(i) as _);
    #[cfg(target_os = "windows")]
    return Ok(Box::new(windows::SendInputInjector::new()));
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Err(InputError::Unsupported)
}

/// Tracks clicks to produce double/triple-click counts (macOS needs them
/// explicitly; Windows derives them itself).
#[derive(Debug, Default)]
pub struct ClickCounter {
    last: Option<(std::time::Instant, MouseButton, (f64, f64))>,
    count: i64,
}

impl ClickCounter {
    pub const INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    pub const SLOP: f64 = 4.0;

    /// Register a button-down at `pos`; returns the click count (1, 2, 3...).
    pub fn down(&mut self, now: std::time::Instant, b: MouseButton, pos: (f64, f64)) -> i64 {
        self.count = match self.last {
            Some((t, lb, (x, y)))
                if lb == b
                    && now.saturating_duration_since(t) <= Self::INTERVAL
                    && (x - pos.0).abs() <= Self::SLOP
                    && (y - pos.1).abs() <= Self::SLOP =>
            {
                self.count + 1
            }
            _ => 1,
        };
        self.last = Some((now, b, pos));
        self.count
    }

    /// Count for the matching button-up.
    pub fn current(&self) -> i64 {
        self.count.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn click_counting() {
        let t = Instant::now();
        let mut c = ClickCounter::default();
        assert_eq!(c.down(t, MouseButton::Left, (10.0, 10.0)), 1);
        assert_eq!(
            c.down(
                t + Duration::from_millis(200),
                MouseButton::Left,
                (11.0, 10.0)
            ),
            2
        );
        assert_eq!(
            c.down(
                t + Duration::from_millis(400),
                MouseButton::Left,
                (11.0, 12.0)
            ),
            3
        );
        assert_eq!(
            c.down(
                t + Duration::from_millis(1200),
                MouseButton::Left,
                (11.0, 12.0)
            ),
            1
        );
        assert_eq!(
            c.down(
                t + Duration::from_millis(1300),
                MouseButton::Right,
                (11.0, 12.0)
            ),
            1
        );
        assert_eq!(
            c.down(
                t + Duration::from_millis(1400),
                MouseButton::Right,
                (40.0, 12.0)
            ),
            1
        );
    }
}

//! Input model shared by Controller capture and Host injection
//! (FR-13 to FR-16).
//!
//! Keys travel as USB HID usages (page 0x07): a physical-key code that is
//! the same on every OS, so layouts behave correctly (FR-14). Each backend
//! maps HID usage to its native scancode / virtual keycode.

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

/// Host-side injector. Called on a dedicated real-time thread (§6 rule 8).
pub trait InputInjector: Send {
    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError>;
    /// Release every key and button held by the remote side. Called on
    /// disconnect so nothing stays stuck.
    fn release_all(&mut self) -> Result<(), InputError>;
}

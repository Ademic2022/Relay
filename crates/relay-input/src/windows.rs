//! Windows injection via `SendInput` (FR-13, FR-14, §10).
//!
//! - Keys are sent as scancodes (`KEYEVENTF_SCANCODE`, E0 prefix via
//!   `KEYEVENTF_EXTENDEDKEY`) so the Host's own layout applies.
//! - Pointer positions are absolute over the virtual desktop
//!   (`MOUSEEVENTF_VIRTUALDESK`), in physical pixels: the process is made
//!   per-monitor DPI aware.
//! - Wheel deltas are converted to `WHEEL_DELTA` units with remainders
//!   carried over, so trackpad scrolling stays smooth.
//! - For the secure desktop (UAC, login), the Agent must run on that
//!   desktop as SYSTEM (M7).

#![allow(unsafe_code)]

use std::collections::HashSet;

use ::windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use ::windows::Win32::UI::Input::KeyboardAndMouse::*;
use ::windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::hid::{self, usage};
use crate::*;

const WHEEL_DELTA: f32 = 120.0;
const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

#[derive(Debug)]
pub struct SendInputInjector {
    target: DisplayTarget,
    pos: (i32, i32),
    buttons: Vec<MouseButton>,
    keys: HashSet<u16>,
    wheel_rem: (f32, f32),
}

impl Default for SendInputInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl SendInputInjector {
    pub fn new() -> Self {
        // SAFETY: process-wide setter; harmless if already set.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        Self {
            target: DisplayTarget {
                id: 0,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            pos: (0, 0),
            buttons: Vec::new(),
            keys: HashSet::new(),
            wheel_rem: (0.0, 0.0),
        }
    }

    fn send(inputs: &[INPUT]) -> Result<(), InputError> {
        // SAFETY: slice of fully initialised INPUT structs.
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            // Typically UIPI: the foreground window belongs to a more
            // privileged process (e.g. an elevated app or UAC).
            Err(InputError::Platform(
                "SendInput blocked (UIPI / secure desktop?)".into(),
            ))
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
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn key_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn key(&mut self, u: u16, down: bool) -> Result<(), InputError> {
        if down {
            self.keys.insert(u);
        } else {
            self.keys.remove(&u);
        }
        let up = if down {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        if u == usage::PAUSE {
            return Self::send(&[Self::key_input(VK_PAUSE.0, 0, up)]);
        }
        let (scan, ext) = hid::to_win_scancode(u).ok_or(InputError::UnmappedKey(u))?;
        let mut flags = KEYEVENTF_SCANCODE | up;
        if ext {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        Self::send(&[Self::key_input(0, scan, flags)])
    }
}

impl InputInjector for SendInputInjector {
    fn set_display(&mut self, target: DisplayTarget) {
        self.target = target;
    }

    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMove { x, y } => {
                let t = self.target;
                self.pos = mapping::to_host_pixel(*x, *y, t.x, t.y, t.width, t.height);
                // SAFETY: plain getters.
                let (vx, vy, vw, vh) = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
                    )
                };
                let nx = ((self.pos.0 - vx) as i64 * 65535 / (vw - 1) as i64) as i32;
                let ny = ((self.pos.1 - vy) as i64 * 65535 / (vh - 1) as i64) as i32;
                Self::send(&[Self::mouse(
                    nx,
                    ny,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                )])
            }
            InputEvent::MouseButton { button, down } => {
                self.buttons.retain(|b| b != button);
                if *down {
                    self.buttons.push(*button);
                }
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                    (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                    (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                    (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
                };
                Self::send(&[Self::mouse(0, 0, data as i32, flags)])
            }
            InputEvent::Scroll { dx, dy, .. } => {
                // 40 px ≈ one notch (WHEEL_DELTA); remainders carried over.
                let k = WHEEL_DELTA / PX_PER_WHEEL_NOTCH;
                let fy = self.wheel_rem.1 + dy * k;
                // Wire dx > 0 scrolls toward the left; HWHEEL > 0 is right.
                let fx = self.wheel_rem.0 - dx * k;
                let (iy, ix) = (fy.trunc(), fx.trunc());
                self.wheel_rem = (fx - ix, fy - iy);
                let mut v = Vec::with_capacity(2);
                if iy != 0.0 {
                    v.push(Self::mouse(0, 0, iy as i32, MOUSEEVENTF_WHEEL));
                }
                if ix != 0.0 {
                    v.push(Self::mouse(0, 0, ix as i32, MOUSEEVENTF_HWHEEL));
                }
                if v.is_empty() { Ok(()) } else { Self::send(&v) }
            }
            InputEvent::Key { usage, down, .. } => self.key(*usage, *down),
            InputEvent::Text(s) => {
                let mut v = Vec::new();
                for unit in s.encode_utf16() {
                    v.push(Self::key_input(0, unit, KEYEVENTF_UNICODE));
                    v.push(Self::key_input(
                        0,
                        unit,
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    ));
                }
                if v.is_empty() { Ok(()) } else { Self::send(&v) }
            }
        }
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        for b in std::mem::take(&mut self.buttons) {
            let _ = self.inject(&InputEvent::MouseButton {
                button: b,
                down: false,
            });
        }
        let held: Vec<u16> = self.keys.iter().copied().collect();
        for k in held {
            let _ = self.key(k, false);
        }
        Ok(())
    }
}

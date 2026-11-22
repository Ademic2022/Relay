//! macOS injection via Quartz events (FR-13, FR-14).
//!
//! - Events are posted at the HID tap, so they behave like real devices.
//! - Requires the Accessibility permission (TCC); [`accessibility_trusted`]
//!   checks it and can show the system prompt.
//! - Pointer moves become drag events while a button is held; click counts
//!   are set explicitly so double/triple clicks work.
//! - Scroll uses pixel units (continuous for trackpads, §10), lines for
//!   wheels, with fractional remainders carried over.
//! - Modifier keys are posted as `FlagsChanged` events with the full
//!   modifier state, which is what apps observe from a real keyboard.

#![allow(unsafe_code, non_upper_case_globals)]

use std::collections::HashSet;
use std::ffi::c_void;
use std::time::Instant;

use crate::hid::{self, usage};
use crate::*;

type CFTypeRef = *const c_void;
type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

const kCGEventSourceStateHIDSystemState: i32 = 1;
const kCGHIDEventTap: u32 = 0;

const kCGEventLeftMouseDown: u32 = 1;
const kCGEventLeftMouseUp: u32 = 2;
const kCGEventRightMouseDown: u32 = 3;
const kCGEventRightMouseUp: u32 = 4;
const kCGEventMouseMoved: u32 = 5;
const kCGEventLeftMouseDragged: u32 = 6;
const kCGEventRightMouseDragged: u32 = 7;
const kCGEventFlagsChanged: u32 = 12;
const kCGEventOtherMouseDown: u32 = 25;
const kCGEventOtherMouseUp: u32 = 26;
const kCGEventOtherMouseDragged: u32 = 27;

const kCGMouseEventClickState: u32 = 1;
const kCGScrollWheelEventIsContinuous: u32 = 88;
const kCGScrollEventUnitPixel: u32 = 0;
const kCGScrollEventUnitLine: u32 = 1;

const kCGEventFlagMaskAlphaShift: u64 = 0x0001_0000;
const kCGEventFlagMaskShift: u64 = 0x0002_0000;
const kCGEventFlagMaskControl: u64 = 0x0004_0000;
const kCGEventFlagMaskAlternate: u64 = 0x0008_0000;
const kCGEventFlagMaskCommand: u64 = 0x0010_0000;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    fn CGEventCreateMouseEvent(
        src: CGEventSourceRef,
        ty: u32,
        pos: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(src: CGEventSourceRef, key: u16, down: bool) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent2(
        src: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        w1: i32,
        w2: i32,
        w3: i32,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, ev: CGEventRef);
    fn CGEventSetFlags(ev: CGEventRef, flags: u64);
    fn CGEventSetType(ev: CGEventRef, ty: u32);
    fn CGEventSetIntegerValueField(ev: CGEventRef, field: u32, value: i64);
    fn CGEventKeyboardSetUnicodeString(ev: CGEventRef, len: usize, s: *const u16);
    fn CGDisplayBounds(display: u32) -> CGRect;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    static kCFBooleanTrue: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: [usize; 6];
    static kCFTypeDictionaryValueCallBacks: [usize; 5];
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: isize,
        kcb: *const c_void,
        vcb: *const c_void,
    ) -> CFTypeRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    static kAXTrustedCheckOptionPrompt: CFTypeRef;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
}

/// Whether this process may post input events. With `prompt`, macOS shows
/// its "grant Accessibility access" dialog if not yet trusted.
pub fn accessibility_trusted(prompt: bool) -> bool {
    // SAFETY: CF dictionary is created, used and released here.
    unsafe {
        if !prompt {
            return AXIsProcessTrustedWithOptions(std::ptr::null());
        }
        let keys = [kAXTrustedCheckOptionPrompt];
        let vals = [kCFBooleanTrue];
        let dict = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            vals.as_ptr(),
            1,
            (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
            (&raw const kCFTypeDictionaryValueCallBacks).cast(),
        );
        let ok = AXIsProcessTrustedWithOptions(dict);
        if !dict.is_null() {
            CFRelease(dict);
        }
        ok
    }
}

struct Event(CGEventRef);

impl Event {
    fn post(self) -> Result<(), InputError> {
        if self.0.is_null() {
            return Err(InputError::Platform("CGEventCreate failed".into()));
        }
        // SAFETY: valid event; released in Drop.
        unsafe { CGEventPost(kCGHIDEventTap, self.0) };
        Ok(())
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: created with +1 by a CGEventCreate* function.
            unsafe { CFRelease(self.0.cast_const()) };
        }
    }
}

fn button_code(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

pub struct CgEventInjector {
    source: CGEventSourceRef,
    bounds: CGRect,
    pos: CGPoint,
    buttons: Vec<MouseButton>,
    clicks: ClickCounter,
    keys: HashSet<u16>,
    flags: u64,
    scroll_rem: (f32, f32),
}

// SAFETY: the event source is only used through &mut self.
unsafe impl Send for CgEventInjector {}

impl std::fmt::Debug for CgEventInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CgEventInjector")
            .field("pos", &self.pos)
            .finish()
    }
}

impl Drop for CgEventInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
        if !self.source.is_null() {
            // SAFETY: created by CGEventSourceCreate.
            unsafe { CFRelease(self.source.cast_const()) };
        }
    }
}

impl CgEventInjector {
    pub fn new() -> Result<Self, InputError> {
        if !accessibility_trusted(false) {
            return Err(InputError::PermissionDenied);
        }
        // SAFETY: plain constructor.
        let source = unsafe { CGEventSourceCreate(kCGEventSourceStateHIDSystemState) };
        // SAFETY: main display bounds as a default target.
        let bounds = unsafe { CGDisplayBounds(0) };
        Ok(Self {
            source,
            bounds,
            pos: CGPoint::default(),
            buttons: Vec::new(),
            clicks: ClickCounter::default(),
            keys: HashSet::new(),
            flags: 0,
            scroll_rem: (0.0, 0.0),
        })
    }

    fn mouse(&self, ty: u32, button: u32, clicks: i64) -> Result<(), InputError> {
        // SAFETY: event created from our source; posted and released.
        let ev = Event(unsafe { CGEventCreateMouseEvent(self.source, ty, self.pos, button) });
        if !ev.0.is_null() {
            // SAFETY: valid event.
            unsafe {
                CGEventSetIntegerValueField(ev.0, kCGMouseEventClickState, clicks);
                CGEventSetFlags(ev.0, self.flags);
            }
        }
        ev.post()
    }

    fn key(&mut self, usage_code: u16, down: bool) -> Result<(), InputError> {
        let code = hid::to_mac_keycode(usage_code).ok_or(InputError::UnmappedKey(usage_code))?;
        if down {
            self.keys.insert(usage_code);
        } else {
            self.keys.remove(&usage_code);
        }
        let bit = match usage_code {
            usage::LEFT_SHIFT | usage::RIGHT_SHIFT => Some(kCGEventFlagMaskShift),
            usage::LEFT_CTRL | usage::RIGHT_CTRL => Some(kCGEventFlagMaskControl),
            usage::LEFT_ALT | usage::RIGHT_ALT => Some(kCGEventFlagMaskAlternate),
            usage::LEFT_META | usage::RIGHT_META => Some(kCGEventFlagMaskCommand),
            usage::CAPS_LOCK => Some(kCGEventFlagMaskAlphaShift),
            _ => None,
        };
        // SAFETY: event created from our source; posted and released.
        let ev = Event(unsafe { CGEventCreateKeyboardEvent(self.source, code, down) });
        if let Some(bit) = bit {
            if usage_code == usage::CAPS_LOCK {
                if down {
                    self.flags ^= bit; // toggles
                }
            } else {
                // Keep a side's bit while the other side is still held.
                let other_held = self.keys.iter().any(|&k| {
                    k != usage_code && hid::modifier_bit(k) == hid::modifier_bit(usage_code)
                });
                if down || other_held {
                    self.flags |= bit;
                } else {
                    self.flags &= !bit;
                }
            }
            if !ev.0.is_null() {
                // SAFETY: valid event.
                unsafe { CGEventSetType(ev.0, kCGEventFlagsChanged) };
            }
        }
        if !ev.0.is_null() {
            // SAFETY: valid event.
            unsafe { CGEventSetFlags(ev.0, self.flags) };
        }
        ev.post()
    }

    fn text(&self, s: &str) -> Result<(), InputError> {
        // Sent in chunks: CGEventKeyboardSetUnicodeString handles ≤ 20 units.
        let units: Vec<u16> = s.encode_utf16().collect();
        for chunk in units.chunks(20) {
            for down in [true, false] {
                // SAFETY: event created from our source; posted and released.
                let ev = Event(unsafe { CGEventCreateKeyboardEvent(self.source, 0, down) });
                if !ev.0.is_null() {
                    // SAFETY: valid event and UTF-16 buffer.
                    unsafe { CGEventKeyboardSetUnicodeString(ev.0, chunk.len(), chunk.as_ptr()) };
                }
                ev.post()?;
            }
        }
        Ok(())
    }

    fn scroll(&mut self, dx: f32, dy: f32, precise: bool) -> Result<(), InputError> {
        let (units, scale) = if precise {
            (kCGScrollEventUnitPixel, 1.0)
        } else {
            (kCGScrollEventUnitLine, 1.0 / PX_PER_WHEEL_NOTCH)
        };
        let fx = self.scroll_rem.0 + dx * scale;
        let fy = self.scroll_rem.1 + dy * scale;
        let (ix, iy) = (fx.trunc(), fy.trunc());
        self.scroll_rem = (fx - ix, fy - iy);
        if ix == 0.0 && iy == 0.0 {
            return Ok(());
        }
        // SAFETY: event created from our source; posted and released.
        let ev = Event(unsafe {
            CGEventCreateScrollWheelEvent2(self.source, units, 2, iy as i32, ix as i32, 0)
        });
        if !ev.0.is_null() {
            // SAFETY: valid event.
            unsafe {
                CGEventSetIntegerValueField(
                    ev.0,
                    kCGScrollWheelEventIsContinuous,
                    i64::from(precise),
                )
            };
        }
        ev.post()
    }
}

impl InputInjector for CgEventInjector {
    fn set_display(&mut self, target: DisplayTarget) {
        // Quartz event coordinates are global display points.
        // SAFETY: plain getter; returns an empty rect for unknown IDs.
        let b = unsafe { CGDisplayBounds(target.id) };
        if b.size.width > 0.0 {
            self.bounds = b;
        }
    }

    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMove { x, y } => {
                let b = self.bounds;
                self.pos = CGPoint {
                    x: b.origin.x + f64::from(x.clamp(0.0, 1.0)) * (b.size.width - 1.0),
                    y: b.origin.y + f64::from(y.clamp(0.0, 1.0)) * (b.size.height - 1.0),
                };
                let (ty, btn) = match self.buttons.first() {
                    Some(MouseButton::Left) => (kCGEventLeftMouseDragged, 0),
                    Some(MouseButton::Right) => (kCGEventRightMouseDragged, 1),
                    Some(&b) => (kCGEventOtherMouseDragged, button_code(b)),
                    None => (kCGEventMouseMoved, 0),
                };
                self.mouse(ty, btn, 0)
            }
            InputEvent::MouseButton { button, down } => {
                let code = button_code(*button);
                let ty = match (button, down) {
                    (MouseButton::Left, true) => kCGEventLeftMouseDown,
                    (MouseButton::Left, false) => kCGEventLeftMouseUp,
                    (MouseButton::Right, true) => kCGEventRightMouseDown,
                    (MouseButton::Right, false) => kCGEventRightMouseUp,
                    (_, true) => kCGEventOtherMouseDown,
                    (_, false) => kCGEventOtherMouseUp,
                };
                let clicks = if *down {
                    self.buttons.retain(|b| b != button);
                    self.buttons.push(*button);
                    self.clicks
                        .down(Instant::now(), *button, (self.pos.x, self.pos.y))
                } else {
                    self.buttons.retain(|b| b != button);
                    self.clicks.current()
                };
                self.mouse(ty, code, clicks)
            }
            InputEvent::Scroll { dx, dy, precise } => self.scroll(*dx, *dy, *precise),
            InputEvent::Key { usage, down, .. } => self.key(*usage, *down),
            InputEvent::Text(s) => self.text(s),
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

//! The Host's global "end session" hotkey, Ctrl+Alt+Shift+X / ⌃⌥⇧X (FR-10).
//!
//! Registered only while a session is active, so the combination belongs to
//! other apps the rest of the time. It does not need Accessibility or Input
//! Monitoring permission on macOS (Carbon hot keys are delivered by the
//! window server), and works on Windows without a window (`RegisterHotKey`
//! on a dedicated message-loop thread).
//!
//! Input injected for the Controller can also trigger it; that only ends
//! the Controller's own session, which is harmless.

/// What [`SessionHotkey::new`] calls when the combination is pressed. It
/// runs on the main thread (macOS) or the hotkey thread (Windows): keep it
/// short, e.g. notify a channel.
pub type OnPress = Box<dyn Fn() + Send + Sync + 'static>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HotkeyError {
    #[error("global hotkeys are not supported on this OS")]
    Unsupported,
    #[error("could not register the end-session hotkey: {0}")]
    Os(String),
}

pub use imp::SessionHotkey;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod imp {
    use super::{HotkeyError, OnPress};
    use std::ffi::c_void;
    use std::sync::OnceLock;

    type OSStatus = i32;
    type EventTargetRef = *mut c_void;
    type EventHotKeyRef = *mut c_void;
    type EventHandlerRef = *mut c_void;
    type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> OSStatus;

    #[repr(C)]
    struct EventTypeSpec {
        class: u32,
        kind: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EventHotKeyID {
        signature: u32,
        id: u32,
    }

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn GetApplicationEventTarget() -> EventTargetRef;
        fn InstallEventHandler(
            target: EventTargetRef,
            handler: Handler,
            num_types: usize,
            list: *const EventTypeSpec,
            user_data: *mut c_void,
            out: *mut EventHandlerRef,
        ) -> OSStatus;
        fn RegisterEventHotKey(
            key_code: u32,
            modifiers: u32,
            id: EventHotKeyID,
            target: EventTargetRef,
            options: u32,
            out: *mut EventHotKeyRef,
        ) -> OSStatus;
        fn UnregisterEventHotKey(key: EventHotKeyRef) -> OSStatus;
    }

    const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
    const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
    const KVK_ANSI_X: u32 = 0x07;
    // Carbon modifier masks: shiftKey | optionKey | controlKey.
    const MODIFIERS: u32 = 0x0200 | 0x0800 | 0x1000;
    const SIGNATURE: u32 = u32::from_be_bytes(*b"RLAY");

    static ON_PRESS: OnceLock<OnPress> = OnceLock::new();

    extern "C" fn handler(_call: *mut c_void, _event: *mut c_void, _data: *mut c_void) -> OSStatus {
        if let Some(f) = ON_PRESS.get() {
            f();
        }
        0 // noErr: handled
    }

    /// Must be created and used on the main thread, whose run loop (winit's
    /// event loop) delivers the hot key.
    #[derive(Debug)]
    pub struct SessionHotkey {
        key: Option<usize>, // EventHotKeyRef; usize keeps the type Send
    }

    impl SessionHotkey {
        pub fn new(on_press: OnPress) -> Result<Self, HotkeyError> {
            if ON_PRESS.set(on_press).is_err() {
                return Err(HotkeyError::Os("hotkey already created".into()));
            }
            let spec = EventTypeSpec {
                class: K_EVENT_CLASS_KEYBOARD,
                kind: K_EVENT_HOT_KEY_PRESSED,
            };
            let mut out: EventHandlerRef = std::ptr::null_mut();
            // SAFETY: valid spec array of length 1, out-pointer to a local,
            // handler is a 'static extern "C" fn.
            let st = unsafe {
                InstallEventHandler(
                    GetApplicationEventTarget(),
                    handler,
                    1,
                    &spec,
                    std::ptr::null_mut(),
                    &mut out,
                )
            };
            if st != 0 {
                return Err(HotkeyError::Os(format!("InstallEventHandler {st}")));
            }
            Ok(Self { key: None })
        }

        pub fn set_active(&mut self, on: bool) -> Result<(), HotkeyError> {
            match (on, self.key) {
                (true, None) => {
                    let mut out: EventHotKeyRef = std::ptr::null_mut();
                    let id = EventHotKeyID {
                        signature: SIGNATURE,
                        id: 1,
                    };
                    // SAFETY: out-pointer to a local; target from Carbon.
                    let st = unsafe {
                        RegisterEventHotKey(
                            KVK_ANSI_X,
                            MODIFIERS,
                            id,
                            GetApplicationEventTarget(),
                            0,
                            &mut out,
                        )
                    };
                    if st != 0 {
                        return Err(HotkeyError::Os(format!(
                            "RegisterEventHotKey {st} (another app may own ⌃⌥⇧X)"
                        )));
                    }
                    self.key = Some(out as usize);
                }
                (false, Some(k)) => {
                    // SAFETY: k came from RegisterEventHotKey and is
                    // unregistered once.
                    unsafe { UnregisterEventHotKey(k as EventHotKeyRef) };
                    self.key = None;
                }
                _ => {}
            }
            Ok(())
        }
    }

    impl Drop for SessionHotkey {
        fn drop(&mut self) {
            let _ = self.set_active(false);
        }
    }
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod imp {
    use super::{HotkeyError, OnPress};
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_APP, WM_HOTKEY,
    };

    const ID: i32 = 0x524C; // "RL"
    const VK_X: u32 = 0x58;
    const WM_ON: u32 = WM_APP + 1;
    const WM_OFF: u32 = WM_APP + 2;
    const WM_QUIT_HOTKEY: u32 = WM_APP + 3;

    /// Owns a thread with a message loop; `RegisterHotKey(NULL, …)` posts
    /// `WM_HOTKEY` to the registering thread, so registration happens there.
    #[derive(Debug)]
    pub struct SessionHotkey {
        thread_id: u32,
        thread: Option<std::thread::JoinHandle<()>>,
        on: bool,
    }

    impl SessionHotkey {
        pub fn new(on_press: OnPress) -> Result<Self, HotkeyError> {
            let (tx, rx) = std::sync::mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("relay-hotkey".into())
                .spawn(move || {
                    let mut msg = MSG::default();
                    // SAFETY: plain Win32 message-loop calls on this thread.
                    unsafe {
                        // Create this thread's message queue before anyone posts to it.
                        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                        let _ = tx.send(GetCurrentThreadId());
                        let mut registered = false;
                        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                            match msg.message {
                                WM_ON if !registered => {
                                    registered = RegisterHotKey(None, ID, MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT, VK_X).is_ok();
                                    if !registered {
                                        eprintln!("Relay: could not register Ctrl+Alt+Shift+X (another app may own it)");
                                    }
                                }
                                WM_OFF if registered => {
                                    let _ = UnregisterHotKey(None, ID);
                                    registered = false;
                                }
                                WM_HOTKEY if msg.wParam.0 == ID as usize => on_press(),
                                WM_QUIT_HOTKEY => break,
                                _ => {}
                            }
                        }
                        if registered {
                            let _ = UnregisterHotKey(None, ID);
                        }
                    }
                })
                .map_err(|e| HotkeyError::Os(e.to_string()))?;
            let thread_id = rx
                .recv()
                .map_err(|_| HotkeyError::Os("hotkey thread died".into()))?;
            Ok(Self {
                thread_id,
                thread: Some(thread),
                on: false,
            })
        }

        fn post(&self, m: u32) -> Result<(), HotkeyError> {
            // SAFETY: posting a thread message with no pointers attached.
            unsafe { PostThreadMessageW(self.thread_id, m, WPARAM(0), LPARAM(0)) }
                .map_err(|e| HotkeyError::Os(e.to_string()))
        }

        pub fn set_active(&mut self, on: bool) -> Result<(), HotkeyError> {
            if on != self.on {
                self.post(if on { WM_ON } else { WM_OFF })?;
                self.on = on;
            }
            Ok(())
        }
    }

    impl Drop for SessionHotkey {
        fn drop(&mut self) {
            let _ = self.post(WM_QUIT_HOTKEY);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    use super::{HotkeyError, OnPress};

    #[derive(Debug)]
    pub struct SessionHotkey;

    impl SessionHotkey {
        pub fn new(_on_press: OnPress) -> Result<Self, HotkeyError> {
            Err(HotkeyError::Unsupported)
        }

        pub fn set_active(&mut self, _on: bool) -> Result<(), HotkeyError> {
            Ok(())
        }
    }
}

//! Per-session permissions the Host user can change while a session runs
//! (FR-11): control, clipboard, file transfer. Viewing is the session
//! itself; ending it is "End session".
//!
//! The Host enforces them (input, clipboard and file streams check the
//! flags on every event or request) and tells the Controller with a fresh
//! `Permissions` message, so its toolbar and title can say what changed.
//! Nothing can be raised above what the Host allowed when the session began
//! (`--view-only`, `--no-files`, clipboard setting).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use relay_proto::v1;

#[derive(Debug)]
pub struct Perms {
    pub control: Arc<AtomicBool>,
    pub clipboard: Arc<AtomicBool>,
    pub files: Arc<AtomicBool>,
    /// The most each may be (set at session start).
    cap: [bool; 3],
    /// FR-18 offered (unattended sessions with control).
    pub privacy: bool,
    /// FR-18: the Host's own keyboard and mouse are blocked.
    pub block_input: Arc<AtomicBool>,
}

impl Perms {
    #[cfg(test)]
    pub fn new(control: bool, clipboard: bool, files: bool) -> Arc<Self> {
        Self::with_privacy(control, clipboard, files, false)
    }

    pub fn with_privacy(control: bool, clipboard: bool, files: bool, privacy: bool) -> Arc<Self> {
        Arc::new(Self {
            control: Arc::new(AtomicBool::new(control)),
            clipboard: Arc::new(AtomicBool::new(clipboard)),
            files: Arc::new(AtomicBool::new(files)),
            cap: [control, clipboard, files],
            privacy: privacy && control,
            block_input: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn get(&self) -> [bool; 3] {
        [
            self.control.load(Ordering::Relaxed),
            self.clipboard.load(Ordering::Relaxed),
            self.files.load(Ordering::Relaxed),
        ]
    }

    pub fn cap(&self) -> [bool; 3] {
        self.cap
    }

    /// Apply a change (clamped to the cap). Returns the new state.
    pub fn set(&self, want: [bool; 3]) -> [bool; 3] {
        for ((flag, w), c) in [&self.control, &self.clipboard, &self.files]
            .into_iter()
            .zip(want)
            .zip(self.cap)
        {
            flag.store(w && c, Ordering::Relaxed);
        }
        self.get()
    }

    pub fn to_proto(&self) -> v1::Permissions {
        let [control, clipboard, file_transfer] = self.get();
        v1::Permissions {
            view: true,
            control,
            clipboard,
            file_transfer,
            privacy: self.privacy && control,
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let ([c, cb, f], [cc, ccb, cf]) = (self.get(), self.cap);
        serde_json::json!({
            "control": c, "clipboard": cb, "files": f,
            "can_control": cc, "can_clipboard": ccb, "can_files": cf,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoke_and_restore_within_the_cap() {
        let p = Perms::new(true, false, true);
        assert_eq!(
            p.set([false, true, false]),
            [false, false, false],
            "clipboard capped"
        );
        assert_eq!(
            p.set([true, true, true]),
            [true, false, true],
            "restored up to the cap"
        );
        let m = p.to_proto();
        assert!(m.view && m.control && !m.clipboard && m.file_transfer);
    }
}

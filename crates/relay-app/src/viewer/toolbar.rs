//! Session toolbar (FR-19): a strip at the top centre of the viewer that
//! slides in when the pointer touches the top edge. Every button does what
//! its Ctrl+Alt shortcut does; clicks on it never reach the Host.
//!
//! It is one line of text drawn by the overlay renderer, so hit-testing is
//! by character column: [`Toolbar::hit`].

use super::overlay::PAD;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Quality,
    Monitor,
    CtrlAltDel,
    Chat,
    Files,
    Stats,
    FullScreen,
    /// FR-18 (unattended hosts): the host's own keyboard and mouse.
    BlockInput,
    /// FR-18: black out the host's monitors.
    Blackout,
    Disconnect,
}

/// Pointer this close to the top edge (physical px) reveals the toolbar.
pub const REVEAL_PX: f64 = 6.0;
/// It stays while the pointer is within this distance of it.
pub const KEEP_PX: f32 = 24.0;

#[derive(Debug, Default)]
pub struct Toolbar {
    /// Button and its character columns `[start, end)` in the line.
    spans: Vec<(Tool, usize, usize)>,
    pub line: String,
    pub shown: bool,
}

/// What the buttons say right now.
#[derive(Debug, Clone, Default)]
pub struct Labels {
    pub quality: String,
    /// "Display 1/2", or empty with a single display.
    pub monitor: String,
    pub unread: u32,
    pub stats_on: bool,
    pub full_screen: bool,
    /// FR-18: offered, and (block input, black screen) in effect.
    pub privacy: Option<(bool, bool)>,
}

impl Toolbar {
    /// Rebuild the line from `l`.
    pub fn set(&mut self, l: &Labels) {
        let mut items: Vec<(Tool, String)> = vec![(Tool::Quality, l.quality.clone())];
        if !l.monitor.is_empty() {
            items.push((Tool::Monitor, l.monitor.clone()));
        }
        items.extend([
            (Tool::CtrlAltDel, "Ctrl+Alt+Del".to_string()),
            (
                Tool::Chat,
                if l.unread > 0 {
                    format!("Chat ({})", l.unread)
                } else {
                    "Chat".into()
                },
            ),
            (Tool::Files, "Files".into()),
            (
                Tool::Stats,
                if l.stats_on { "Stats on" } else { "Stats" }.into(),
            ),
            (
                Tool::FullScreen,
                if l.full_screen {
                    "Window"
                } else {
                    "Full screen"
                }
                .into(),
            ),
        ]);
        if let Some((block, black)) = l.privacy {
            items.push((
                Tool::BlockInput,
                if block {
                    "Unblock input"
                } else {
                    "Block input"
                }
                .into(),
            ));
            items.push((
                Tool::Blackout,
                if black { "Show screen" } else { "Black screen" }.into(),
            ));
        }
        items.push((Tool::Disconnect, "Disconnect".into()));
        self.line.clear();
        self.spans.clear();
        for (i, (tool, label)) in items.into_iter().enumerate() {
            if i > 0 {
                self.line.push_str(" | ");
            }
            let start = self.line.chars().count();
            self.line.push_str(&format!(" {label} "));
            self.spans.push((tool, start, self.line.chars().count()));
        }
    }

    /// The button under `x` (physical px from the toolbar's left edge),
    /// drawn at integer `scale`.
    pub fn hit(&self, x: f32, scale: usize) -> Option<Tool> {
        let cell = (8 * scale) as f32;
        let col = (x - (PAD * scale) as f32) / cell;
        if col < 0.0 {
            return None;
        }
        let col = col as usize;
        self.spans
            .iter()
            .find(|(_, a, b)| (*a..*b).contains(&col))
            .map(|(t, ..)| *t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_and_hits() {
        let mut t = Toolbar::default();
        t.set(&Labels {
            quality: "Speed".into(),
            monitor: "Display 1/2".into(),
            unread: 2,
            ..Default::default()
        });
        assert!(t.line.starts_with(" Speed  |  Display 1/2 "));
        assert!(t.line.contains("Chat (2)"));
        // Scale 2: each character is 16 px, after 8 px padding.
        let at = |col: usize| 8.0 + col as f32 * 16.0 + 4.0;
        assert_eq!(t.hit(at(1), 2), Some(Tool::Quality));
        assert_eq!(t.hit(at(7), 2), None, "the separator is not a button");
        assert_eq!(t.hit(at(10), 2), Some(Tool::Monitor));
        let last = t.line.chars().count() - 2;
        assert_eq!(t.hit(at(last), 2), Some(Tool::Disconnect));
        assert_eq!(t.hit(2.0, 2), None, "padding");

        t.set(&Labels {
            quality: "Speed".into(),
            ..Default::default()
        });
        assert!(!t.line.contains("Display"), "no picker with one display");
        assert!(!t.line.contains("Block"), "privacy only when offered");
        t.set(&Labels {
            privacy: Some((true, false)),
            ..Default::default()
        });
        assert!(t.line.contains("Unblock input") && t.line.contains("Black screen"));
    }
}

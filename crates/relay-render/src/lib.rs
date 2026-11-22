//! Remote-screen viewer (M1): a native wgpu surface, not the webview.
//!
//! - Decoded NV12 textures are sampled directly (zero-copy) and converted
//!   to RGB in a fragment shader.
//! - Present mode is Mailbox, falling back to Immediate: the viewer never
//!   waits on vsync (§6 "Present").
//! - The cursor is drawn locally from the cursor channel (§6 rule 5), on
//!   top of the video, so it moves at input rate rather than video rate.

/// Present modes in preference order. Fifo (vsync) is deliberately absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentPreference {
    Mailbox,
    Immediate,
}

pub const PRESENT_ORDER: [PresentPreference; 2] =
    [PresentPreference::Mailbox, PresentPreference::Immediate];

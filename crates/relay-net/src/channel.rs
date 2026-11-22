//! Logical channels multiplexed on one QUIC connection (§8.1).

/// Logical channel. The discriminant is the first byte written on each
/// unidirectional / bidirectional stream so the receiver can route it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Channel {
    Control = 0,
    Input = 1,
    Cursor = 2,
    Clipboard = 3,
    Chat = 4,
    Files = 5,
}

/// Send priority; higher wins. Mapped onto `quinn::SendStream::set_priority`.
/// Video uses datagrams, which QUIC sends ahead of stream data.
impl Channel {
    pub const fn priority(self) -> i32 {
        match self {
            Channel::Control | Channel::Input => 100, // §6 rule 8
            Channel::Cursor => 80,
            Channel::Clipboard | Channel::Chat => 50,
            Channel::Files => 10, // FR-28: below input and video
        }
    }

    pub const fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0 => Channel::Control,
            1 => Channel::Input,
            2 => Channel::Cursor,
            3 => Channel::Clipboard,
            4 => Channel::Chat,
            5 => Channel::Files,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_ordering() {
        for c in [
            Channel::Control,
            Channel::Input,
            Channel::Cursor,
            Channel::Clipboard,
            Channel::Chat,
            Channel::Files,
        ] {
            assert_eq!(Channel::from_u8(c as u8), Some(c));
        }
        assert!(Channel::Input.priority() > Channel::Files.priority());
        assert_eq!(Channel::from_u8(200), None);
    }
}

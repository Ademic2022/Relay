//! Network layer for Relay.
//!
//! M0 provides the channel map and video datagram framing. QUIC setup
//! (`quinn`), hole punching, relay client and connection migration arrive
//! in M1 and M3.

pub mod channel;
pub mod ctl;
pub mod demux;
pub mod quic;
pub mod tls;
pub mod video;

/// Default rendezvous server port (UDP/QUIC).
pub const RENDEZVOUS_PORT: u16 = 21116;
/// Default relay server port (QUIC).
pub const RELAY_PORT: u16 = 21117;
/// NAT keep-alive interval to the rendezvous server (§8.2 step 1).
pub const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
/// Hole-punch budget before falling back to relay (FR-4).
pub const PUNCH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Lower-case hex of `b` (keys on the command line and in files).
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Parse a 64-character hex key.
pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

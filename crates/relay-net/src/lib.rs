//! Network layer for Relay.
//!
//! M0 provides the channel map and video datagram framing. QUIC setup
//! (`quinn`), hole punching, relay client and connection migration arrive
//! in M1 and M3.

pub mod channel;
pub mod quic;
pub mod video;

/// Default rendezvous server port (UDP/QUIC).
pub const RENDEZVOUS_PORT: u16 = 21116;
/// Default relay server port (QUIC).
pub const RELAY_PORT: u16 = 21117;
/// NAT keep-alive interval to the rendezvous server (§8.2 step 1).
pub const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
/// Hole-punch budget before falling back to relay (FR-4).
pub const PUNCH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

//! Password-authenticated key exchange interface (FR-3, §8.2 step 5).
//!
//! Implemented with `opaque-ke` in M3. The output is bound to the TLS
//! exporter of the QUIC connection so a relay cannot man-in-the-middle.

/// Which password the Host is checking against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordKind {
    OneTime,
    Permanent,
}

/// One side of a PAKE run, driven message by message over the control stream.
pub trait PakeParty {
    /// Feed the peer's message (or `None` to start). Returns the next
    /// message to send, if any.
    fn step(&mut self, incoming: Option<&[u8]>) -> Result<Option<Vec<u8>>, PakeError>;
    /// Shared session key once the exchange has finished.
    fn session_key(&self) -> Option<[u8; 32]>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PakeError {
    #[error("wrong password")]
    BadPassword,
    #[error("malformed PAKE message")]
    Malformed,
    #[error("unexpected PAKE message for current step")]
    OutOfOrder,
}

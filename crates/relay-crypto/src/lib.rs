//! Device identity and password handling (FR-1, FR-2, FR-3, FR-5, FR-31).
//!
//! The PAKE (OPAQUE, FR-3) lands in M3; the [`pake`] module holds its
//! interface so session code can be written against it now.

pub mod device;
pub mod keystore;
pub mod pake;
pub mod password;
pub mod totp;
pub mod update;

pub use device::{DeviceId, DeviceKey, PeerPins, PinCheck};

/// Errors from this crate.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("OS random number generator failed: {0}")]
    Rng(String),
    #[error("invalid key material")]
    InvalidKey,
    #[error("keystore error: {0}")]
    Keystore(String),
}

pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|e| CryptoError::Rng(e.to_string()))?;
    Ok(buf)
}

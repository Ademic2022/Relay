//! Ed25519 device keys, 9-digit Device IDs and peer key pinning.

use std::collections::HashMap;
use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::{CryptoError, random_bytes};

/// Domain separator for Device ID derivation.
const ID_CONTEXT: &str = "relay v1 device-id";

/// A 9-digit public device identifier, always in `100_000_000..=999_999_999`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(u32);

impl DeviceId {
    pub const MIN: u32 = 100_000_000;
    pub const MAX: u32 = 999_999_999;

    /// Derive the ID from a public key. `nonce` is bumped by the rendezvous
    /// server on collision (§8.3), so a key maps to the same ID every time
    /// for a given nonce.
    pub fn derive(pubkey: &[u8; 32], nonce: u32) -> Self {
        let mut h = blake3::Hasher::new_derive_key(ID_CONTEXT);
        h.update(pubkey);
        h.update(&nonce.to_le_bytes());
        let digest = h.finalize();
        let n = u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("8 bytes"));
        let span = u64::from(Self::MAX - Self::MIN + 1);
        // Bias from the modulo is < 2^-34; irrelevant for an identifier.
        DeviceId(Self::MIN + (n % span) as u32)
    }

    /// Parse user input. Spaces and dashes are ignored ("123 456 789").
    pub fn parse(s: &str) -> Option<Self> {
        let digits: String = s.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
        if digits.len() != 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Self::from_u32(digits.parse().ok()?)
    }

    pub fn from_u32(n: u32) -> Option<Self> {
        (Self::MIN..=Self::MAX).contains(&n).then_some(DeviceId(n))
    }

    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Display for DeviceId {
    /// Grouped for reading aloud: `123 456 789`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        write!(
            f,
            "{:03} {:03} {:03}",
            n / 1_000_000,
            n / 1_000 % 1_000,
            n % 1_000
        )
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

/// The device's long-term Ed25519 keypair (FR-1).
pub struct DeviceKey {
    signing: SigningKey,
}

impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the secret.
        f.debug_struct("DeviceKey")
            .field("public", &self.public_bytes())
            .finish()
    }
}

impl DeviceKey {
    pub fn generate() -> Result<Self, CryptoError> {
        Ok(Self::from_secret(&random_bytes::<32>()?))
    }

    pub fn from_secret(secret: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(secret),
        }
    }

    /// Secret seed, for the OS keystore only.
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn device_id(&self, nonce: u32) -> DeviceId {
        DeviceId::derive(&self.public_bytes(), nonce)
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }
}

/// Verify an Ed25519 signature by a peer's public key.
pub fn verify(pubkey: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> Result<(), CryptoError> {
    let key = VerifyingKey::from_bytes(pubkey).map_err(|_| CryptoError::InvalidKey)?;
    key.verify(msg, &Signature::from_bytes(sig))
        .map_err(|_| CryptoError::InvalidKey)
}

/// Result of checking a peer's key against the pin store (FR-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinCheck {
    /// First contact; the caller should pin the key once auth succeeds.
    New,
    /// Matches the pinned key.
    Match,
    /// Key differs from the pinned key: show a warning.
    Changed,
}

/// Trust-on-first-use pin store of peer public keys, keyed by Device ID.
#[derive(Debug, Default, Clone)]
pub struct PeerPins {
    pins: HashMap<DeviceId, [u8; 32]>,
}

impl PeerPins {
    pub fn check(&self, id: DeviceId, pubkey: &[u8; 32]) -> PinCheck {
        match self.pins.get(&id) {
            None => PinCheck::New,
            Some(k) if k == pubkey => PinCheck::Match,
            Some(_) => PinCheck::Changed,
        }
    }

    pub fn pin(&mut self, id: DeviceId, pubkey: [u8; 32]) {
        self.pins.insert(id, pubkey);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DeviceId, &[u8; 32])> {
        self.pins.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_nine_digits_and_stable() {
        let key = DeviceKey::from_secret(&[1u8; 32]);
        let a = key.device_id(0);
        assert_eq!(a, key.device_id(0));
        assert!((DeviceId::MIN..=DeviceId::MAX).contains(&a.as_u32()));
        assert_ne!(a, key.device_id(1), "nonce must change the ID");
    }

    #[test]
    fn id_parse_and_display() {
        let id = DeviceId::parse("123 456-789").unwrap();
        assert_eq!(id.as_u32(), 123_456_789);
        assert_eq!(id.to_string(), "123 456 789");
        assert_eq!(DeviceId::parse(&id.to_string()), Some(id));
        assert!(DeviceId::parse("012345678").is_none());
        assert!(DeviceId::parse("12345678").is_none());
        assert!(DeviceId::parse("12345678a").is_none());
    }

    #[test]
    fn sign_verify() {
        let key = DeviceKey::generate().unwrap();
        let sig = key.sign(b"hello");
        verify(&key.public_bytes(), b"hello", &sig).unwrap();
        assert!(verify(&key.public_bytes(), b"hellp", &sig).is_err());
    }

    #[test]
    fn secret_round_trip() {
        let key = DeviceKey::generate().unwrap();
        let again = DeviceKey::from_secret(&key.secret_bytes());
        assert_eq!(key.public_bytes(), again.public_bytes());
    }

    #[test]
    fn pinning() {
        let mut pins = PeerPins::default();
        let id = DeviceId::from_u32(555_555_555).unwrap();
        assert_eq!(pins.check(id, &[1; 32]), PinCheck::New);
        pins.pin(id, [1; 32]);
        assert_eq!(pins.check(id, &[1; 32]), PinCheck::Match);
        assert_eq!(pins.check(id, &[2; 32]), PinCheck::Changed);
    }
}

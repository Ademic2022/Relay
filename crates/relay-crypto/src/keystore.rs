//! Storage for the device secret key (FR-1).
//!
//! Platform backends (Windows DPAPI / Credential Manager, macOS Keychain)
//! arrive with M3. [`MemoryKeystore`] serves tests and the mock pipeline.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::CryptoError;

/// A secret store. Implementations must never write secrets in plaintext.
pub trait Keystore: Send + Sync {
    fn load(&self, name: &str) -> Result<Option<Vec<u8>>, CryptoError>;
    fn store(&self, name: &str, secret: &[u8]) -> Result<(), CryptoError>;
    fn delete(&self, name: &str) -> Result<(), CryptoError>;
}

/// Name under which the device key is stored.
pub const DEVICE_KEY_ENTRY: &str = "relay.device-key.v1";

/// Load the device key, generating and storing one on first launch.
pub fn load_or_create_device_key(ks: &dyn Keystore) -> Result<crate::DeviceKey, CryptoError> {
    if let Some(bytes) = ks.load(DEVICE_KEY_ENTRY)? {
        let secret: [u8; 32] = bytes.try_into().map_err(|_| CryptoError::InvalidKey)?;
        return Ok(crate::DeviceKey::from_secret(&secret));
    }
    let key = crate::DeviceKey::generate()?;
    ks.store(DEVICE_KEY_ENTRY, &key.secret_bytes())?;
    Ok(key)
}

/// In-memory keystore for tests.
#[derive(Debug, Default)]
pub struct MemoryKeystore(Mutex<HashMap<String, Vec<u8>>>);

impl Keystore for MemoryKeystore {
    fn load(&self, name: &str) -> Result<Option<Vec<u8>>, CryptoError> {
        Ok(self.0.lock().expect("poisoned").get(name).cloned())
    }
    fn store(&self, name: &str, secret: &[u8]) -> Result<(), CryptoError> {
        self.0
            .lock()
            .expect("poisoned")
            .insert(name.to_owned(), secret.to_vec());
        Ok(())
    }
    fn delete(&self, name: &str) -> Result<(), CryptoError> {
        self.0.lock().expect("poisoned").remove(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_created_once() {
        let ks = MemoryKeystore::default();
        let a = load_or_create_device_key(&ks).unwrap();
        let b = load_or_create_device_key(&ks).unwrap();
        assert_eq!(a.public_bytes(), b.public_bytes());
    }
}

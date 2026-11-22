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

/// The OS secret store: macOS Keychain, Windows Credential Manager (FR-1).
#[cfg(feature = "os-keystore")]
#[derive(Debug, Clone)]
pub struct OsKeystore {
    service: String,
}

#[cfg(feature = "os-keystore")]
impl OsKeystore {
    /// `service` separates installations (e.g. "relay", "relay-dev").
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, name: &str) -> Result<keyring::Entry, CryptoError> {
        keyring::Entry::new(&self.service, name).map_err(|e| CryptoError::Keystore(e.to_string()))
    }
}

#[cfg(feature = "os-keystore")]
impl Keystore for OsKeystore {
    fn load(&self, name: &str) -> Result<Option<Vec<u8>>, CryptoError> {
        match self.entry(name)?.get_secret() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CryptoError::Keystore(e.to_string())),
        }
    }

    fn store(&self, name: &str, secret: &[u8]) -> Result<(), CryptoError> {
        self.entry(name)?
            .set_secret(secret)
            .map_err(|e| CryptoError::Keystore(e.to_string()))
    }

    fn delete(&self, name: &str) -> Result<(), CryptoError> {
        match self.entry(name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CryptoError::Keystore(e.to_string())),
        }
    }
}

/// Secrets as files in a directory only root / SYSTEM can read (the
/// unattended agent runs before anyone logs in, when no user keychain is
/// available). Files are created owner-only (0600 on Unix). On Windows the
/// caller wraps contents with machine-scope DPAPI via `seal` / `open`.
pub struct FileKeystore {
    dir: std::path::PathBuf,
    seal: fn(&[u8]) -> Result<Vec<u8>, CryptoError>,
    open: fn(&[u8]) -> Result<Vec<u8>, CryptoError>,
}

impl std::fmt::Debug for FileKeystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileKeystore")
            .field("dir", &self.dir)
            .finish()
    }
}

fn identity(b: &[u8]) -> Result<Vec<u8>, CryptoError> {
    Ok(b.to_vec())
}

impl FileKeystore {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self::with_sealing(dir, identity, identity)
    }

    /// `seal`/`open` encrypt at rest (e.g. Windows DPAPI).
    pub fn with_sealing(
        dir: impl Into<std::path::PathBuf>,
        seal: fn(&[u8]) -> Result<Vec<u8>, CryptoError>,
        open: fn(&[u8]) -> Result<Vec<u8>, CryptoError>,
    ) -> Self {
        Self {
            dir: dir.into(),
            seal,
            open,
        }
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.join(format!("{name}.secret"))
    }
}

impl Keystore for FileKeystore {
    fn load(&self, name: &str) -> Result<Option<Vec<u8>>, CryptoError> {
        match std::fs::read(self.path(name)) {
            Ok(b) => (self.open)(&b).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CryptoError::Keystore(e.to_string())),
        }
    }

    fn store(&self, name: &str, secret: &[u8]) -> Result<(), CryptoError> {
        use std::io::Write;
        let err = |e: std::io::Error| CryptoError::Keystore(e.to_string());
        std::fs::create_dir_all(&self.dir).map_err(err)?;
        let sealed = (self.seal)(secret)?;
        let tmp = self.dir.join(format!("{name}.secret.tmp"));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        o.open(&tmp).map_err(err)?.write_all(&sealed).map_err(err)?;
        std::fs::rename(&tmp, self.path(name)).map_err(err)
    }

    fn delete(&self, name: &str) -> Result<(), CryptoError> {
        match std::fs::remove_file(self.path(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CryptoError::Keystore(e.to_string())),
        }
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    #[test]
    fn file_keystore_round_trip_owner_only() {
        let d = std::env::temp_dir().join(format!("relay-fks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let ks = FileKeystore::new(&d);
        assert_eq!(ks.load("k").unwrap(), None);
        ks.store("k", b"secret").unwrap();
        assert_eq!(ks.load("k").unwrap().as_deref(), Some(&b"secret"[..]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.join("k.secret"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        ks.delete("k").unwrap();
        assert_eq!(ks.load("k").unwrap(), None);
    }
}

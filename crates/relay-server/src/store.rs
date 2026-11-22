//! On-disk state: the server identity and the Device ID registry.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use relay_crypto::DeviceId;
use relay_net::tls::Identity;

use crate::{hex, parse_hex32};

/// Load `server.key` (PKCS#8) or create it with owner-only permissions.
/// Its public key is what clients pin (§8.3); it is printed at start.
pub fn load_or_create_identity(dir: &Path) -> anyhow::Result<Identity> {
    let path = dir.join("server.key");
    if let Ok(der) = std::fs::read(&path) {
        let kp = rcgen_public(&der)?;
        return Identity::from_ed25519(&der, kp).map_err(anyhow::Error::msg);
    }
    let id = Identity::generate().map_err(anyhow::Error::msg)?;
    write_private(&path, id.pkcs8())?;
    Ok(id)
}

/// Ed25519 PKCS#8 v1 (RFC 8410) is 16 bytes of prefix + the 32-byte
/// seed; derive the public key from the seed.
fn rcgen_public(der: &[u8]) -> anyhow::Result<[u8; 32]> {
    let seed: [u8; 32] = der
        .get(16..48)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| anyhow::anyhow!("server.key is not an Ed25519 PKCS#8 v1 key"))?;
    Ok(relay_crypto::DeviceKey::from_secret(&seed).public_bytes())
}

fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(data)?;
    Ok(())
}

/// Device ID ↔ public key registry (`devices.txt`: "id hexkey" lines).
/// IDs are derived from the key; on collision the nonce is bumped
/// (§8.3), so a key keeps its ID forever.
#[derive(Debug, Clone)]
pub struct Devices {
    path: PathBuf,
    inner: Arc<Mutex<HashMap<u32, [u8; 32]>>>,
}

impl Devices {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("devices.txt");
        let mut map = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if let Some((id, key)) = line.split_once(' ')
                    && let (Ok(id), Some(key)) = (id.parse::<u32>(), parse_hex32(key))
                {
                    map.insert(id, key);
                }
            }
        }
        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(map)),
        })
    }

    /// The ID for `key`, assigning (and persisting) one if new.
    pub fn id_for(&self, key: &[u8; 32]) -> anyhow::Result<DeviceId> {
        let mut m = self.inner.lock().expect("poisoned");
        for nonce in 0..1000 {
            let id = DeviceId::derive(key, nonce);
            match m.get(&id.as_u32()) {
                Some(k) if k == key => return Ok(id),
                Some(_) => continue, // collision: next nonce
                None => {
                    m.insert(id.as_u32(), *key);
                    let mut f = std::fs::OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(&self.path)?;
                    writeln!(f, "{} {}", id.as_u32(), hex(key))?;
                    return Ok(id);
                }
            }
        }
        anyhow::bail!("could not assign a Device ID")
    }

    pub fn key_of(&self, id: u32) -> Option<[u8; 32]> {
        self.inner.lock().expect("poisoned").get(&id).copied()
    }
}

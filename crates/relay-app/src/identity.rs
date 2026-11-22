//! This device's identity (FR-1).
//!
//! The Ed25519 device key lives in the OS keystore (Keychain / Credential
//! Manager). `RELAY_KEYSTORE=memory` uses a throwaway key instead, for
//! tests and quick experiments; the Device ID then changes every run.

use relay_crypto::DeviceKey;
use relay_crypto::keystore::{self, Keystore, MemoryKeystore};
use relay_net::tls::Identity;

pub struct Me {
    pub key: DeviceKey,
    pub tls: Identity,
}

impl Me {
    pub fn public_key(&self) -> [u8; 32] {
        self.key.public_bytes()
    }
}

pub fn load() -> anyhow::Result<Me> {
    let mem = MemoryKeystore::default();
    let os;
    let ks: &dyn Keystore = match std::env::var("RELAY_KEYSTORE").as_deref() {
        Ok("memory") => &mem,
        _ => {
            os = keystore::OsKeystore::new("relay");
            &os
        }
    };
    let key = keystore::load_or_create_device_key(ks).map_err(|e| {
        anyhow::anyhow!("device key ({e}); set RELAY_KEYSTORE=memory to use a temporary key")
    })?;
    from_key(key)
}

pub fn from_key(key: DeviceKey) -> anyhow::Result<Me> {
    let tls =
        Identity::from_ed25519(&key.pkcs8_der(), key.public_bytes()).map_err(anyhow::Error::msg)?;
    Ok(Me { key, tls })
}

#[cfg(test)]
pub fn ephemeral() -> Me {
    from_key(DeviceKey::generate().expect("rng")).expect("identity")
}

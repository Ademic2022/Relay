//! Known-hosts pinning on the Controller (FR-5).
//!
//! The first time a Host is reached, its device key is remembered (by
//! Device ID, or by address for direct connections). A different key
//! later means the Host was reinstalled, or someone is impersonating it,
//! so the connection is refused until the user forgets the old key.

use std::path::PathBuf;

use relay_crypto::PinCheck;

fn path() -> Option<PathBuf> {
    if std::env::var("RELAY_KEYSTORE").as_deref() == Ok("memory") {
        return None; // throwaway identities: do not touch the real pin file
    }
    Some(config_dir()?.join("relay").join("known_hosts"))
}

/// Per-user configuration directory.
fn config_dir() -> Option<PathBuf> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(target_os = "windows") {
        env("APPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library").join("Application Support"))
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))
    }
}

fn load() -> Vec<(String, [u8; 32])> {
    let Some(p) = path() else { return Vec::new() };
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (name, key) = l.split_once(' ')?;
            Some((name.to_string(), relay_net::parse_hex32(key)?))
        })
        .collect()
}

pub fn check(name: &str, key: &[u8; 32]) -> PinCheck {
    match load().iter().find(|(n, _)| n == name) {
        None => PinCheck::New,
        Some((_, k)) if k == key => PinCheck::Match,
        Some(_) => PinCheck::Changed,
    }
}

pub fn pin(name: &str, key: &[u8; 32]) {
    let Some(p) = path() else { return };
    let mut entries: Vec<_> = load().into_iter().filter(|(n, _)| n != name).collect();
    entries.push((name.to_string(), *key));
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text: String = entries
        .iter()
        .map(|(n, k)| format!("{n} {}\n", relay_net::hex(k)))
        .collect();
    if let Err(e) = std::fs::write(&p, text) {
        tracing::warn!("could not save {}: {e}", p.display());
    }
}

pub fn forget(name: &str) {
    let Some(p) = path() else { return };
    let text: String = load()
        .into_iter()
        .filter(|(n, _)| n != name)
        .map(|(n, k)| format!("{n} {}\n", relay_net::hex(&k)))
        .collect();
    let _ = std::fs::write(p, text);
}

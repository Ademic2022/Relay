//! `relay update check|apply` (FR-42).
//!
//! Fetches `<update_url>/manifest` and `<update_url>/manifest.sig` with the
//! system `curl` (macOS and Windows 10+ ship it), verifies the Ed25519
//! signature against the release key pinned at build time
//! (`RELAY_UPDATE_KEY`), refuses anything not newer than this build, then
//! downloads this platform's artifact, checks size and BLAKE3, and swaps
//! it in place of the running binary (the old one is kept as `.old`).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use relay_crypto::update::{Manifest, Version, platform};

/// Release public key pinned at build time; `RELAY_UPDATE_KEY_OVERRIDE`
/// (tests) replaces it at run time.
fn release_key() -> Option<[u8; 32]> {
    std::env::var("RELAY_UPDATE_KEY_OVERRIDE")
        .ok()
        .or_else(|| option_env!("RELAY_UPDATE_KEY").map(str::to_string))
        .and_then(|k| relay_net::parse_hex32(&k))
}

pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is x.y.z")
}

fn fetch(url: &str, to: &Path) -> anyhow::Result<()> {
    // --proto restricts redirects too: no plain HTTP anywhere.
    let st = std::process::Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https,file",
            "--max-time",
            "600",
            "-o",
        ])
        .arg(to)
        .arg(url)
        .status()
        .context("running curl")?;
    if !st.success() {
        bail!("download failed: {url}");
    }
    Ok(())
}

pub struct Found {
    pub manifest: Manifest,
}

/// Fetch and verify the manifest. `Ok(None)` = already up to date.
pub fn check(base: &str, key: &[u8; 32], tmp: &Path) -> anyhow::Result<Option<Found>> {
    let (m, s) = (tmp.join("manifest"), tmp.join("manifest.sig"));
    fetch(&format!("{base}/manifest"), &m)?;
    fetch(&format!("{base}/manifest.sig"), &s)?;
    let text = std::fs::read_to_string(&m)?;
    let sig = std::fs::read_to_string(&s)?;
    let manifest = Manifest::verify(&text, &sig, key)?;
    if manifest.version <= current() {
        return Ok(None);
    }
    Ok(Some(Found { manifest }))
}

/// Download and verify this platform's artifact, then replace `exe`.
pub fn apply(found: &Found, exe: &Path, tmp: &Path) -> anyhow::Result<()> {
    let plat = platform();
    let a = found
        .manifest
        .artifact(&plat)
        .with_context(|| format!("no build for {plat} in version {}", found.manifest.version))?;
    let download = tmp.join("relay.download");
    fetch(&a.url, &download)?;
    let len = std::fs::metadata(&download)?.len();
    if len != a.size {
        bail!(
            "download has the wrong size ({len} bytes, expected {})",
            a.size
        );
    }
    if crate::files::store::hash_file(&download)? != a.blake3 {
        bail!("download failed its BLAKE3 check");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&download, std::fs::Permissions::from_mode(0o755))?;
    }
    // Stage next to the target (same filesystem), then swap. Windows lets
    // a running executable be renamed, not overwritten.
    let staged = exe.with_extension("new");
    std::fs::copy(&download, &staged)?;
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(&staged, exe) {
        let _ = std::fs::rename(&old, exe); // put the old one back
        return Err(e.into());
    }
    Ok(())
}

fn setup() -> anyhow::Result<(String, [u8; 32], PathBuf)> {
    let base = crate::settings::Settings::load()
        .get("update_url")
        .context("no update URL (relay config set update_url https://...)")?;
    let key =
        release_key().context("this build has no release key pinned; updates are disabled")?;
    let tmp = std::env::temp_dir().join(format!("relay-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    Ok((base.trim_end_matches('/').to_string(), key, tmp))
}

pub fn check_cli() -> anyhow::Result<()> {
    let (base, key, tmp) = setup()?;
    match check(&base, &key, &tmp)? {
        None => println!("Relay {} is up to date.", current()),
        Some(f) => println!(
            "Relay {} is available (you have {}). Run `relay update apply`.",
            f.manifest.version,
            current()
        ),
    }
    Ok(())
}

pub fn apply_cli() -> anyhow::Result<()> {
    let (base, key, tmp) = setup()?;
    let Some(found) = check(&base, &key, &tmp)? else {
        println!("Relay {} is up to date.", current());
        return Ok(());
    };
    let exe = std::env::current_exe()?.canonicalize()?;
    apply(&found, &exe, &tmp)?;
    println!(
        "Updated to {}. Restart Relay to use it.",
        found.manifest.version
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_crypto::update::{Artifact, sign};

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("relay-upd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn publish(site: &Path, secret: &[u8; 32], version: Version, payload: &[u8], tamper: bool) {
        std::fs::write(site.join("relay-new"), payload).unwrap();
        let m = Manifest {
            version,
            artifacts: vec![Artifact {
                platform: platform(),
                url: format!("file://{}", site.join("relay-new").display()),
                size: payload.len() as u64,
                blake3: *blake3::hash(payload).as_bytes(),
            }],
        };
        let text = m.render();
        std::fs::write(site.join("manifest.sig"), sign(&text, secret)).unwrap();
        let text = if tamper {
            text.replace("relay-new", "relay-evil")
        } else {
            text
        };
        std::fs::write(site.join("manifest"), text).unwrap();
    }

    #[test]
    fn signed_update_applies_and_tampering_is_refused() {
        let (site, install, tmp) = (dir("site"), dir("inst"), dir("tmp"));
        let secret = [5u8; 32];
        let key = ed25519_dalek::SigningKey::from_bytes(&secret)
            .verifying_key()
            .to_bytes();
        let base = format!("file://{}", site.display());
        let exe = install.join("relay");
        std::fs::write(&exe, b"old binary").unwrap();

        let newer = Version(current().0, current().1, current().2 + 1);
        publish(&site, &secret, newer, b"new binary", false);
        let f = check(&base, &key, &tmp)
            .unwrap()
            .expect("newer version found");
        apply(&f, &exe, &tmp).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new binary");
        assert_eq!(
            std::fs::read(exe.with_extension("old")).unwrap(),
            b"old binary"
        );

        publish(&site, &secret, newer, b"new binary", true);
        assert!(
            check(&base, &key, &tmp).is_err(),
            "tampered manifest refused"
        );

        publish(&site, &secret, current(), b"same", false);
        assert!(
            check(&base, &key, &tmp).unwrap().is_none(),
            "no downgrade / same version"
        );

        publish(&site, &[6u8; 32], newer, b"x", false);
        assert!(check(&base, &key, &tmp).is_err(), "signed by someone else");
    }

    #[test]
    fn corrupt_download_is_refused() {
        let (site, install, tmp) = (dir("site2"), dir("inst2"), dir("tmp2"));
        let secret = [5u8; 32];
        let key = ed25519_dalek::SigningKey::from_bytes(&secret)
            .verifying_key()
            .to_bytes();
        let newer = Version(current().0 + 1, 0, 0);
        publish(&site, &secret, newer, b"new binary", false);
        std::fs::write(site.join("relay-new"), b"NEW binary").unwrap(); // swapped after signing
        let exe = install.join("relay");
        std::fs::write(&exe, b"old binary").unwrap();
        let f = check(&format!("file://{}", site.display()), &key, &tmp)
            .unwrap()
            .unwrap();
        assert!(apply(&f, &exe, &tmp).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old binary", "untouched");
    }
}

//! Unattended access configuration (FR-31, FR-33, FR-38).
//!
//! Lives in a system folder only root / SYSTEM can read:
//!
//! | OS | Folder |
//! |---|---|
//! | macOS | `/Library/Application Support/Relay` |
//! | Windows | `%ProgramData%\Relay` (secrets sealed with machine DPAPI) |
//!
//! It holds the device key the agent registers with, and
//! `unattended.conf`: the permanent password's OPAQUE verifier (never the
//! password) and the optional allow-list of Controller Device IDs.
//! Deleting the file disables unattended access at once: a running agent
//! checks for it every few seconds and exits.
//!
//! `RELAY_SYSTEM_DIR` overrides the folder (tests, experiments).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use relay_crypto::keystore::{FileKeystore, Keystore};
use relay_crypto::pake::{HostPassword, PasswordKind};

pub fn system_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("RELAY_SYSTEM_DIR") {
        return PathBuf::from(d);
    }
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/Relay")
    } else if cfg!(windows) {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("Relay")
    } else {
        PathBuf::from("/var/lib/relay")
    }
}

fn config_path() -> PathBuf {
    config_path_in(&system_dir())
}

fn config_path_in(dir: &Path) -> PathBuf {
    dir.join("unattended.conf")
}

/// Keystore for the agent's device key.
pub fn keystore() -> Box<dyn Keystore> {
    #[cfg(windows)]
    {
        fn seal(b: &[u8]) -> Result<Vec<u8>, relay_crypto::CryptoError> {
            relay_service::windows::dpapi_seal(b).map_err(relay_crypto::CryptoError::Keystore)
        }
        fn open(b: &[u8]) -> Result<Vec<u8>, relay_crypto::CryptoError> {
            relay_service::windows::dpapi_open(b).map_err(relay_crypto::CryptoError::Keystore)
        }
        Box::new(FileKeystore::with_sealing(system_dir(), seal, open))
    }
    #[cfg(not(windows))]
    Box::new(FileKeystore::new(system_dir()))
}

/// Loaded configuration.
pub struct Config {
    pub verifier: HostPassword,
    /// Controller Device IDs allowed in; empty = any with the password.
    pub allow: Vec<u32>,
}

impl Config {
    pub fn allows(&self, id: Option<relay_crypto::DeviceId>) -> bool {
        self.allow.is_empty() || id.is_some_and(|i| self.allow.contains(&i.as_u32()))
    }
}

pub fn load() -> anyhow::Result<Option<Config>> {
    load_from(&system_dir())
}

fn load_from(dir: &Path) -> anyhow::Result<Option<Config>> {
    let text = match std::fs::read(config_path_in(dir)) {
        Ok(b) => {
            let b = keystore_open(&b)?;
            String::from_utf8(b).context("unattended.conf is corrupt")?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading unattended.conf (run as root / SYSTEM?)"),
    };
    let mut lines = text.lines();
    if lines.next() != Some("relay-unattended v1") {
        bail!("unattended.conf: unknown format");
    }
    let (mut allow, mut verifier) = (Vec::new(), None);
    for l in lines {
        if let Some(ids) = l.strip_prefix("allow ") {
            allow = ids
                .split(',')
                .filter_map(|i| i.trim().parse().ok())
                .collect();
        } else if let Some(h) = l.strip_prefix("verifier ") {
            let bytes: Vec<u8> = (0..h.len() / 2)
                .filter_map(|i| u8::from_str_radix(h.get(i * 2..i * 2 + 2)?, 16).ok())
                .collect();
            verifier = Some(
                HostPassword::from_bytes(&bytes).map_err(|e| anyhow::anyhow!("verifier: {e}"))?,
            );
        }
    }
    Ok(Some(Config {
        verifier: verifier.context("unattended.conf has no verifier")?,
        allow,
    }))
}

#[cfg(windows)]
fn keystore_seal(b: &[u8]) -> anyhow::Result<Vec<u8>> {
    relay_service::windows::dpapi_seal(b).map_err(anyhow::Error::msg)
}
#[cfg(windows)]
fn keystore_open(b: &[u8]) -> anyhow::Result<Vec<u8>> {
    relay_service::windows::dpapi_open(b).map_err(anyhow::Error::msg)
}
#[cfg(not(windows))]
fn keystore_seal(b: &[u8]) -> anyhow::Result<Vec<u8>> {
    Ok(b.to_vec())
}
#[cfg(not(windows))]
fn keystore_open(b: &[u8]) -> anyhow::Result<Vec<u8>> {
    Ok(b.to_vec())
}

pub fn save(password: &str, allow: &[u32]) -> anyhow::Result<()> {
    save_to(&system_dir(), password, allow)
}

fn save_to(dir: &Path, password: &str, allow: &[u32]) -> anyhow::Result<()> {
    if !relay_crypto::password::permanent_acceptable(password) {
        bail!(
            "password too weak: at least {} characters, mixed",
            relay_crypto::password::PERMANENT_MIN_LEN
        );
    }
    let v = HostPassword::new(password, PasswordKind::Permanent)?;
    let text = format!(
        "relay-unattended v1\nallow {}\nverifier {}\n",
        allow
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(","),
        relay_net::hex(&v.to_bytes())
    );
    write_private(&config_path_in(dir), &keystore_seal(text.as_bytes())?)
}

fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    let tmp = path.with_extension("tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(&tmp)?.write_all(data)?;
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            path.parent().unwrap_or(Path::new(".")),
            std::fs::Permissions::from_mode(0o700),
        );
    }
    Ok(())
}

/// FR-38: the permanent password stops working immediately.
pub fn remove() -> anyhow::Result<()> {
    remove_in(&system_dir())
}

fn remove_in(dir: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(config_path_in(dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn enabled() -> bool {
    config_path().exists()
}

// ------------------------------------------------------------ CLI

fn admin_or_override() -> anyhow::Result<()> {
    if std::env::var_os("RELAY_SYSTEM_DIR").is_none() && !relay_service::is_admin() {
        bail!(
            "this needs administrator rights: {}",
            if cfg!(windows) {
                "run it from an elevated terminal"
            } else {
                "run it with sudo"
            }
        );
    }
    Ok(())
}

pub struct SetupArgs {
    pub allow: Vec<String>,
    pub no_service: bool,
    pub dry_run: bool,
    pub password: Option<String>,
}

/// FR-31 wizard (terminal form).
pub fn setup(a: SetupArgs) -> anyhow::Result<()> {
    let bin = relay_service::current_binary()?;
    let allow: Vec<u32> = a
        .allow
        .iter()
        .map(|s| {
            relay_crypto::DeviceId::parse(s)
                .map(|d| d.as_u32())
                .with_context(|| format!("not a Device ID: {s}"))
        })
        .collect::<anyhow::Result<_>>()?;
    if a.dry_run {
        println!(
            "Would store a permanent-password verifier in {}",
            config_path().display()
        );
        if allow.is_empty() {
            println!("Would allow any device that knows the password.");
        } else {
            println!("Would allow only: {}", a.allow.join(", "));
        }
        if !a.no_service {
            for step in relay_service::install_plan(&bin) {
                println!("Would {step}");
            }
        }
        return Ok(());
    }
    admin_or_override()?;

    // RELAY_UNATTENDED_PASSWORD: scripted deployment (MDM, provisioning).
    let password = match a
        .password
        .or_else(|| std::env::var("RELAY_UNATTENDED_PASSWORD").ok())
    {
        Some(p) => p,
        None => loop {
            let p = rpassword::prompt_password("Permanent password (12+ characters): ")?;
            let strength = relay_crypto::password::permanent_strength(&p);
            println!("  strength: {strength:?}");
            if !relay_crypto::password::permanent_acceptable(&p) {
                println!("  Too weak; use a longer passphrase with mixed characters.");
                continue;
            }
            if rpassword::prompt_password("Repeat it: ")? != p {
                println!("  They do not match.");
                continue;
            }
            break p;
        },
    };
    save(&password, &allow)?;
    // The agent's identity (and so its Device ID) is created once and kept.
    relay_crypto::keystore::load_or_create_device_key(keystore().as_ref())?;
    println!(
        "Permanent password set. Allowed devices: {}",
        if allow.is_empty() {
            "any".into()
        } else {
            a.allow.join(", ")
        }
    );
    if !a.no_service {
        relay_service::install(&bin)?;
        println!("Service installed and started.");
    }
    if cfg!(target_os = "macos") {
        println!(
            "Next, in System Settings → Privacy & Security, allow {} under:\n  \
             • Screen & System Audio Recording (to see the screen)\n  \
             • Accessibility (to control it)\n  \
             • Full Disk Access (optional, for file transfer)\n\
             The FileVault unlock screen at power-on cannot be reached remotely.",
            bin.display()
        );
    }
    crate::logs::set_data_dir(system_dir());
    crate::logs::audit("UNATTENDED ACCESS ENABLED");
    Ok(())
}

pub fn disable() -> anyhow::Result<()> {
    admin_or_override()?;
    remove()?;
    if std::env::var_os("RELAY_SYSTEM_DIR").is_none() {
        relay_service::uninstall()?;
    }
    crate::logs::set_data_dir(system_dir());
    crate::logs::audit("UNATTENDED ACCESS DISABLED");
    println!("Unattended access disabled; the permanent password no longer works.");
    Ok(())
}

pub fn status() -> anyhow::Result<()> {
    let st = relay_service::status();
    println!(
        "Unattended access: {}",
        if enabled() { "enabled" } else { "disabled" }
    );
    println!(
        "Service: {} ({})",
        if st.running {
            "running"
        } else if st.installed {
            "installed, not running"
        } else {
            "not installed"
        },
        st.detail
    );
    match load() {
        Ok(Some(c)) if c.allow.is_empty() => println!("Allowed devices: any with the password"),
        Ok(Some(c)) => println!(
            "Allowed devices: {}",
            c.allow
                .iter()
                .filter_map(|i| relay_crypto::DeviceId::from_u32(*i))
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Ok(None) => {}
        Err(e) => println!("(cannot read the configuration: {e:#})"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_remove() {
        // Its own folder: no process-wide environment changes, so it cannot
        // race other tests.
        let d = std::env::temp_dir().join(format!("relay-unattended-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(load_from(&d).unwrap().is_none());
        assert!(save_to(&d, "short", &[]).is_err(), "weak passwords refused");
        save_to(&d, "correct-horse-battery-staple", &[123_456_789]).unwrap();
        // Bytes, not text: on Windows the file is DPAPI-sealed binary.
        let raw = std::fs::read(config_path_in(&d)).unwrap();
        assert!(
            !raw.windows(13).any(|w| w == b"correct-horse"),
            "no plaintext password"
        );
        let c = load_from(&d).unwrap().unwrap();
        assert_eq!(c.allow, vec![123_456_789]);
        assert!(c.allows(relay_crypto::DeviceId::from_u32(123_456_789)));
        assert!(!c.allows(relay_crypto::DeviceId::from_u32(987_654_321)));
        assert!(
            !c.allows(None),
            "unregistered controllers are not on the list"
        );
        remove_in(&d).unwrap();
        assert!(!config_path_in(&d).exists());
    }
}

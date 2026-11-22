//! User settings (FR-41), in `settings.conf` in the user's Relay folder:
//! `key = value` lines, edited with `relay config`.
//!
//! | Key | Values | Default |
//! |---|---|---|
//! | `server` | rendezvous host[:port] | build-time `RELAY_DEFAULT_SERVER` |
//! | `server_key` | 64 hex chars | build-time `RELAY_DEFAULT_SERVER_KEY` |
//! | `quality` | `speed` / `balanced` / `quality` | `speed` |
//! | `clipboard` | `true` / `false` | `true` |
//! | `cmd_ctrl_swap` | `true` / `false` | `true` |
//! | `start_with_os` | `true` / `false` (host at login) | `false` |
//! | `language` | `en` | `en` |
//! | `update_url` | base URL of signed update manifests | build-time `RELAY_UPDATE_URL` |
//!
//! Command-line flags and `RELAY_*` environment variables override these.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, bail};

pub const KEYS: [&str; 8] = [
    "server",
    "server_key",
    "quality",
    "clipboard",
    "cmd_ctrl_swap",
    "start_with_os",
    "language",
    "update_url",
];

fn default(key: &str) -> Option<&'static str> {
    match key {
        "server" => option_env!("RELAY_DEFAULT_SERVER"),
        "server_key" => option_env!("RELAY_DEFAULT_SERVER_KEY"),
        "update_url" => option_env!("RELAY_UPDATE_URL"),
        "quality" => Some("speed"),
        "clipboard" | "cmd_ctrl_swap" => Some("true"),
        "start_with_os" => Some("false"),
        "language" => Some("en"),
        _ => None,
    }
}

fn path() -> PathBuf {
    crate::logs::user_dir().join("settings.conf")
}

#[derive(Debug, Default, Clone)]
pub struct Settings(BTreeMap<String, String>);

impl Settings {
    pub fn load() -> Self {
        let text = std::fs::read_to_string(path()).unwrap_or_default();
        Self(
            text.lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .filter(|(k, _)| KEYS.contains(&k.as_str()))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.0
            .get(key)
            .cloned()
            .or_else(|| default(key).map(str::to_string))
            .filter(|v| !v.is_empty())
    }

    pub fn flag(&self, key: &str) -> bool {
        self.get(key).is_some_and(|v| v == "true")
    }

    fn save(&self) -> anyhow::Result<()> {
        let p = path();
        std::fs::create_dir_all(p.parent().context("no settings folder")?)?;
        let body: String = self.0.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
        std::fs::write(
            &p,
            format!("# Relay settings (relay config set KEY VALUE)\n{body}"),
        )?;
        Ok(())
    }
}

fn validate(key: &str, value: &str) -> anyhow::Result<()> {
    let boolean = || matches!(value, "true" | "false");
    let ok = match key {
        "server" => crate::rdv::parse_server(value).is_ok(),
        "server_key" => relay_net::parse_hex32(value).is_some(),
        "quality" => matches!(value, "speed" | "balanced" | "quality"),
        "clipboard" | "cmd_ctrl_swap" | "start_with_os" => boolean(),
        "language" => value == "en", // only English ships in v1
        "update_url" => value.starts_with("https://") || value.starts_with("file://"),
        _ => bail!("unknown setting {key}; known: {}", KEYS.join(", ")),
    };
    if !ok {
        bail!("invalid value for {key}: {value}");
    }
    Ok(())
}

pub fn list() {
    let s = Settings::load();
    for k in KEYS {
        let v = s.get(k).unwrap_or_else(|| "(not set)".into());
        let src = if s.0.contains_key(k) {
            ""
        } else {
            "  (default)"
        };
        println!("{k} = {v}{src}");
    }
    println!("# file: {}", path().display());
}

pub fn set(key: &str, value: &str) -> anyhow::Result<()> {
    validate(key, value)?;
    let mut s = Settings::load();
    if key == "start_with_os" {
        start_with_os(value == "true")?;
    }
    s.0.insert(key.into(), value.into());
    s.save()
}

pub fn unset(key: &str) -> anyhow::Result<()> {
    if !KEYS.contains(&key) {
        bail!("unknown setting {key}");
    }
    let mut s = Settings::load();
    if key == "start_with_os" {
        start_with_os(false)?;
    }
    s.0.remove(key);
    s.save()
}

/// Start `relay host` at login (FR-41): a per-user LaunchAgent on macOS,
/// the `Run` key on Windows.
fn start_with_os(on: bool) -> anyhow::Result<()> {
    let bin = std::env::current_exe()?;
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").context("no HOME")?;
        let plist = PathBuf::from(home).join("Library/LaunchAgents/com.relay.host.plist");
        if on {
            std::fs::create_dir_all(plist.parent().context("no folder")?)?;
            std::fs::write(
                &plist,
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.relay.host</string>
    <key>ProgramArguments</key><array><string>{}</string><string>host</string></array>
    <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
                    bin.display()
                        .to_string()
                        .replace('&', "&amp;")
                        .replace('<', "&lt;")
                ),
            )?;
        } else {
            let _ = std::fs::remove_file(&plist);
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let args: Vec<String> = if on {
            vec![
                "add".into(),
                key.into(),
                "/v".into(),
                "Relay".into(),
                "/d".into(),
                format!("\"{}\" host", bin.display()),
                "/f".into(),
            ]
        } else {
            vec![
                "delete".into(),
                key.into(),
                "/v".into(),
                "Relay".into(),
                "/f".into(),
            ]
        };
        let st = std::process::Command::new("reg").args(&args).status()?;
        if on && !st.success() {
            bail!("could not register Relay to start with Windows");
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (on, bin);
        bail!("start_with_os is not supported on this OS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        assert!(validate("quality", "balanced").is_ok());
        assert!(validate("quality", "turbo").is_err());
        assert!(validate("clipboard", "yes").is_err());
        assert!(validate("server_key", &"ab".repeat(32)).is_ok());
        assert!(validate("server_key", "abc").is_err());
        assert!(
            validate("update_url", "http://insecure.example").is_err(),
            "HTTPS only"
        );
        assert!(validate("language", "fr").is_err());
        assert!(validate("nope", "1").is_err());
    }

    #[test]
    fn defaults() {
        let s = Settings::default();
        assert_eq!(s.get("quality").as_deref(), Some("speed"));
        assert!(s.flag("clipboard"));
        assert!(!s.flag("start_with_os"));
    }
}

//! macOS: LaunchDaemon supervisor + LoginWindow LaunchAgent (FR-32).

#![allow(unsafe_code)]

use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

use crate::{ServiceError, Status, agent_args};

pub const DAEMON_LABEL: &str = "com.relay.service";
pub const LOGINWINDOW_LABEL: &str = "com.relay.loginwindow";
pub const DAEMON_PLIST: &str = "/Library/LaunchDaemons/com.relay.service.plist";
pub const LOGINWINDOW_PLIST: &str = "/Library/LaunchAgents/com.relay.loginwindow.plist";

unsafe extern "C" {
    fn geteuid() -> u32;
}

pub fn is_root() -> bool {
    // SAFETY: plain libc getter.
    unsafe { geteuid() == 0 }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn plist(label: &str, args: &[String], extra: &str) -> String {
    let args: String = args
        .iter()
        .map(|a| format!("        <string>{}</string>\n", xml_escape(a)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
{extra}</dict>
</plist>
"#
    )
}

/// The root LaunchDaemon: `relay service run`.
pub fn daemon_plist(bin: &Path) -> String {
    plist(
        DAEMON_LABEL,
        &[bin.display().to_string(), "service".into(), "run".into()],
        "",
    )
}

/// The login-window agent: runs only while the login window is up.
pub fn loginwindow_plist(bin: &Path) -> String {
    let mut args = vec![bin.display().to_string()];
    args.extend(agent_args());
    plist(
        LOGINWINDOW_LABEL,
        &args,
        "    <key>LimitLoadToSessionType</key>\n    <string>LoginWindow</string>\n",
    )
}

pub fn plan(bin: &Path) -> Vec<String> {
    vec![
        format!(
            "write {DAEMON_PLIST} (root daemon: `{} service run`)",
            bin.display()
        ),
        format!("write {LOGINWINDOW_PLIST} (agent at the login window)"),
        format!("launchctl bootstrap system {DAEMON_PLIST}"),
        format!(
            "launchctl bootstrap loginwindow {LOGINWINDOW_PLIST} (takes effect at the next login window)"
        ),
        "then grant Screen Recording, Accessibility and Full Disk Access to the relay binary \
         in System Settings → Privacy & Security"
            .into(),
    ]
}

fn launchctl(args: &[&str]) -> Result<(), ServiceError> {
    let out = Command::new("/bin/launchctl").args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ServiceError::Platform(format!(
            "launchctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

pub fn install(bin: &Path) -> Result<(), ServiceError> {
    std::fs::write(DAEMON_PLIST, daemon_plist(bin))?;
    std::fs::write(LOGINWINDOW_PLIST, loginwindow_plist(bin))?;
    let _ = launchctl(&["bootout", &format!("system/{DAEMON_LABEL}")]);
    launchctl(&["bootstrap", "system", DAEMON_PLIST])?;
    // Loaded by the login window itself next time it appears.
    let _ = launchctl(&["bootstrap", "loginwindow", LOGINWINDOW_PLIST]);
    Ok(())
}

pub fn uninstall() -> Result<(), ServiceError> {
    let _ = launchctl(&["bootout", &format!("system/{DAEMON_LABEL}")]);
    let _ = launchctl(&["bootout", &format!("loginwindow/{LOGINWINDOW_LABEL}")]);
    for p in [DAEMON_PLIST, LOGINWINDOW_PLIST] {
        match std::fs::remove_file(p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn status() -> Status {
    let installed = Path::new(DAEMON_PLIST).exists();
    let running = Command::new("/bin/launchctl")
        .args(["print", &format!("system/{DAEMON_LABEL}")])
        .output()
        .is_ok_and(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).contains("state = running")
        });
    Status {
        installed,
        running,
        detail: if installed {
            DAEMON_PLIST.into()
        } else {
            "not installed".into()
        },
    }
}

/// UID owning the console (0 = login window, none logged in).
pub fn console_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/dev/console").ok().map(|m| m.uid())
}

/// Keep one agent in the console user's GUI session.
pub fn supervise(bin: &Path) -> Result<(), ServiceError> {
    let mut child: Option<(u32, Child)> = None;
    loop {
        let uid = console_uid().unwrap_or(0);
        let alive = child
            .as_mut()
            .is_some_and(|(_, c)| matches!(c.try_wait(), Ok(None)));
        let stale = child.as_ref().is_some_and(|(u, _)| *u != uid);
        if stale || !alive {
            if let Some((_, mut c)) = child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            // At the login window the LoginWindow LaunchAgent serves.
            if uid != 0 {
                let mut cmd = Command::new("/bin/launchctl");
                cmd.args(["asuser", &uid.to_string(), &bin.display().to_string()])
                    .args(agent_args());
                match cmd.spawn() {
                    Ok(c) => {
                        tracing::info!("agent started for uid {uid}");
                        child = Some((uid, c));
                    }
                    Err(e) => tracing::error!("cannot start agent: {e}"),
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

pub fn reboot(safe_mode: bool) -> Result<(), ServiceError> {
    if safe_mode {
        // Safe boot on macOS needs NVRAM changes that Apple silicon does not
        // allow from software; only a normal reboot is offered.
        return Err(ServiceError::Unsupported);
    }
    let out = Command::new("/sbin/shutdown")
        .args(["-r", "now"])
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ServiceError::Platform(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plists() {
        let bin = Path::new("/Applications/Relay.app/Contents/MacOS/relay");
        let d = daemon_plist(bin);
        assert!(d.contains("<string>com.relay.service</string>"));
        assert!(d.contains("<string>service</string>") && d.contains("<string>run</string>"));
        let l = loginwindow_plist(bin);
        assert!(l.contains("<string>LoginWindow</string>"));
        assert!(l.contains("<string>--unattended</string>"));
        assert!(plist("x", &["a&b<c>".into()], "").contains("a&amp;b&lt;c&gt;"));
        assert!(!plan(bin).is_empty());
    }
}

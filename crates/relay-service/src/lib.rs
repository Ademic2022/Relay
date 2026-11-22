//! Privileged background component for unattended access (M7, FR-31/32/34/38).
//!
//! The **service** (Windows Service as LocalSystem / macOS LaunchDaemon as
//! root) does not touch the network or the screen. It supervises the
//! **agent** — `relay host --unattended` — and keeps exactly one running in
//! the session whose screen should be reachable:
//!
//! - **Windows**: the agent runs as SYSTEM inside the active console session
//!   (`WTSGetActiveConsoleSessionId` + `CreateProcessAsUserW` with the
//!   service's own token re-targeted to that session), so it can follow the
//!   input desktop onto Winlogon / UAC (§10). It is relaunched when the
//!   console session changes (user switch, logon, logoff) or it exits. The
//!   service also performs Ctrl+Alt+Del (`SendSAS`) for the agent over a
//!   SYSTEM-only named pipe, and reboots (optionally into Safe Mode with
//!   networking).
//! - **macOS**: a LaunchAgent limited to the `LoginWindow` session runs the
//!   agent at the login window; once someone logs in, the daemon starts
//!   the agent in that user's GUI session with `launchctl asuser`. The
//!   FileVault pre-boot screen runs before macOS and is out of reach.
//!
//! Installation needs an administrator and is driven by
//! `relay unattended setup`.

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

pub const SERVICE_NAME: &str = "RelayService";

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("administrator privileges required")]
    NotAdmin,
    #[error("not supported on this OS")]
    Unsupported,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Platform(String),
}

/// Where the agent must run to see the current screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTarget {
    /// Nobody logged in: login screen / login window.
    LoginScreen,
    /// A user session (Windows session ID or macOS UID).
    UserSession(u32),
    /// UAC / Ctrl+Alt+Del secure desktop (Windows only).
    SecureDesktop,
}

/// Installation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub installed: bool,
    pub running: bool,
    pub detail: String,
}

/// The agent command line, relative to the installed binary.
pub fn agent_args() -> Vec<String> {
    vec!["host".into(), "--unattended".into()]
}

/// Steps `install` would take, for `--dry-run` and for the setup wizard.
pub fn install_plan(bin: &Path) -> Vec<String> {
    #[cfg(target_os = "macos")]
    return macos::plan(bin);
    #[cfg(target_os = "windows")]
    return windows::plan(bin);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = bin;
        vec!["unattended access is not supported on this OS".into()]
    }
}

/// Is this process running with administrator / root rights?
pub fn is_admin() -> bool {
    #[cfg(target_os = "macos")]
    return macos::is_root();
    #[cfg(target_os = "windows")]
    return windows::is_elevated();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    false
}

pub fn install(bin: &Path) -> Result<(), ServiceError> {
    if !is_admin() {
        return Err(ServiceError::NotAdmin);
    }
    #[cfg(target_os = "macos")]
    return macos::install(bin);
    #[cfg(target_os = "windows")]
    return windows::install(bin);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = bin;
        Err(ServiceError::Unsupported)
    }
}

/// Remove the service (FR-38). The caller also deletes the permanent
/// password, which invalidates it immediately.
pub fn uninstall() -> Result<(), ServiceError> {
    if !is_admin() {
        return Err(ServiceError::NotAdmin);
    }
    #[cfg(target_os = "macos")]
    return macos::uninstall();
    #[cfg(target_os = "windows")]
    return windows::uninstall();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Err(ServiceError::Unsupported)
}

pub fn status() -> Status {
    #[cfg(target_os = "macos")]
    return macos::status();
    #[cfg(target_os = "windows")]
    return windows::status();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Status {
        installed: false,
        running: false,
        detail: "unsupported OS".into(),
    }
}

/// The service's main loop (`relay service run`). Returns when stopped.
pub fn run(bin: &Path) -> Result<(), ServiceError> {
    #[cfg(target_os = "macos")]
    return macos::supervise(bin);
    #[cfg(target_os = "windows")]
    return windows::run_service(bin);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = bin;
        Err(ServiceError::Unsupported)
    }
}

/// Is a user logged in at the console (so the banner must show, FR-33)?
pub fn console_user_present() -> bool {
    #[cfg(target_os = "macos")]
    return macos::console_uid().is_some_and(|u| u != 0);
    #[cfg(target_os = "windows")]
    return windows::console_user().is_some();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    true
}

/// Reboot now, optionally into Safe Mode with networking (FR-34, Windows).
pub fn reboot(safe_mode: bool) -> Result<(), ServiceError> {
    #[cfg(target_os = "macos")]
    return macos::reboot(safe_mode);
    #[cfg(target_os = "windows")]
    return windows::reboot(safe_mode);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = safe_mode;
        Err(ServiceError::Unsupported)
    }
}

/// Ctrl+Alt+Del (FR-14): Windows only, performed by the service.
pub fn send_sas() -> Result<(), ServiceError> {
    #[cfg(target_os = "windows")]
    return windows::request_sas();
    #[cfg(not(target_os = "windows"))]
    Err(ServiceError::Unsupported)
}

/// The running executable, for registering it with the OS.
pub fn current_binary() -> Result<PathBuf, ServiceError> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

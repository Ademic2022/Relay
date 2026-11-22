//! Privileged background component for unattended access (M7, FR-31/32).
//!
//! - Windows: `RelayService` runs as LocalSystem in session 0, keeps the
//!   rendezvous registration alive, and launches the Agent into the active
//!   console session (`WTSGetActiveConsoleSessionId` + `CreateProcessAsUser`),
//!   moving it to the Winlogon desktop for login and UAC screens.
//! - macOS: a LaunchDaemon keeps the network registration alive; a
//!   LaunchAgent with `LimitLoadToSessionType = LoginWindow` serves the
//!   login window; a per-user LaunchAgent serves logged-in sessions.
//!   The FileVault pre-boot screen is out of reach.

/// Where the Agent must run to see the current screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTarget {
    /// Nobody logged in: login screen / login window.
    LoginScreen,
    /// A user session (Windows session ID or macOS UID).
    UserSession(u32),
    /// UAC / Ctrl+Alt+Del secure desktop (Windows only).
    SecureDesktop,
}

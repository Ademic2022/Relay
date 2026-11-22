//! Accept / Decline prompt for attended sessions (FR-8).
//!
//! macOS shows a native dialog; elsewhere the terminal asks. Either way it
//! declines on its own after [`CONSENT_TIMEOUT`]. The Tauri UI replaces
//! both later; the rule (no session without a yes) stays here.

use std::time::Duration;

pub use relay_session::host::CONSENT_TIMEOUT;

/// Keep only characters that are safe to show in a dialog/script string.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_' | ':' | '(' | ')'))
        .take(64)
        .collect()
}

/// Ask the local user. `true` only on an explicit Accept.
pub async fn ask(who: &str) -> bool {
    let who = sanitize(who);
    let res = tokio::time::timeout(CONSENT_TIMEOUT + Duration::from_secs(1), prompt(&who)).await;
    matches!(res, Ok(true))
}

#[cfg(target_os = "macos")]
async fn prompt(who: &str) -> bool {
    let script = format!(
        "display dialog \"{who} wants to view and control this computer.\" \
         with title \"Relay\" buttons {{\"Decline\", \"Accept\"}} default button \"Decline\" \
         with icon caution giving up after {}",
        CONSENT_TIMEOUT.as_secs()
    );
    println!("  ? Connection request from {who}: answer the dialog (auto-declines in 30 s).");
    match tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .await
    {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            s.contains("button returned:Accept") && !s.contains("gave up:true")
        }
        Err(e) => {
            tracing::warn!("consent dialog unavailable ({e}); asking in the terminal");
            terminal(who).await
        }
    }
}

#[cfg(not(target_os = "macos"))]
async fn prompt(who: &str) -> bool {
    terminal(who).await
}

async fn terminal(who: &str) -> bool {
    use tokio::io::AsyncBufReadExt;
    println!(
        "  ? {who} wants to view and control this computer. Accept? [y/N] (auto-declines in 30 s)"
    );
    let mut line = String::new();
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    matches!(stdin.read_line(&mut line).await, Ok(n) if n > 0)
        && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_blocks_script_injection() {
        let evil = "bob\" & do shell script \"rm -rf ~\" & \"";
        let s = sanitize(evil);
        assert!(!s.contains('"') && !s.contains('&'));
    }
}

//! Machine interface for the Relay app (`relay host --ui`).
//!
//! The Tauri app runs `relay host --ui` as a child process: windows that
//! need the main thread (the banner) stay in this process, and the app
//! talks to it over pipes.
//!
//! - **Events** (stdout): lines `@@relay {json}`; other lines are for humans
//!   and the app ignores them.
//! - **Commands** (stdin): one JSON object per line, `{"cmd": "accept"}`,
//!   `decline`, `end`, `new_password`, `{"cmd":"chat","text":"…"}`.
//! - When stdin closes (the app quit or crashed) the host exits, so a host
//!   is never left running without its window.
//!
//! In this mode the Accept/Decline question (FR-8) is asked by the app's
//! window instead of an `osascript` dialog or the terminal. The rule is the
//! same: no session without an explicit Accept before the timeout.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::sync::broadcast;

static ON: AtomicBool = AtomicBool::new(false);

pub const PREFIX: &str = "@@relay ";

pub fn enable() {
    ON.store(true, Ordering::Relaxed);
}

pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Send `{"event": name, ...fields}` to the app (no-op outside UI mode).
pub fn emit(name: &str, fields: Value) {
    if !on() {
        return;
    }
    let mut v = json!({ "event": name });
    if let (Some(obj), Value::Object(f)) = (v.as_object_mut(), fields) {
        obj.extend(f);
    }
    println!("{PREFIX}{v}");
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Accept,
    Decline,
    End,
    NewPassword,
    Chat(String),
    /// FR-11: control, clipboard, file transfer for the running session.
    Permissions([bool; 3]),
}

pub fn parse(line: &str) -> Option<Cmd> {
    let v: Value = serde_json::from_str(line).ok()?;
    Some(match v.get("cmd")?.as_str()? {
        "accept" => Cmd::Accept,
        "decline" => Cmd::Decline,
        "end" => Cmd::End,
        "new_password" => Cmd::NewPassword,
        "chat" => Cmd::Chat(v.get("text")?.as_str()?.to_string()),
        "permissions" => Cmd::Permissions([
            v.get("control")?.as_bool()?,
            v.get("clipboard")?.as_bool()?,
            v.get("files")?.as_bool()?,
        ]),
        _ => return None,
    })
}

static CMDS: OnceLock<broadcast::Sender<Cmd>> = OnceLock::new();

/// Subscribe to commands from the app. The first call starts the stdin
/// reader; when stdin closes, the process exits.
pub fn commands() -> broadcast::Receiver<Cmd> {
    CMDS.get_or_init(|| {
        let (tx, _) = broadcast::channel(64);
        let t = tx.clone();
        tokio::spawn(async move {
            let mut r = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(l)) = r.next_line().await {
                match parse(&l) {
                    Some(c) => {
                        let _ = t.send(c);
                    }
                    None => tracing::debug!("ignoring unknown UI command"),
                }
            }
            crate::logs::audit("app closed; host exiting");
            std::process::exit(0);
        });
        tx
    })
    .subscribe()
}

/// FR-8 through the app window: `true` only on an explicit Accept.
pub async fn consent(who: &str) -> bool {
    let mut cmds = commands();
    emit(
        "consent",
        json!({ "name": who, "timeout_s": crate::consent::CONSENT_TIMEOUT.as_secs() }),
    );
    let answer = tokio::time::timeout(crate::consent::CONSENT_TIMEOUT, async {
        loop {
            match cmds.recv().await {
                Ok(Cmd::Accept) => return true,
                Ok(Cmd::Decline) => return false,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    emit("consent_done", json!({ "accepted": answer }));
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(parse(r#"{"cmd":"accept"}"#), Some(Cmd::Accept));
        assert_eq!(
            parse(r#"{"cmd":"permissions","control":false,"clipboard":true,"files":false}"#),
            Some(Cmd::Permissions([false, true, false]))
        );
        assert_eq!(
            parse(r#"{"cmd":"chat","text":"hi"}"#),
            Some(Cmd::Chat("hi".into()))
        );
        assert_eq!(parse(r#"{"cmd":"chat"}"#), None);
        assert_eq!(parse("y"), None, "terminal answers are not app commands");
        assert_eq!(parse(r#"{"cmd":"format_disk"}"#), None);
    }
}

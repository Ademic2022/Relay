//! Relay desktop app (PRD §5.1): the window people use day to day.
//!
//! The engine is the `relay` binary next to this one. The app never touches
//! video: it runs
//!
//! - `relay host --ui` as a long-lived child (events on stdout, commands on
//!   stdin, see `crates/relay-app/src/ui.rs`), and
//! - `relay connect|view … --password-stdin` per outgoing session; that
//!   child opens the native wgpu viewer window itself (§5.1: video must not
//!   go through the webview).
//!
//! Consent (FR-8) is asked in this window: the host child waits for an
//! explicit Accept from here and declines on its own after 30 s.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, State};

const EVENT_PREFIX: &str = "@@relay ";

/// The `relay` engine binary: `RELAY_BIN`, else next to this executable.
fn relay_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("RELAY_BIN") {
        return PathBuf::from(p);
    }
    let name = if cfg!(windows) { "relay.exe" } else { "relay" };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn command() -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut c = Command::new(relay_bin());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// The user's Relay folder (same as the engine's `logs::user_dir`).
fn user_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base =
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", windows)))]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share"));
    base.unwrap_or_else(std::env::temp_dir).join("Relay")
}

/// Last lines of a child's stderr, for a human error message.
type Tail = Arc<Mutex<VecDeque<String>>>;

fn drain(stream: impl std::io::Read + Send + 'static, tail: Option<Tail>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let Some(t) = &tail {
                let mut t = t.lock().expect("poisoned");
                if t.len() == 20 {
                    t.pop_front();
                }
                t.push_back(line);
            }
        }
    });
}

/// Strip ANSI colour codes and log prefixes from an engine error line.
fn clean(line: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in line.chars() {
        match (esc, c) {
            (false, '\u{1b}') => esc = true,
            (true, 'm') => esc = false,
            (true, _) => {}
            (false, c) => out.push(c),
        }
    }
    out.trim().trim_start_matches("Error: ").to_string()
}

/// Short, human message for an engine failure (§16.3).
fn friendly(raw: &str) -> String {
    let r = raw.to_ascii_lowercase();
    if r.contains("wrong password") {
        "Wrong password.".into()
    } else if r.contains("declined") {
        "The other person declined the connection.".into()
    } else if r.contains("too many attempts") {
        "Too many attempts. Wait a minute and try again.".into()
    } else if r.contains("not allowed to connect") {
        "This computer is not allowed to connect to that device.".into()
    } else if r.contains("no answer")
        || r.contains("timed out")
        || r.contains("not online")
        || r.contains("unreachable")
    {
        "Couldn't reach the device. It may be offline.".into()
    } else if r.contains("no rendezvous server") || r.contains("server") && r.contains("not set") {
        "No Relay server is configured. Set one in Settings, or connect by address (IP:port)."
            .into()
    } else if r.contains("screen recording") {
        "Relay needs Screen Recording permission.".into()
    } else if r.contains("key changed") || r.contains("pinned") {
        "Warning: that device's identity key changed. Connection refused.".into()
    } else if raw.is_empty() {
        "Something went wrong.".into()
    } else {
        raw.to_string()
    }
}

struct Host {
    child: Child,
    stdin: ChildStdin,
}

#[derive(Default)]
struct AppState {
    host: Mutex<Option<Host>>,
    viewers: Mutex<Vec<Child>>,
}

impl AppState {
    fn stop_all(&self) {
        if let Some(mut h) = self.host.lock().expect("poisoned").take() {
            drop(h.stdin); // the host exits when its stdin closes
            let _ = h.child.wait();
        }
        for mut v in self.viewers.lock().expect("poisoned").drain(..) {
            let _ = v.kill();
            let _ = v.wait();
        }
    }
}

#[tauri::command]
fn start_host(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let mut slot = state.host.lock().expect("poisoned");
    if slot.is_some() {
        return Ok(());
    }
    let mut child = command()
        .args(["host", "--ui"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start Relay ({}): {e}", relay_bin().display()))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let tail: Tail = Arc::default();
    drain(child.stderr.take().ok_or("no stderr")?, Some(tail.clone()));
    let app2 = app.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Some(json) = line.strip_prefix(EVENT_PREFIX) else {
                continue; // human-readable output
            };
            let Ok(ev) = serde_json::from_str::<Value>(json) else {
                continue;
            };
            if ev.get("event").and_then(Value::as_str) == Some("consent") {
                // FR-8: the question must be seen. Bring the window forward.
                if let Some(w) = app2.get_webview_window("main") {
                    let _ = w.unminimize();
                    let _ = w.show();
                    let _ = w.set_focus();
                    let _ = w.request_user_attention(Some(tauri::UserAttentionType::Critical));
                }
            }
            let _ = app2.emit("host", ev);
        }
        // stdout closed: the host exited.
        let state = app2.state::<AppState>();
        let code = state
            .host
            .lock()
            .expect("poisoned")
            .take()
            .and_then(|mut h| h.child.wait().ok())
            .and_then(|s| s.code());
        let last = tail
            .lock()
            .expect("poisoned")
            .iter()
            .rev()
            .map(|l| clean(l))
            .find(|l| !l.is_empty() && !l.contains(" INFO ") && !l.contains(" DEBUG "))
            .unwrap_or_default();
        let _ = app2.emit(
            "host_exit",
            serde_json::json!({ "code": code, "message": friendly(&last), "raw": last }),
        );
    });
    *slot = Some(Host { child, stdin });
    Ok(())
}

#[tauri::command]
fn stop_host(state: State<'_, AppState>) {
    if let Some(mut h) = state.host.lock().expect("poisoned").take() {
        drop(h.stdin);
        let _ = h.child.wait();
    }
}

/// Commands the window may send to the host (nothing else passes).
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum HostCmd {
    Accept,
    Decline,
    End,
    NewPassword,
    Chat { text: String },
}

#[tauri::command]
fn host_cmd(cmd: HostCmd, state: State<'_, AppState>) -> Result<(), String> {
    let line = match &cmd {
        HostCmd::Accept => serde_json::json!({ "cmd": "accept" }),
        HostCmd::Decline => serde_json::json!({ "cmd": "decline" }),
        HostCmd::End => serde_json::json!({ "cmd": "end" }),
        HostCmd::NewPassword => serde_json::json!({ "cmd": "new_password" }),
        HostCmd::Chat { text } => serde_json::json!({ "cmd": "chat", "text": text }),
    };
    let mut slot = state.host.lock().expect("poisoned");
    let h = slot.as_mut().ok_or("Relay is not running")?;
    writeln!(h.stdin, "{line}")
        .and_then(|()| h.stdin.flush())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Recent {
    target: String,
    last_used: u64,
}

fn recent_path() -> PathBuf {
    user_dir().join("recent.json")
}

fn load_recent() -> Vec<Recent> {
    std::fs::read(recent_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn remember(target: &str) {
    let mut r = load_recent();
    r.retain(|x| x.target != target);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    r.insert(
        0,
        Recent {
            target: target.into(),
            last_used: now,
        },
    );
    r.truncate(10);
    let _ = std::fs::create_dir_all(user_dir());
    if let Ok(b) = serde_json::to_vec_pretty(&r) {
        let _ = std::fs::write(recent_path(), b);
    }
}

#[tauri::command]
fn recent() -> Vec<Recent> {
    load_recent()
}

#[tauri::command]
fn forget_recent(target: String) {
    let mut r = load_recent();
    r.retain(|x| x.target != target);
    if let Ok(b) = serde_json::to_vec_pretty(&r) {
        let _ = std::fs::write(recent_path(), b);
    }
}

/// What the user typed: a 9-digit Device ID (spaces allowed) or an
/// `IP:port` direct address (FR-7).
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Id(String),
    Direct(String),
}

fn parse_target(s: &str) -> Option<Target> {
    let s = s.trim();
    let digits: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() == 9 && digits.chars().all(|c| c.is_ascii_digit()) {
        return Some(Target::Id(digits));
    }
    s.parse::<std::net::SocketAddr>()
        .ok()
        .map(|a| Target::Direct(a.to_string()))
}

#[tauri::command]
fn connect(
    app: AppHandle,
    target: String,
    password: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let t = parse_target(&target)
        .ok_or("Enter a 9-digit Relay ID, or an address like 192.168.1.20:21118.")?;
    if password.trim().is_empty() {
        return Err("Enter the password shown on the other computer.".into());
    }
    let (args, label) = match &t {
        Target::Id(id) => (vec!["connect".to_string(), id.clone()], id.clone()),
        Target::Direct(a) => (vec!["view".to_string(), a.clone()], a.clone()),
    };
    let mut child = command()
        .args(&args)
        .arg("--password-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start Relay: {e}"))?;
    // Password over a pipe, never on the command line.
    if let Some(mut s) = child.stdin.take() {
        writeln!(s, "{}", password.trim()).map_err(|e| e.to_string())?;
    }
    let tail: Tail = Arc::default();
    drain(child.stderr.take().ok_or("no stderr")?, Some(tail.clone()));
    remember(&label);
    let pid = child.id();
    state.viewers.lock().expect("poisoned").push(child);
    let _ = app.emit(
        "viewer",
        serde_json::json!({ "state": "started", "target": label }),
    );
    std::thread::spawn(move || {
        // Poll our own child for exit (it lives in AppState so quitting the
        // app can close it).
        let status = loop {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let state = app.state::<AppState>();
            let mut vs = state.viewers.lock().expect("poisoned");
            let Some(i) = vs.iter().position(|c| c.id() == pid) else {
                break None; // removed by stop_all
            };
            if let Ok(Some(st)) = vs[i].try_wait() {
                // try_wait returned a status: the child is already reaped.
                #[allow(clippy::zombie_processes)]
                vs.remove(i);
                break Some(st);
            }
        };
        let ok = status.is_some_and(|s| s.success());
        let last = tail
            .lock()
            .expect("poisoned")
            .iter()
            .rev()
            .map(|l| clean(l))
            .find(|l| !l.is_empty() && !l.contains(" INFO ") && !l.contains(" DEBUG "))
            .unwrap_or_default();
        let _ = app.emit(
            "viewer",
            serde_json::json!({
                "state": "ended",
                "target": label,
                "ok": ok,
                "message": if ok { String::new() } else { friendly(&last) },
            }),
        );
    });
    Ok(())
}

#[derive(Debug, Serialize)]
struct SettingRow {
    key: String,
    value: String,
    default: bool,
}

fn run_relay(args: &[&str]) -> Result<String, String> {
    let out = command().args(args).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(clean(err.lines().last().unwrap_or("failed")))
    }
}

#[tauri::command]
fn settings() -> Result<Vec<SettingRow>, String> {
    Ok(run_relay(&["config", "list"])?
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once(" = "))
        .map(|(k, v)| {
            let default = v.ends_with("(default)");
            let v = v.trim_end_matches("(default)").trim();
            SettingRow {
                key: k.trim().into(),
                value: if v == "(not set)" {
                    String::new()
                } else {
                    v.into()
                },
                default,
            }
        })
        .collect())
}

#[tauri::command]
fn set_setting(key: String, value: String) -> Result<(), String> {
    if value.trim().is_empty() {
        run_relay(&["config", "unset", &key]).map(drop)
    } else {
        run_relay(&["config", "set", &key, value.trim()]).map(drop)
    }
}

#[tauri::command]
fn audit_log() -> Result<String, String> {
    run_relay(&["audit"])
}

#[tauri::command]
fn open_privacy_settings(pane: String) {
    #[cfg(target_os = "macos")]
    {
        let anchor = match pane.as_str() {
            "accessibility" => "Privacy_Accessibility",
            _ => "Privacy_ScreenCapture",
        };
        let _ = Command::new("open")
            .arg(format!(
                "x-apple.systempreferences:com.apple.preference.security?{anchor}"
            ))
            .spawn();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = pane;
}

#[tauri::command]
fn version() -> String {
    env!("CARGO_PKG_VERSION").into()
}

fn main() {
    let app = tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            start_host,
            stop_host,
            host_cmd,
            connect,
            recent,
            forget_recent,
            settings,
            set_setting,
            audit_log,
            open_privacy_settings,
            version,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the Relay app");
    app.run(|handle, ev| {
        if let tauri::RunEvent::Exit = ev {
            // No host or viewer outlives the app.
            handle.state::<AppState>().stop_all();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("123 456 789"),
            Some(Target::Id("123456789".into()))
        );
        assert_eq!(
            parse_target(" 192.168.1.5:21118 "),
            Some(Target::Direct("192.168.1.5:21118".into()))
        );
        assert_eq!(parse_target("12345"), None);
        assert_eq!(parse_target("example.com; rm -rf /"), None);
    }

    #[test]
    fn errors_are_human() {
        assert_eq!(friendly("wrong password"), "Wrong password.");
        assert_eq!(
            clean("\u{1b}[31mError: no answer from 1.2.3.4:5\u{1b}[0m"),
            "no answer from 1.2.3.4:5"
        );
        assert!(friendly("no answer from x").starts_with("Couldn't reach"));
    }

    #[test]
    fn host_commands_are_closed_set() {
        assert!(serde_json::from_str::<HostCmd>(r#"{"cmd":"accept"}"#).is_ok());
        assert!(serde_json::from_str::<HostCmd>(r#"{"cmd":"chat","text":"hi"}"#).is_ok());
        assert!(serde_json::from_str::<HostCmd>(r#"{"cmd":"shell","text":"x"}"#).is_err());
    }
}

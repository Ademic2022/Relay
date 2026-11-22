//! Two-pane file manager in the viewer window (FR-26, FR-27, FR-28).
//!
//! Left: this computer. Right: the host. Keyboard-driven, drawn with the
//! overlay text renderer:
//!
//! | Key | Action |
//! |---|---|
//! | ↑ ↓ PgUp PgDn Home End | move |
//! | Enter | open folder |
//! | Backspace | parent folder |
//! | Tab | switch pane |
//! | F5 | copy file or folder to the other pane (upload / download) |
//! | F6 | when a name is taken: keep both / overwrite / skip (FR-29) |
//! | F7 | new folder (host) |
//! | F2 | rename (host) |
//! | F8 / Delete | delete (host, asks first) |
//! | F9 | cancel the newest running transfer |
//! | Esc | close (or cancel a prompt) |
//!
//! Local-pane operations other than browsing are left to the OS.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use relay_proto::v1;
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{KeyCode, PhysicalKey};

use crate::files::client::{Progress, remote_join};

/// What the window asks the network side to do.
#[derive(Debug)]
pub enum FileCmd {
    ListRemote(String),
    /// A file or a folder (recursive, FR-29).
    Upload {
        local: PathBuf,
        remote_dir: String,
        conflict: v1::Conflict,
    },
    Download {
        remote: String,
        local_dir: PathBuf,
        is_dir: bool,
        conflict: v1::Conflict,
    },
    Op {
        op: v1::FileOp,
        path: String,
        new_name: String,
    },
}

/// What comes back.
#[derive(Debug)]
pub enum FileEvent {
    Listing(v1::DirListing),
    Started {
        name: String,
        upload: bool,
        progress: Arc<Progress>,
    },
    Finished {
        name: String,
        result: Result<String, String>,
    },
    OpDone(Result<(), String>),
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
}

#[derive(Debug, Default)]
struct Pane {
    path: String,
    entries: Vec<Entry>,
    sel: usize,
}

impl Pane {
    fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.sel)
    }

    fn set(&mut self, path: String, mut entries: Vec<Entry>) {
        entries.insert(
            0,
            Entry {
                name: "..".into(),
                is_dir: true,
                size: 0,
            },
        );
        self.path = path;
        self.entries = entries;
        self.sel = self.sel.min(self.entries.len().saturating_sub(1));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Prompt {
    Mkdir(String),
    Rename(String),
    ConfirmDelete,
}

struct Transfer {
    name: String,
    upload: bool,
    progress: Arc<Progress>,
    status: Option<Result<String, String>>,
}

pub struct FileManager {
    pub open: bool,
    local: Pane,
    remote: Pane,
    focus: Side,
    prompt: Option<Prompt>,
    transfers: Vec<Transfer>,
    message: String,
    /// What a copy does when the name is taken (F6).
    conflict: v1::Conflict,
}

fn conflict_label(c: v1::Conflict) -> &'static str {
    match c {
        v1::Conflict::Rename => "keep both",
        v1::Conflict::Overwrite => "overwrite",
        v1::Conflict::Skip => "skip",
    }
}

const ROWS: usize = 16;
const COL: usize = 40;

pub fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        format!("{s:<w$}")
    } else {
        // Keep the end (file names and the deepest folder matter most).
        let tail: String = s.chars().skip(n - (w - 1)).collect();
        format!("…{tail}")
    }
}

/// Host paths are opaque strings; find the parent by its last separator.
fn remote_parent(p: &str) -> String {
    let t = p.trim_end_matches(['/', '\\']);
    match t.rfind(['/', '\\']) {
        Some(0) => "/".into(),
        Some(i) if t[..i].ends_with(':') => format!("{}\\", &t[..i]), // C:\
        Some(i) => t[..i].to_string(),
        None => p.to_string(),
    }
}

impl FileManager {
    pub fn new() -> Self {
        let mut fm = Self {
            open: false,
            local: Pane::default(),
            remote: Pane::default(),
            focus: Side::Remote,
            prompt: None,
            transfers: Vec::new(),
            message: String::new(),
            conflict: v1::Conflict::Rename,
        };
        let start = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        fm.list_local(&start);
        fm
    }

    pub fn conflict(&self) -> v1::Conflict {
        self.conflict
    }

    /// Host folder that dropped files go to ("" = host's default).
    pub fn remote_dir(&self) -> String {
        self.remote.path.clone()
    }

    fn list_local(&mut self, dir: &Path) {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        match std::fs::read_dir(&dir) {
            Ok(rd) => {
                let mut v: Vec<Entry> = rd
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if name.ends_with(".relaypart") || name.ends_with(".relaypart.meta") {
                            return None;
                        }
                        let md = e.metadata().ok()?;
                        Some(Entry {
                            name,
                            is_dir: md.is_dir(),
                            size: md.len(),
                        })
                    })
                    .collect();
                v.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                self.local.sel = 0;
                self.local.set(dir.to_string_lossy().to_string(), v);
            }
            Err(e) => self.message = format!("{}: {e}", dir.display()),
        }
    }

    pub fn on_event(&mut self, ev: FileEvent) {
        match ev {
            FileEvent::Listing(l) => {
                let v = l
                    .entries
                    .into_iter()
                    .map(|e| Entry {
                        name: e.name,
                        is_dir: e.is_dir,
                        size: e.size,
                    })
                    .collect();
                self.remote.set(l.path, v);
            }
            FileEvent::Started {
                name,
                upload,
                progress,
            } => self.transfers.push(Transfer {
                name,
                upload,
                progress,
                status: None,
            }),
            FileEvent::Finished { name, result } => {
                if let Some(t) = self
                    .transfers
                    .iter_mut()
                    .rev()
                    .find(|t| t.name == name && t.status.is_none())
                {
                    t.status = Some(result.clone());
                }
                self.message = match result {
                    Ok(to) => format!("done: {name} → {to}"),
                    Err(e) => format!("failed: {name}: {e}"),
                };
            }
            FileEvent::OpDone(r) => {
                if let Err(e) = r {
                    self.message = e;
                }
            }
        }
    }

    /// One line summary of running transfers, for the window title.
    pub fn summary(&self) -> Option<String> {
        let running: Vec<&Transfer> = self
            .transfers
            .iter()
            .filter(|t| t.status.is_none())
            .collect();
        let t = running.last()?;
        let (d, n) = (
            t.progress.done.load(Ordering::Relaxed),
            t.progress.total.load(Ordering::Relaxed).max(1),
        );
        Some(format!(
            "{} {}% {}{}",
            if t.upload { "↑" } else { "↓" },
            d * 100 / n,
            t.name,
            if running.len() > 1 {
                format!(" (+{})", running.len() - 1)
            } else {
                String::new()
            }
        ))
    }

    pub fn has_running(&self) -> bool {
        self.transfers.iter().any(|t| t.status.is_none())
    }

    /// Handle a key while open. Returns commands for the network side.
    pub fn key(&mut self, ev: &KeyEvent) -> Vec<FileCmd> {
        let mut out = Vec::new();
        if ev.state != ElementState::Pressed {
            return out;
        }
        let PhysicalKey::Code(code) = ev.physical_key else {
            return out;
        };

        if let Some(p) = self.prompt.clone() {
            match (code, p) {
                (KeyCode::Escape, _) => self.prompt = None,
                (KeyCode::KeyY, Prompt::ConfirmDelete) => {
                    self.prompt = None;
                    if let Some(e) = self.remote.selected().filter(|e| e.name != "..") {
                        out.push(FileCmd::Op {
                            op: v1::FileOp::Delete,
                            path: remote_join(&self.remote.path, &e.name),
                            new_name: String::new(),
                        });
                        out.push(FileCmd::ListRemote(self.remote.path.clone()));
                    }
                }
                (_, Prompt::ConfirmDelete) => self.prompt = None,
                (KeyCode::Enter | KeyCode::NumpadEnter, Prompt::Mkdir(name)) => {
                    self.prompt = None;
                    if !name.trim().is_empty() {
                        out.push(FileCmd::Op {
                            op: v1::FileOp::Mkdir,
                            path: remote_join(&self.remote.path, name.trim()),
                            new_name: String::new(),
                        });
                        out.push(FileCmd::ListRemote(self.remote.path.clone()));
                    }
                }
                (KeyCode::Enter | KeyCode::NumpadEnter, Prompt::Rename(name)) => {
                    self.prompt = None;
                    if let Some(e) = self.remote.selected().filter(|e| e.name != "..")
                        && !name.trim().is_empty()
                    {
                        out.push(FileCmd::Op {
                            op: v1::FileOp::Rename,
                            path: remote_join(&self.remote.path, &e.name),
                            new_name: name.trim().to_string(),
                        });
                        out.push(FileCmd::ListRemote(self.remote.path.clone()));
                    }
                }
                (KeyCode::Backspace, Prompt::Mkdir(mut n) | Prompt::Rename(mut n)) => {
                    n.pop();
                    self.prompt = Some(if matches!(self.prompt, Some(Prompt::Mkdir(_))) {
                        Prompt::Mkdir(n)
                    } else {
                        Prompt::Rename(n)
                    });
                }
                (_, Prompt::Mkdir(mut n) | Prompt::Rename(mut n)) => {
                    if let Some(t) = &ev.text {
                        n.extend(t.chars().filter(|c| !c.is_control()));
                    }
                    self.prompt = Some(if matches!(self.prompt, Some(Prompt::Mkdir(_))) {
                        Prompt::Mkdir(n)
                    } else {
                        Prompt::Rename(n)
                    });
                }
            }
            return out;
        }

        let side = self.focus;
        let pane = match side {
            Side::Local => &mut self.local,
            Side::Remote => &mut self.remote,
        };
        let last = pane.entries.len().saturating_sub(1);
        match code {
            KeyCode::Escape => self.open = false,
            KeyCode::Tab => {
                self.focus = if side == Side::Local {
                    Side::Remote
                } else {
                    Side::Local
                }
            }
            KeyCode::ArrowUp => pane.sel = pane.sel.saturating_sub(1),
            KeyCode::ArrowDown => pane.sel = (pane.sel + 1).min(last),
            KeyCode::PageUp => pane.sel = pane.sel.saturating_sub(ROWS),
            KeyCode::PageDown => pane.sel = (pane.sel + ROWS).min(last),
            KeyCode::Home => pane.sel = 0,
            KeyCode::End => pane.sel = last,
            KeyCode::Backspace => {
                pane.sel = 0;
                match side {
                    Side::Local => {
                        let up = Path::new(&self.local.path).parent().map(Path::to_path_buf);
                        if let Some(up) = up {
                            self.list_local(&up);
                        }
                    }
                    Side::Remote => out.push(FileCmd::ListRemote(remote_parent(&self.remote.path))),
                }
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                if let Some(e) = pane.selected().cloned().filter(|e| e.is_dir) {
                    pane.sel = 0;
                    match side {
                        Side::Local => {
                            let to = if e.name == ".." {
                                Path::new(&self.local.path).parent().map(Path::to_path_buf)
                            } else {
                                Some(Path::new(&self.local.path).join(&e.name))
                            };
                            if let Some(to) = to {
                                self.list_local(&to);
                            }
                        }
                        Side::Remote => out.push(FileCmd::ListRemote(if e.name == ".." {
                            remote_parent(&self.remote.path)
                        } else {
                            remote_join(&self.remote.path, &e.name)
                        })),
                    }
                }
            }
            KeyCode::F5 => {
                let conflict = self.conflict;
                if let Some(e) = pane.selected().filter(|e| e.name != "..").cloned() {
                    match side {
                        Side::Local => out.push(FileCmd::Upload {
                            local: Path::new(&self.local.path).join(&e.name),
                            remote_dir: self.remote.path.clone(),
                            conflict,
                        }),
                        Side::Remote => out.push(FileCmd::Download {
                            remote: remote_join(&self.remote.path, &e.name),
                            local_dir: PathBuf::from(&self.local.path),
                            is_dir: e.is_dir,
                            conflict,
                        }),
                    }
                } else {
                    self.message = "select a file or folder".into();
                }
            }
            KeyCode::F6 => {
                self.conflict = match self.conflict {
                    v1::Conflict::Rename => v1::Conflict::Overwrite,
                    v1::Conflict::Overwrite => v1::Conflict::Skip,
                    v1::Conflict::Skip => v1::Conflict::Rename,
                };
                self.message = format!("if a name is taken: {}", conflict_label(self.conflict));
            }
            KeyCode::F7 if side == Side::Remote => self.prompt = Some(Prompt::Mkdir(String::new())),
            KeyCode::F2 if side == Side::Remote => {
                if let Some(e) = pane.selected().filter(|e| e.name != "..") {
                    self.prompt = Some(Prompt::Rename(e.name.clone()));
                }
            }
            KeyCode::F8 | KeyCode::Delete if side == Side::Remote => {
                if pane.selected().is_some_and(|e| e.name != "..") {
                    self.prompt = Some(Prompt::ConfirmDelete);
                }
            }
            KeyCode::F9 => {
                if let Some(t) = self.transfers.iter().rev().find(|t| t.status.is_none()) {
                    t.progress.cancel.store(true, Ordering::Relaxed);
                }
            }
            _ => {}
        }
        out
    }

    fn pane_lines(&self, side: Side) -> Vec<String> {
        let (p, title) = match side {
            Side::Local => (&self.local, "This computer"),
            Side::Remote => (&self.remote, "Host"),
        };
        let focused = self.focus == side;
        let mut v = vec![
            fit(
                &format!("{}{title}", if focused { "▸ " } else { "  " }),
                COL,
            ),
            fit(&p.path, COL),
            "-".repeat(COL),
        ];
        let start = p
            .sel
            .saturating_sub(ROWS / 2)
            .min(p.entries.len().saturating_sub(ROWS));
        for (i, e) in p.entries.iter().enumerate().skip(start).take(ROWS) {
            let mark = if i == p.sel && focused {
                ">"
            } else if i == p.sel {
                "-"
            } else {
                " "
            };
            let size = if e.is_dir {
                "<dir>".to_string()
            } else {
                human(e.size)
            };
            let name_w = COL - 2 - 10;
            let name = format!(
                "{}{}",
                e.name,
                if e.is_dir && e.name != ".." { "/" } else { "" }
            );
            v.push(format!("{mark} {}{size:>10}", fit(&name, name_w)));
        }
        while v.len() < ROWS + 3 {
            v.push(" ".repeat(COL));
        }
        v
    }

    pub fn lines(&self) -> Vec<String> {
        let (l, r) = (self.pane_lines(Side::Local), self.pane_lines(Side::Remote));
        let mut out: Vec<String> = l
            .iter()
            .zip(&r)
            .map(|(a, b)| format!("{a} | {b}"))
            .collect();
        out.push(String::new());
        for t in self.transfers.iter().rev().take(4) {
            let (d, n) = (
                t.progress.done.load(Ordering::Relaxed),
                t.progress.total.load(Ordering::Relaxed).max(1),
            );
            let pct = (d * 100 / n).min(100) as usize;
            let bar: String = (0..20)
                .map(|i| if i < pct / 5 { '#' } else { '.' })
                .collect();
            let state = match &t.status {
                None => format!("{} / {}", human(d), human(n)),
                Some(Ok(to)) if to.starts_with("skipped") => "skipped".into(),
                Some(Ok(_)) => "done, BLAKE3 verified".into(),
                Some(Err(e)) => format!("failed: {e}"),
            };
            out.push(format!(
                "{} [{bar}] {pct:>3}% {} {state}",
                if t.upload { "↑" } else { "↓" },
                fit(&t.name, 24).trim_end()
            ));
        }
        match &self.prompt {
            Some(Prompt::Mkdir(n)) => out.push(format!("New folder name: {n}_   (Enter / Esc)")),
            Some(Prompt::Rename(n)) => out.push(format!("Rename to: {n}_   (Enter / Esc)")),
            Some(Prompt::ConfirmDelete) => {
                out.push("Delete the selected item on the host? y / n".into())
            }
            None if !self.message.is_empty() => out.push(self.message.clone()),
            None => {}
        }
        out.push(format!(
            "Tab pane  Enter open  Bksp up  F5 copy  F6 if taken: {}  F7 mkdir  F2 rename  F8 delete  F9 cancel  Esc close",
            conflict_label(self.conflict)
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths() {
        assert_eq!(remote_parent("/home/ann/docs"), "/home/ann");
        assert_eq!(remote_parent("/home"), "/");
        assert_eq!(remote_parent("C:\\Users\\ann"), "C:\\Users");
        assert_eq!(remote_parent("C:\\Users"), "C:\\");
        assert_eq!(remote_join("/home/ann", "x.txt"), "/home/ann/x.txt");
        assert_eq!(remote_join("C:\\Users", "x.txt"), "C:\\Users\\x.txt");
        assert_eq!(remote_join("/", "x"), "/x");
    }

    #[test]
    fn renders_two_panes() {
        let mut fm = FileManager::new();
        fm.on_event(FileEvent::Listing(v1::DirListing {
            path: "/home/ann".into(),
            entries: vec![v1::DirEntry {
                name: "notes.txt".into(),
                is_dir: false,
                size: 1500,
                modified_ms: 0,
            }],
            error: String::new(),
        }));
        let lines = fm.lines();
        assert!(lines[0].contains("This computer") && lines[0].contains("Host"));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("notes.txt") && l.contains("1.5 KB"))
        );
        assert_eq!(human(5_000_000_000), "5.0 GB");
    }
}

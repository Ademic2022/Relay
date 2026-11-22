//! Diagnostic and audit logs (FR-39, FR-40).
//!
//! - **Diagnostic log**: everything `tracing` prints, also written to a
//!   rotating file capped at [`DIAG_CAP`] in total (two files). It never
//!   contains screen content, keystrokes or chat text: code logs events,
//!   not payloads.
//! - **Audit log**: one line per connection event on the Host (who, when,
//!   how long, attended or unattended, files received). Append-only from
//!   Relay's side; nothing a Controller can do reaches it (the file
//!   manager refuses Relay's data folder).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Total size of the diagnostic log (current + one rotated file).
pub const DIAG_CAP: u64 = 10 * 1024 * 1024;

/// Per-user data folder (`~/Library/Application Support/Relay`,
/// `%APPDATA%\Relay`, `~/.local/share/relay`). Unattended agents use the
/// system folder instead, see [`crate::unattended::system_dir`].
pub fn user_dir() -> PathBuf {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(windows) {
        env("APPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library").join("Application Support"))
    } else {
        env("XDG_DATA_HOME").or_else(|| env("HOME").map(|h| h.join(".local").join("share")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join(if cfg!(any(windows, target_os = "macos")) {
            "Relay"
        } else {
            "relay"
        })
}

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where this process keeps its logs (set once at start).
pub fn data_dir() -> &'static Path {
    DATA_DIR.get_or_init(user_dir)
}

pub fn set_data_dir(p: PathBuf) {
    let _ = DATA_DIR.set(p);
}

/// Is `p` inside Relay's own data (logs, config)? The file manager refuses
/// these so a Controller cannot read or erase the audit log.
pub fn is_protected(p: &Path) -> bool {
    let canon = |q: &Path| q.canonicalize().unwrap_or_else(|_| q.to_path_buf());
    let target = canon(p);
    [data_dir().to_path_buf(), crate::unattended::system_dir()]
        .iter()
        .map(|d| canon(d))
        .any(|d| target.starts_with(&d))
}

// ------------------------------------------------------------ diagnostic

/// A `tracing` writer that rotates `diag.log` → `diag.1.log`.
pub struct Rotating {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    cap: u64,
}

impl Rotating {
    pub fn open(dir: &Path, cap_total: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join("diag.log");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path,
            file: Some(file),
            written,
            cap: cap_total / 2,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let _ = fs::rename(&self.path, self.path.with_file_name("diag.1.log"));
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        );
        self.written = 0;
        Ok(())
    }
}

impl Write for Rotating {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > self.cap {
            self.rotate()?;
        }
        let n = self.file.as_mut().map_or(Ok(buf.len()), |f| f.write(buf))?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), |f| f.flush())
    }
}

/// Shared handle so `tracing_subscriber` can make writers on demand.
pub struct DiagLog(pub Mutex<Rotating>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for DiagLog {
    type Writer = DiagWriter<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        DiagWriter(&self.0)
    }
}

pub struct DiagWriter<'a>(&'a Mutex<Rotating>);

impl Write for DiagWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().map_or(Ok(buf.len()), |mut r| r.write(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().map_or(Ok(()), |mut r| r.flush())
    }
}

// ------------------------------------------------------------ audit

fn audit_path() -> PathBuf {
    data_dir().join("audit.log")
}

/// Append an audit line, timestamped (UTC, ISO 8601).
pub fn audit(line: &str) {
    let ts = jiff::Timestamp::now();
    let entry = format!("{ts} {}\n", line.replace('\n', " "));
    let write = || -> io::Result<()> {
        fs::create_dir_all(data_dir())?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(audit_path())?
            .write_all(entry.as_bytes())
    };
    if let Err(e) = write() {
        tracing::warn!("audit log: {e}");
    }
    eprint!("  [audit] {entry}");
}

/// The audit log, for `relay audit`.
pub fn read_audit() -> io::Result<String> {
    fs::read_to_string(audit_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_within_cap() {
        let d = std::env::temp_dir().join(format!("relay-logs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let mut r = Rotating::open(&d, 20_000).unwrap();
        for _ in 0..1000 {
            r.write_all(&[b'x'; 99]).unwrap();
            r.write_all(b"\n").unwrap();
        }
        let a = fs::metadata(d.join("diag.log")).unwrap().len();
        let b = fs::metadata(d.join("diag.1.log")).unwrap().len();
        assert!(a <= 10_000 && b <= 10_000, "{a} {b}");
        assert!(!d.join("diag.2.log").exists(), "only two files");
    }
}

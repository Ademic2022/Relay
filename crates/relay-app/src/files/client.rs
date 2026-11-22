//! Controller side: browse the Host, operate on files, transfer both ways.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, bail};
use relay_net::quic::{self, Connection, RecvStream, SendStream};
use relay_proto::v1::{self, file_message::Msg};

use super::{CHUNK, MAX_MSG, TAG, store};

/// Live progress of one transfer, shared with the UI.
#[derive(Debug, Default)]
pub struct Progress {
    pub name: std::sync::Mutex<String>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub resumed_from: AtomicU64,
    pub cancel: AtomicBool,
}

fn msg(m: Msg) -> v1::FileMessage {
    v1::FileMessage { msg: Some(m) }
}

async fn open(conn: &Connection) -> anyhow::Result<(SendStream, RecvStream)> {
    let (mut tx, rx) = conn.open_bi().await?;
    // Below input, video, cursor, chat and clipboard (FR-28).
    let _ = tx.set_priority(relay_net::channel::Channel::Files.priority());
    tx.write_all(&[TAG]).await?;
    Ok((tx, rx))
}

async fn recv(rx: &mut RecvStream) -> anyhow::Result<Msg> {
    quic::recv_msg::<v1::FileMessage>(rx, MAX_MSG)
        .await?
        .and_then(|m| m.msg)
        .context("host closed the file stream")
}

pub async fn list(conn: &Connection, path: &str) -> anyhow::Result<v1::DirListing> {
    let (mut tx, mut rx) = open(conn).await?;
    quic::send_msg(
        &mut tx,
        &msg(Msg::ListDir(v1::ListDir { path: path.into() })),
    )
    .await?;
    let _ = tx.finish();
    match recv(&mut rx).await? {
        Msg::Listing(l) if l.error.is_empty() => Ok(l),
        Msg::Listing(l) => bail!("{}", l.error),
        _ => bail!("unexpected answer"),
    }
}

pub async fn op(
    conn: &Connection,
    op: v1::FileOp,
    path: &str,
    new_path: &str,
) -> anyhow::Result<()> {
    let (mut tx, mut rx) = open(conn).await?;
    quic::send_msg(
        &mut tx,
        &msg(Msg::Op(v1::FileOpRequest {
            op: op as i32,
            path: path.into(),
            new_path: new_path.into(),
        })),
    )
    .await?;
    let _ = tx.finish();
    match recv(&mut rx).await? {
        Msg::Ack(a) if a.error.is_empty() => Ok(()),
        Msg::Ack(a) => bail!("{}", a.error),
        _ => bail!("unexpected answer"),
    }
}

/// What happened to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Copied {
    /// Saved at this path (on the receiving side).
    To(String),
    /// The name was taken and the policy is skip (FR-29).
    Skipped,
}

/// Upload `local` into the Host folder `remote_dir`. Resumes a partial
/// upload of the same file left by an earlier attempt.
pub async fn upload(
    conn: &Connection,
    local: &Path,
    remote_dir: &str,
    conflict: v1::Conflict,
    p: Arc<Progress>,
) -> anyhow::Result<Copied> {
    upload_one(conn, local, remote_dir, conflict, &p, true).await
}

/// One file. `own_total`: this transfer is the whole job, so it sets
/// `p.total`; inside a folder the caller has set it for all files.
async fn upload_one(
    conn: &Connection,
    local: &Path,
    remote_dir: &str,
    conflict: v1::Conflict,
    p: &Progress,
    own_total: bool,
) -> anyhow::Result<Copied> {
    let name = local
        .file_name()
        .context("no file name")?
        .to_string_lossy()
        .to_string();
    *p.name.lock().expect("poisoned") = name.clone();
    let l2 = local.to_path_buf();
    let (size, hash) = tokio::task::spawn_blocking(move || -> std::io::Result<(u64, [u8; 32])> {
        let md = std::fs::metadata(&l2)?;
        if md.is_dir() {
            return Err(std::io::Error::other("that is a folder"));
        }
        Ok((md.len(), store::hash_file(&l2)?))
    })
    .await??;
    let total = size.div_ceil(u64::from(CHUNK));
    if own_total {
        p.total.store(size, Ordering::Relaxed);
    }

    let (mut tx, mut rx) = open(conn).await?;
    quic::send_msg(
        &mut tx,
        &msg(Msg::Start(v1::TransferStart {
            path: remote_dir.into(),
            size,
            chunk_size: CHUNK,
            file_blake3: hash.to_vec().into(),
            resume_from_chunk: 0,
            upload: true,
            name: name.clone(),
            conflict: conflict as i32,
        })),
    )
    .await?;
    let from = match recv(&mut rx).await? {
        Msg::Ack(a) if a.skipped => {
            p.done.fetch_add(size, Ordering::Relaxed);
            return Ok(Copied::Skipped);
        }
        Msg::Ack(a) if a.error.is_empty() => a.next_chunk,
        Msg::Ack(a) => bail!("{}", a.error),
        _ => bail!("unexpected answer"),
    };
    let resumed = (from * u64::from(CHUNK)).min(size);
    p.resumed_from.fetch_add(resumed, Ordering::Relaxed);
    p.done.fetch_add(resumed, Ordering::Relaxed);

    // Send chunks while watching for an early error from the host.
    let send = async {
        let (ctx, mut crx) = tokio::sync::mpsc::channel::<std::io::Result<v1::FileChunk>>(2);
        let path = local.to_path_buf();
        std::thread::spawn(move || {
            use std::io::{Read, Seek};
            let read = || -> std::io::Result<()> {
                let mut f = std::fs::File::open(&path)?;
                f.seek(std::io::SeekFrom::Start(from * u64::from(CHUNK)))?;
                for index in from..total {
                    let len = (size - index * u64::from(CHUNK)).min(u64::from(CHUNK)) as usize;
                    let mut data = vec![0u8; len];
                    f.read_exact(&mut data)?;
                    let c = v1::FileChunk {
                        index,
                        chunk_blake3: blake3::hash(&data).as_bytes().to_vec().into(),
                        data: data.into(),
                    };
                    if ctx.blocking_send(Ok(c)).is_err() {
                        return Ok(());
                    }
                }
                Ok(())
            };
            if let Err(e) = read() {
                let _ = ctx.blocking_send(Err(e));
            }
        });
        while let Some(c) = crx.recv().await {
            if p.cancel.load(Ordering::Relaxed) {
                let _ = tx.reset(0u32.into());
                bail!("cancelled");
            }
            let c = c?;
            let n = c.data.len() as u64;
            quic::send_msg(&mut tx, &msg(Msg::Chunk(c))).await?;
            p.done.fetch_add(n, Ordering::Relaxed);
        }
        anyhow::Ok(())
    };
    send.await?;
    match recv(&mut rx).await? {
        Msg::Ack(a) if a.error.is_empty() && a.next_chunk == total => {
            Ok(Copied::To(remote_join(remote_dir, &name)))
        }
        Msg::Ack(a) => bail!("{}", a.error),
        _ => bail!("unexpected answer"),
    }
}

/// Download the Host file `remote` into `local_dir`.
pub async fn download(
    conn: &Connection,
    remote: &str,
    local_dir: &Path,
    conflict: v1::Conflict,
    p: Arc<Progress>,
) -> anyhow::Result<Copied> {
    download_one(conn, remote, local_dir, conflict, &p, true).await
}

async fn download_one(
    conn: &Connection,
    remote: &str,
    local_dir: &Path,
    conflict: v1::Conflict,
    p: &Progress,
    own_total: bool,
) -> anyhow::Result<Copied> {
    let (mut tx, mut rx) = open(conn).await?;
    quic::send_msg(
        &mut tx,
        &msg(Msg::Start(v1::TransferStart {
            path: remote.into(),
            upload: false,
            ..Default::default()
        })),
    )
    .await?;
    let start = match recv(&mut rx).await? {
        Msg::Start(s) => s,
        Msg::Ack(a) => bail!("{}", a.error),
        _ => bail!("unexpected answer"),
    };
    let name = store::safe_name(&start.name)
        .context("host sent an invalid file name")?
        .to_string();
    *p.name.lock().expect("poisoned") = name.clone();
    let hash: [u8; 32] = start.file_blake3.as_ref().try_into().context("bad hash")?;
    if start.chunk_size == 0 || start.chunk_size > CHUNK {
        bail!("bad chunk size");
    }
    let target = local_dir.join(&name);
    let (size, chunk) = (start.size, start.chunk_size);
    if own_total {
        p.total.store(size, Ordering::Relaxed);
    }
    // FR-29: skip before any data moves.
    if conflict == v1::Conflict::Skip && target.exists() {
        let _ = quic::send_msg(&mut tx, &ack_error("skipped")).await;
        let _ = tx.finish();
        p.done.fetch_add(size, Ordering::Relaxed);
        return Ok(Copied::Skipped);
    }
    let mut part =
        tokio::task::spawn_blocking(move || store::Partial::open(&target, size, chunk, hash))
            .await??;
    let total = part.total_chunks();
    let resumed = (part.next_chunk * u64::from(chunk)).min(size);
    p.resumed_from.fetch_add(resumed, Ordering::Relaxed);
    p.done.fetch_add(resumed, Ordering::Relaxed);
    quic::send_msg(
        &mut tx,
        &msg(Msg::Ack(v1::TransferAck {
            next_chunk: part.next_chunk,
            ..Default::default()
        })),
    )
    .await?;
    while part.next_chunk < total {
        if p.cancel.load(Ordering::Relaxed) {
            let _ = rx.stop(0u32.into());
            bail!("cancelled");
        }
        let c = match recv(&mut rx).await? {
            Msg::Chunk(c) => c,
            Msg::Ack(a) => bail!("{}", a.error),
            _ => bail!("unexpected message"),
        };
        let n = c.data.len() as u64;
        let r;
        (part, r) = tokio::task::spawn_blocking(move || {
            let r = part.write_chunk(c.index, &c.data, &c.chunk_blake3);
            (part, r)
        })
        .await?;
        r.map_err(anyhow::Error::msg)?;
        p.done.fetch_add(n, Ordering::Relaxed);
    }
    let dest = tokio::task::spawn_blocking(move || part.finish(conflict))
        .await?
        .map_err(anyhow::Error::msg)?;
    let _ = tx.finish();
    Ok(Copied::To(dest.display().to_string()))
}

fn ack_error(e: &str) -> v1::FileMessage {
    msg(Msg::Ack(v1::TransferAck {
        error: e.into(),
        ..Default::default()
    }))
}

// ------------------------------------------------------------ folders (FR-29)

/// Folders deeper than this are refused (and symlink loops on the Host
/// end here).
const MAX_DEPTH: usize = 32;
/// Most entries one folder transfer may carry.
const MAX_ENTRIES: usize = 100_000;

/// Result of a folder transfer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderSummary {
    /// Where the folder ended up.
    pub to: String,
    pub files: u64,
    pub skipped: u64,
}

/// Host paths are opaque strings: join with the separator they use.
pub fn remote_join(dir: &str, name: &str) -> String {
    let sep = if dir.contains('\\') && !dir.contains('/') {
        '\\'
    } else {
        '/'
    };
    if dir.ends_with(['/', '\\']) {
        format!("{dir}{name}")
    } else {
        format!("{dir}{sep}{name}")
    }
}

/// `name`, or `name (1)`, `name (2)`, ... if `taken` says it is in use.
fn free_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    (1..)
        .map(|n| format!("{name} ({n})"))
        .find(|c| !taken(c))
        .expect("some free name")
}

/// Where a folder called `name` goes inside a destination that already
/// has `existing` (name, is_dir) entries.
fn folder_target(
    name: &str,
    existing: &[(String, bool)],
    conflict: v1::Conflict,
) -> anyhow::Result<String> {
    match existing.iter().find(|(n, _)| n == name) {
        None => Ok(name.to_string()),
        // Keep both: a fresh "name (1)" folder.
        Some(_) if conflict == v1::Conflict::Rename => {
            Ok(free_name(name, |c| existing.iter().any(|(n, _)| n == c)))
        }
        // Overwrite / skip: merge into it, deciding file by file.
        Some((_, true)) => Ok(name.to_string()),
        Some((_, false)) => bail!("a file called {name} is in the way"),
    }
}

/// Upload the folder `local` (recursively) into the Host folder
/// `remote_dir` ("" = the Host's Downloads). Each file is a normal
/// resumable, verified transfer; `conflict` applies per file.
pub async fn upload_folder(
    conn: &Connection,
    local: &Path,
    remote_dir: &str,
    conflict: v1::Conflict,
    p: Arc<Progress>,
) -> anyhow::Result<FolderSummary> {
    let name = local
        .file_name()
        .context("no folder name")?
        .to_string_lossy()
        .to_string();
    *p.name.lock().expect("poisoned") = name.clone();
    let root = local.to_path_buf();
    let (dirs, files) = tokio::task::spawn_blocking(move || walk_local(&root)).await??;
    p.total
        .store(files.iter().map(|f| f.2).sum(), Ordering::Relaxed);

    let parent = if remote_dir.is_empty() {
        // Same place a dropped file goes (FR-27).
        let home = list(conn, "").await?.path;
        let dl = remote_join(&home, "Downloads");
        if list(conn, &dl).await.is_ok() {
            dl
        } else {
            home
        }
    } else {
        remote_dir.to_string()
    };
    let existing: Vec<(String, bool)> = list(conn, &parent)
        .await?
        .entries
        .into_iter()
        .map(|e| (e.name, e.is_dir))
        .collect();
    let top = remote_join(&parent, &folder_target(&name, &existing, conflict)?);

    let at = |rel: &[String]| rel.iter().fold(top.clone(), |d, n| remote_join(&d, n));
    op(conn, v1::FileOp::Mkdirs, &top, "").await?;
    for d in &dirs {
        op(conn, v1::FileOp::Mkdirs, &at(d), "").await?;
    }
    let mut sum = FolderSummary {
        to: top.clone(),
        ..Default::default()
    };
    for (path, rel_dir, _) in &files {
        if p.cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        match upload_one(conn, path, &at(rel_dir), conflict, &p, false).await? {
            Copied::To(_) => sum.files += 1,
            Copied::Skipped => sum.skipped += 1,
        }
    }
    Ok(sum)
}

/// Sub-folders (relative, parents first) and files (path, relative
/// folder, size) under `root`. Symlinks are not followed.
#[allow(clippy::type_complexity)]
fn walk_local(root: &Path) -> anyhow::Result<(Vec<Vec<String>>, Vec<(PathBuf, Vec<String>, u64)>)> {
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    let mut stack = vec![(root.to_path_buf(), Vec::<String>::new())];
    while let Some((dir, rel)) = stack.pop() {
        if rel.len() > MAX_DEPTH {
            bail!("{} is nested too deeply", dir.display());
        }
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .flatten()
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            if store::safe_name(&name).is_none() {
                continue; // partials and odd names stay behind
            }
            let Ok(md) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            let mut r = rel.clone();
            r.push(name);
            if md.is_dir() {
                dirs.push(r.clone());
                stack.push((e.path(), r));
            } else if md.is_file() {
                files.push((e.path(), rel.clone(), md.len()));
            }
            if dirs.len() + files.len() > MAX_ENTRIES {
                bail!("more than {MAX_ENTRIES} items; send smaller folders");
            }
        }
    }
    dirs.sort_by_key(Vec::len);
    Ok((dirs, files))
}

/// Download the Host folder `remote` (recursively) into `local_dir`.
pub async fn download_folder(
    conn: &Connection,
    remote: &str,
    local_dir: &Path,
    conflict: v1::Conflict,
    p: Arc<Progress>,
) -> anyhow::Result<FolderSummary> {
    let name = remote
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .and_then(store::safe_name)
        .context("not a folder name")?
        .to_string();
    *p.name.lock().expect("poisoned") = name.clone();

    // List everything first: totals for progress, and nothing is written
    // if the folder cannot be read.
    let (mut dirs, mut files) = (Vec::<Vec<String>>::new(), Vec::new());
    let mut queue = vec![(remote.to_string(), Vec::<String>::new())];
    while let Some((dir, rel)) = queue.pop() {
        if rel.len() > MAX_DEPTH {
            bail!("{dir} is nested too deeply (or loops)");
        }
        let l = list(conn, &dir).await?;
        for e in l.entries {
            // The Host chooses these names: never let one leave the folder.
            let Some(n) = store::safe_name(&e.name) else {
                continue;
            };
            let mut r = rel.clone();
            r.push(n.to_string());
            if e.is_dir {
                dirs.push(r.clone());
                queue.push((remote_join(&dir, n), r));
            } else {
                files.push((remote_join(&dir, n), rel.clone(), e.size));
            }
            if dirs.len() + files.len() > MAX_ENTRIES {
                bail!("more than {MAX_ENTRIES} items; fetch smaller folders");
            }
        }
    }
    p.total
        .store(files.iter().map(|f| f.2).sum(), Ordering::Relaxed);

    let existing: Vec<(String, bool)> = std::fs::read_dir(local_dir)
        .with_context(|| format!("reading {}", local_dir.display()))?
        .flatten()
        .map(|e| {
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            (e.file_name().to_string_lossy().to_string(), is_dir)
        })
        .collect();
    let top = local_dir.join(folder_target(&name, &existing, conflict)?);
    std::fs::create_dir_all(&top)?;
    dirs.sort_by_key(Vec::len);
    let at = |rel: &[String]| rel.iter().fold(top.clone(), |d, n| d.join(n));
    for d in &dirs {
        std::fs::create_dir_all(at(d))?;
    }
    let mut sum = FolderSummary {
        to: top.display().to_string(),
        ..Default::default()
    };
    for (path, rel_dir, _) in &files {
        if p.cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        match download_one(conn, path, &at(rel_dir), conflict, &p, false).await? {
            Copied::To(_) => sum.files += 1,
            Copied::Skipped => sum.skipped += 1,
        }
    }
    Ok(sum)
}

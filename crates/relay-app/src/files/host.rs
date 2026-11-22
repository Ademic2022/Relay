//! Host side: answers one file request per stream, as the logged-in user.

use std::io::Read;
use std::path::{Path, PathBuf};

use relay_net::quic::{self, RecvStream, SendStream};
use relay_proto::v1::{self, file_message::Msg};

use super::{CHUNK, MAX_MSG, store};

fn msg(m: Msg) -> v1::FileMessage {
    v1::FileMessage { msg: Some(m) }
}

fn ack(next_chunk: u64, error: impl Into<String>) -> v1::FileMessage {
    msg(Msg::Ack(v1::TransferAck {
        next_chunk,
        error: error.into(),
        cancelled: false,
    }))
}

/// The folder a `ListDir("")` opens: the user's home.
pub fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Where dropped files go by default (FR-27).
pub fn default_drop_dir() -> PathBuf {
    let h = home();
    let d = h.join("Downloads");
    if d.is_dir() { d } else { h }
}

fn modified_ms(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn list(path: &str) -> v1::DirListing {
    let dir = if path.is_empty() {
        home()
    } else {
        PathBuf::from(path)
    };
    let dir = dir.canonicalize().unwrap_or(dir);
    let mut entries = Vec::new();
    let err = match std::fs::read_dir(&dir) {
        Ok(rd) => {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".relaypart") || name.ends_with(".relaypart.meta") {
                    continue;
                }
                let md = e.metadata().ok();
                entries.push(v1::DirEntry {
                    name,
                    is_dir: md.as_ref().is_some_and(|m| m.is_dir()),
                    size: md.as_ref().map_or(0, |m| m.len()),
                    modified_ms: md.as_ref().map_or(0, modified_ms),
                });
            }
            String::new()
        }
        Err(e) => e.to_string(),
    };
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    v1::DirListing {
        path: dir.to_string_lossy().to_string(),
        entries,
        error: err,
    }
}

fn file_op(op: &v1::FileOpRequest) -> Result<(), String> {
    let path = Path::new(&op.path);
    let r = match v1::FileOp::try_from(op.op).map_err(|_| "unknown operation")? {
        v1::FileOp::Mkdir => std::fs::create_dir(path),
        v1::FileOp::Rename => {
            let new_name = store::safe_name(&op.new_path).ok_or("invalid new name")?;
            let dest = path.with_file_name(new_name);
            if dest.exists() {
                return Err("a file with that name already exists".into());
            }
            std::fs::rename(path, dest)
        }
        v1::FileOp::Delete => {
            if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            }
        }
        v1::FileOp::Unspecified => return Err("unknown operation".into()),
    };
    r.map_err(|e| e.to_string())
}

/// Handle one request stream (after its tag byte).
pub async fn serve(mut tx: SendStream, mut rx: RecvStream) -> anyhow::Result<()> {
    let Some(first) = quic::recv_msg::<v1::FileMessage>(&mut rx, MAX_MSG).await? else {
        return Ok(());
    };
    match first.msg {
        Some(Msg::ListDir(l)) => {
            let listing = tokio::task::spawn_blocking(move || list(&l.path)).await?;
            quic::send_msg(&mut tx, &msg(Msg::Listing(listing))).await?;
        }
        Some(Msg::Op(op)) => {
            let r = tokio::task::spawn_blocking(move || file_op(&op)).await?;
            quic::send_msg(&mut tx, &ack(0, r.err().unwrap_or_default())).await?;
        }
        Some(Msg::Start(s)) if s.upload => receive_upload(&mut tx, &mut rx, s).await?,
        Some(Msg::Start(s)) => send_download(&mut tx, &mut rx, s).await?,
        _ => anyhow::bail!("unexpected file request"),
    }
    let _ = tx.finish();
    Ok(())
}

/// Controller → Host.
async fn receive_upload(
    tx: &mut SendStream,
    rx: &mut RecvStream,
    s: v1::TransferStart,
) -> anyhow::Result<()> {
    let Some(name) = store::safe_name(&s.name) else {
        quic::send_msg(tx, &ack(0, "invalid file name")).await?;
        return Ok(());
    };
    let hash: [u8; 32] = s
        .file_blake3
        .as_ref()
        .try_into()
        .map_err(|_| anyhow::anyhow!("bad hash"))?;
    if s.chunk_size == 0 || s.chunk_size > CHUNK {
        quic::send_msg(tx, &ack(0, "bad chunk size")).await?;
        return Ok(());
    }
    // Empty folder = "wherever dropped files go" (FR-27).
    let dir = if s.path.is_empty() {
        default_drop_dir()
    } else {
        PathBuf::from(&s.path)
    };
    let target = dir.join(name);
    let (size, chunk) = (s.size, s.chunk_size);
    let opened =
        tokio::task::spawn_blocking(move || store::Partial::open(&target, size, chunk, hash))
            .await?;
    let mut part = match opened {
        Ok(p) => p,
        Err(e) => {
            quic::send_msg(tx, &ack(0, e.to_string())).await?;
            return Ok(());
        }
    };
    let total = part.total_chunks();
    quic::send_msg(tx, &ack(part.next_chunk, "")).await?;
    while part.next_chunk < total {
        let Some(m) = quic::recv_msg::<v1::FileMessage>(rx, MAX_MSG).await? else {
            return Ok(()); // sender went away; the partial stays for a resume
        };
        let Some(Msg::Chunk(c)) = m.msg else {
            anyhow::bail!("expected a chunk");
        };
        // Blocking write off the async threads.
        let r;
        (part, r) = tokio::task::spawn_blocking(move || {
            let r = part.write_chunk(c.index, &c.data, &c.chunk_blake3);
            (part, r)
        })
        .await?;
        if let Err(e) = r {
            quic::send_msg(tx, &ack(part.next_chunk, e)).await?;
            return Ok(());
        }
    }
    let r = tokio::task::spawn_blocking(move || part.finish()).await?;
    match r {
        Ok(dest) => {
            tracing::info!("received {}", dest.display());
            quic::send_msg(tx, &ack(total, "")).await?;
        }
        Err(e) => quic::send_msg(tx, &ack(0, e)).await?,
    }
    Ok(())
}

/// Host → Controller.
async fn send_download(
    tx: &mut SendStream,
    rx: &mut RecvStream,
    s: v1::TransferStart,
) -> anyhow::Result<()> {
    let path = PathBuf::from(&s.path);
    let p2 = path.clone();
    let info = tokio::task::spawn_blocking(move || -> std::io::Result<(u64, [u8; 32])> {
        let md = std::fs::metadata(&p2)?;
        if md.is_dir() {
            return Err(std::io::Error::other("folders cannot be downloaded yet"));
        }
        Ok((md.len(), store::hash_file(&p2)?))
    })
    .await?;
    let (size, hash) = match info {
        Ok(v) => v,
        Err(e) => {
            quic::send_msg(tx, &ack(0, e.to_string())).await?;
            return Ok(());
        }
    };
    quic::send_msg(
        tx,
        &msg(Msg::Start(v1::TransferStart {
            path: s.path.clone(),
            size,
            chunk_size: CHUNK,
            file_blake3: hash.to_vec().into(),
            resume_from_chunk: 0,
            upload: false,
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        })),
    )
    .await?;
    // The Controller says where to resume from.
    let from = match quic::recv_msg::<v1::FileMessage>(rx, MAX_MSG).await? {
        Some(v1::FileMessage {
            msg: Some(Msg::Ack(a)),
        }) if a.error.is_empty() => a.next_chunk,
        _ => return Ok(()),
    };
    send_chunks(tx, &path, size, from).await
}

/// Stream chunks `from..` of `path`, reading ahead on a blocking thread.
pub async fn send_chunks(
    tx: &mut SendStream,
    path: &Path,
    size: u64,
    from: u64,
) -> anyhow::Result<()> {
    let total = size.div_ceil(u64::from(CHUNK));
    let (ctx, mut crx) = tokio::sync::mpsc::channel::<std::io::Result<v1::FileChunk>>(2);
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let read = || -> std::io::Result<()> {
            use std::io::Seek;
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
        quic::send_msg(tx, &msg(Msg::Chunk(c?))).await?;
    }
    Ok(())
}

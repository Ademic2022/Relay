//! Host side: answers one file request per stream, as the logged-in user.

use std::io::Read;
use std::path::{Path, PathBuf};

use relay_net::quic::{self, RecvStream, SendStream};
use relay_proto::v1::{self, file_message::Msg};

use super::throttle::{SLICE, Throttle};
use super::{CHUNK, MAX_MSG, store};

fn msg(m: Msg) -> v1::FileMessage {
    v1::FileMessage { msg: Some(m) }
}

fn ack(next_chunk: u64, error: impl Into<String>) -> v1::FileMessage {
    msg(Msg::Ack(v1::TransferAck {
        next_chunk,
        error: error.into(),
        ..Default::default()
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
    // FR-39: Relay's own data (audit log, config) is off limits.
    if crate::logs::is_protected(path) {
        return Err("that folder belongs to Relay and cannot be changed remotely".into());
    }
    let r = match v1::FileOp::try_from(op.op).map_err(|_| "unknown operation")? {
        v1::FileOp::Mkdir => std::fs::create_dir(path),
        v1::FileOp::Mkdirs => std::fs::create_dir_all(path),
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

/// FR-11: answer a request with "file transfer is off", whatever it was.
pub async fn refuse(mut tx: SendStream, mut rx: RecvStream) -> anyhow::Result<()> {
    const OFF: &str = "the host turned file transfer off";
    let first = quic::recv_msg::<v1::FileMessage>(&mut rx, MAX_MSG).await?;
    let reply = match first.and_then(|m| m.msg) {
        Some(Msg::ListDir(l)) => msg(Msg::Listing(v1::DirListing {
            path: l.path,
            entries: Vec::new(),
            error: OFF.into(),
        })),
        _ => ack(0, OFF),
    };
    quic::send_msg(&mut tx, &reply).await?;
    let _ = tx.finish();
    Ok(())
}

/// Handle one request stream (after its tag byte).
pub async fn serve(
    mut tx: SendStream,
    mut rx: RecvStream,
    throttle: &Throttle,
) -> anyhow::Result<()> {
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
        Some(Msg::Start(s)) if s.upload => receive_upload(&mut tx, &mut rx, s, throttle).await?,
        Some(Msg::Start(s)) => send_download(&mut tx, &mut rx, s, throttle).await?,
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
    throttle: &Throttle,
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
    if crate::logs::is_protected(&dir) {
        quic::send_msg(tx, &ack(0, "that folder belongs to Relay")).await?;
        return Ok(());
    }
    let target = dir.join(name);
    let conflict = v1::Conflict::try_from(s.conflict).unwrap_or(v1::Conflict::Rename);
    // FR-29: skip before any data moves.
    if conflict == v1::Conflict::Skip && target.exists() {
        quic::send_msg(
            tx,
            &msg(Msg::Ack(v1::TransferAck {
                skipped: true,
                ..Default::default()
            })),
        )
        .await?;
        return Ok(());
    }
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
        let Some(m) = recv_paced(rx, throttle).await? else {
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
    let r = tokio::task::spawn_blocking(move || part.finish(conflict)).await?;
    match r {
        Ok(dest) => {
            tracing::info!("received {}", dest.display());
            quic::send_msg(tx, &ack(total, "")).await?;
        }
        Err(e) => quic::send_msg(tx, &ack(0, e)).await?,
    }
    Ok(())
}

/// [`quic::recv_msg`] for upload chunks, reading the body in [`SLICE`]s
/// paced by `throttle` (FR-30): reading slower makes QUIC flow control slow
/// the Controller down smoothly instead of in 1 MiB bursts.
async fn recv_paced(
    rx: &mut RecvStream,
    throttle: &Throttle,
) -> anyhow::Result<Option<v1::FileMessage>> {
    let mut len: u64 = 0;
    for i in 0..10 {
        let mut b = [0u8; 1];
        match rx.read_exact(&mut b).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(0)) if i == 0 => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        len |= u64::from(b[0] & 0x7f) << (7 * i);
        if b[0] & 0x80 == 0 {
            let len = usize::try_from(len).unwrap_or(usize::MAX);
            anyhow::ensure!(len <= MAX_MSG, "message of {len} bytes too large");
            let mut buf = vec![0u8; len];
            for slice in buf.chunks_mut(SLICE) {
                throttle.take(slice.len()).await;
                rx.read_exact(slice).await?;
            }
            return Ok(Some(prost::Message::decode(&buf[..])?));
        }
    }
    anyhow::bail!("bad length prefix")
}

/// Host → Controller.
async fn send_download(
    tx: &mut SendStream,
    rx: &mut RecvStream,
    s: v1::TransferStart,
    throttle: &Throttle,
) -> anyhow::Result<()> {
    let path = PathBuf::from(&s.path);
    if crate::logs::is_protected(&path) {
        quic::send_msg(tx, &ack(0, "that file belongs to Relay")).await?;
        return Ok(());
    }
    let p2 = path.clone();
    let info = tokio::task::spawn_blocking(move || -> std::io::Result<(u64, [u8; 32])> {
        let md = std::fs::metadata(&p2)?;
        if md.is_dir() {
            // Folders are listed and fetched file by file (FR-29).
            return Err(std::io::Error::other("that is a folder"));
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
            ..Default::default()
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
    send_chunks(tx, &path, size, from, throttle).await
}

/// Stream chunks `from..` of `path`, reading ahead on a blocking thread.
/// Each chunk is written in [`SLICE`]s paced by `throttle` (FR-30).
pub async fn send_chunks(
    tx: &mut SendStream,
    path: &Path,
    size: u64,
    from: u64,
    throttle: &Throttle,
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
        let bytes = prost::Message::encode_length_delimited_to_vec(&msg(Msg::Chunk(c?)));
        for slice in bytes.chunks(SLICE) {
            throttle.take(slice.len()).await;
            tx.write_all(slice).await?;
        }
    }
    Ok(())
}

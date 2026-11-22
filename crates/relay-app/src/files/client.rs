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

/// Upload `local` into the Host folder `remote_dir`. Resumes a partial
/// upload of the same file left by an earlier attempt.
pub async fn upload(
    conn: &Connection,
    local: &Path,
    remote_dir: &str,
    p: Arc<Progress>,
) -> anyhow::Result<()> {
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
            return Err(std::io::Error::other("folders cannot be uploaded yet"));
        }
        Ok((md.len(), store::hash_file(&l2)?))
    })
    .await??;
    let total = size.div_ceil(u64::from(CHUNK));
    p.total.store(size, Ordering::Relaxed);

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
            name,
        })),
    )
    .await?;
    let from = match recv(&mut rx).await? {
        Msg::Ack(a) if a.error.is_empty() => a.next_chunk,
        Msg::Ack(a) => bail!("{}", a.error),
        _ => bail!("unexpected answer"),
    };
    p.resumed_from
        .store(from * u64::from(CHUNK), Ordering::Relaxed);
    p.done
        .store((from * u64::from(CHUNK)).min(size), Ordering::Relaxed);

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
        Msg::Ack(a) if a.error.is_empty() && a.next_chunk == total => Ok(()),
        Msg::Ack(a) => bail!("{}", a.error),
        _ => bail!("unexpected answer"),
    }
}

/// Download the Host file `remote` into `local_dir`. Returns the saved path.
pub async fn download(
    conn: &Connection,
    remote: &str,
    local_dir: &Path,
    p: Arc<Progress>,
) -> anyhow::Result<PathBuf> {
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
    let mut part =
        tokio::task::spawn_blocking(move || store::Partial::open(&target, size, chunk, hash))
            .await??;
    let total = part.total_chunks();
    p.total.store(size, Ordering::Relaxed);
    p.resumed_from
        .store(part.next_chunk * u64::from(chunk), Ordering::Relaxed);
    p.done.store(
        (part.next_chunk * u64::from(chunk)).min(size),
        Ordering::Relaxed,
    );
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
    let dest = tokio::task::spawn_blocking(move || part.finish())
        .await?
        .map_err(anyhow::Error::msg)?;
    let _ = tx.finish();
    Ok(dest)
}

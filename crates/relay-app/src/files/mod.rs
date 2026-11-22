//! File transfer (FR-26 to FR-29, §8.1 "Files").
//!
//! Every operation runs on its own bidirectional QUIC stream, tagged
//! [`Channel::Files`] and at the lowest stream priority, so transfers never
//! hold up input or video (FR-28).
//!
//! Transfers are chunked ([`CHUNK`]) with a BLAKE3 hash per chunk, and the
//! whole file is verified against its BLAKE3 hash before it is moved into
//! place. A partial download or upload lives next to its target as
//! `<name>.relaypart`, with `<name>.relaypart.meta` recording the source's
//! size and hash: a later attempt for the same source resumes from the
//! last complete chunk; a different source starts over (FR-28).
//!
//! Folders (FR-29) are walked by the Controller and sent file by file over
//! the same path, so every file is resumable and verified. A taken name is
//! handled per [`relay_proto::v1::Conflict`]: keep both (default),
//! overwrite, or skip.

pub mod client;
pub mod host;
pub mod store;
pub mod throttle;

use relay_net::channel::Channel;

pub const CHUNK: u32 = 1 << 20;
/// Largest message on a file stream (a chunk plus framing).
pub const MAX_MSG: usize = CHUNK as usize + 4096;
pub const TAG: u8 = Channel::Files as u8;

/// Host: answer file requests on `conn` until it closes (one task per
/// request stream, so a big transfer does not block browsing). All of
/// them share `throttle` (FR-30). While `allowed` is false (the Host
/// turned file transfer off, FR-11) every request is refused.
pub async fn accept_loop(
    conn: relay_net::quic::Connection,
    throttle: std::sync::Arc<throttle::Throttle>,
    allowed: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    while let Ok((tx, mut rx)) = conn.accept_bi().await {
        let (throttle, allowed) = (throttle.clone(), allowed.clone());
        tokio::spawn(async move {
            let mut tag = [0u8; 1];
            if rx.read_exact(&mut tag).await.is_err() || tag[0] != TAG {
                return;
            }
            if !allowed.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = host::refuse(tx, rx).await;
                return;
            }
            if let Err(e) = host::serve(tx, rx, &throttle).await {
                tracing::debug!("file request: {e:#}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::client::{self, Progress};
    use super::*;
    use relay_net::quic;
    use relay_proto::v1;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("relay-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn random_file(path: &Path, len: usize) {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let data: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        std::fs::write(path, data).unwrap();
    }

    /// A host node that serves files, and a way to connect to it.
    struct Rig {
        host: quic::Node,
        ctl: quic::Node,
        ctl_id: crate::identity::Me,
    }

    impl Rig {
        fn new() -> Self {
            Self::with_throttle(Arc::default())
        }

        fn with_throttle(throttle: Arc<throttle::Throttle>) -> Self {
            Self::with(throttle, Arc::new(std::sync::atomic::AtomicBool::new(true)))
        }

        fn with(
            throttle: Arc<throttle::Throttle>,
            allowed: Arc<std::sync::atomic::AtomicBool>,
        ) -> Self {
            let lo = "127.0.0.1:0".parse().unwrap();
            let (a, b) = (crate::identity::ephemeral(), crate::identity::ephemeral());
            let (host, _) = quic::node(lo, &a.tls, Some(&[quic::ALPN])).unwrap();
            let (ctl, _) = quic::node(lo, &b.tls, None).unwrap();
            let ep = host.endpoint.clone();
            tokio::spawn(async move {
                while let Some(i) = ep.accept().await {
                    if let Ok(c) = i.await {
                        tokio::spawn(accept_loop(c, throttle.clone(), allowed.clone()));
                    }
                }
            });
            Self {
                host,
                ctl,
                ctl_id: b,
            }
        }

        async fn connect(&self) -> quic::Connection {
            let (cfg, _) = quic::client_config(
                &self.ctl_id.tls,
                relay_net::tls::Expect::AnyRecord,
                quic::ALPN,
                Duration::from_secs(2),
            )
            .unwrap();
            quic::connect(
                &self.ctl.endpoint,
                cfg,
                self.host.endpoint.local_addr().unwrap(),
            )
            .await
            .unwrap()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upload_and_download_round_trip() {
        let (src, host_dir, back) = (tmp("rt-src"), tmp("rt-host"), tmp("rt-back"));
        random_file(&src.join("data.bin"), 24 * 1024 * 1024 + 123);
        let rig = Rig::new();
        let conn = rig.connect().await;

        let p = Arc::new(Progress::default());
        client::upload(
            &conn,
            &src.join("data.bin"),
            host_dir.to_str().unwrap(),
            v1::Conflict::Rename,
            p.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            p.done.load(Ordering::Relaxed),
            p.total.load(Ordering::Relaxed)
        );
        assert_eq!(
            std::fs::read(host_dir.join("data.bin")).unwrap(),
            std::fs::read(src.join("data.bin")).unwrap()
        );

        // Same name again: kept side by side, not overwritten (FR-29).
        client::upload(
            &conn,
            &src.join("data.bin"),
            host_dir.to_str().unwrap(),
            v1::Conflict::Rename,
            Arc::default(),
        )
        .await
        .unwrap();
        assert!(host_dir.join("data (1).bin").exists());

        let remote = host_dir.join("data.bin").to_string_lossy().to_string();
        let saved = client::download(&conn, &remote, &back, v1::Conflict::Rename, Arc::default())
            .await
            .unwrap();
        let client::Copied::To(saved) = saved else {
            panic!("not skipped")
        };
        assert_eq!(
            std::fs::read(saved).unwrap(),
            std::fs::read(src.join("data.bin")).unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upload_resumes_after_the_connection_drops() {
        let (src, host_dir) = (tmp("res-src"), tmp("res-host"));
        random_file(&src.join("big.bin"), 64 * 1024 * 1024);
        let rig = Rig::new();

        // First attempt: drop the connection about a third of the way in.
        let conn = rig.connect().await;
        let p = Arc::new(Progress::default());
        let (c2, p2, s2, h2) = (conn.clone(), p.clone(), src.clone(), host_dir.clone());
        let t = tokio::spawn(async move {
            client::upload(
                &c2,
                &s2.join("big.bin"),
                h2.to_str().unwrap(),
                v1::Conflict::Rename,
                p2,
            )
            .await
        });
        while p.done.load(Ordering::Relaxed) < 20 * 1024 * 1024 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        conn.close(0u32.into(), b"network drop");
        assert!(t.await.unwrap().is_err(), "first attempt fails");
        assert!(!host_dir.join("big.bin").exists());

        // Second attempt on a new connection picks up where it stopped.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let conn = rig.connect().await;
        let p = Arc::new(Progress::default());
        client::upload(
            &conn,
            &src.join("big.bin"),
            host_dir.to_str().unwrap(),
            v1::Conflict::Rename,
            p.clone(),
        )
        .await
        .unwrap();
        let resumed = p.resumed_from.load(Ordering::Relaxed);
        assert!(resumed >= 10 * 1024 * 1024, "resumed from {resumed} bytes");
        assert_eq!(
            std::fs::read(host_dir.join("big.bin")).unwrap(),
            std::fs::read(src.join("big.bin")).unwrap()
        );
        assert!(
            !store::part_paths(&host_dir.join("big.bin")).0.exists(),
            "partial cleaned up"
        );
    }

    /// Every file under `root`, relative path → contents.
    fn tree(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let rel = e
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if e.path().is_dir() {
                    out.insert(format!("{rel}/"), Vec::new());
                    stack.push(e.path());
                } else {
                    out.insert(rel, std::fs::read(e.path()).unwrap());
                }
            }
        }
        out
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn folders_with_conflict_policies() {
        let (src, host_dir, back) = (tmp("fd-src"), tmp("fd-host"), tmp("fd-back"));
        let proj = src.join("project");
        std::fs::create_dir_all(proj.join("src/deep")).unwrap();
        std::fs::create_dir_all(proj.join("empty")).unwrap();
        std::fs::write(proj.join("README"), "hello").unwrap();
        std::fs::write(proj.join("src/main.rs"), "fn main() {}").unwrap();
        random_file(&proj.join("src/deep/blob.bin"), 3 * 1024 * 1024 + 7);
        let rig = Rig::new();
        let conn = rig.connect().await;
        let hd = host_dir.to_str().unwrap();
        let up = |c| {
            let (conn, proj) = (conn.clone(), proj.clone());
            async move {
                let p = Arc::new(Progress::default());
                let s = client::upload_folder(&conn, &proj, hd, c, p.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    p.done.load(Ordering::Relaxed),
                    p.total.load(Ordering::Relaxed),
                    "progress reaches 100%"
                );
                s
            }
        };

        let s = up(v1::Conflict::Rename).await;
        assert_eq!((s.files, s.skipped), (3, 0));
        assert_eq!(
            tree(&host_dir.join("project")),
            tree(&proj),
            "same tree, empty folder too"
        );

        // Keep both: a second copy beside the first.
        let s = up(v1::Conflict::Rename).await;
        assert!(s.to.ends_with("project (1)"), "{}", s.to);
        assert_eq!(tree(&host_dir.join("project (1)")), tree(&proj));

        // Skip: merge into the existing folder, nothing re-sent.
        std::fs::write(proj.join("README"), "changed").unwrap();
        std::fs::write(proj.join("NEW"), "new").unwrap();
        let s = up(v1::Conflict::Skip).await;
        assert_eq!((s.files, s.skipped), (1, 3));
        assert_eq!(
            std::fs::read(host_dir.join("project/README")).unwrap(),
            b"hello"
        );
        assert!(host_dir.join("project/NEW").exists());

        // Overwrite: changed files replaced in place, no "(1)" copies.
        let s = up(v1::Conflict::Overwrite).await;
        assert_eq!((s.files, s.skipped), (4, 0));
        assert_eq!(tree(&host_dir.join("project")), tree(&proj));

        // And back down again.
        let remote = host_dir.join("project").to_string_lossy().to_string();
        let s =
            client::download_folder(&conn, &remote, &back, v1::Conflict::Rename, Arc::default())
                .await
                .unwrap();
        assert_eq!(s.files, 4);
        assert_eq!(tree(&back.join("project")), tree(&proj));
        let s = client::download_folder(&conn, &remote, &back, v1::Conflict::Skip, Arc::default())
            .await
            .unwrap();
        assert_eq!((s.files, s.skipped), (0, 4));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn squeezed_video_slows_transfers_both_ways() {
        let (src, host_dir, back) = (tmp("thr-src"), tmp("thr-host"), tmp("thr-back"));
        random_file(&src.join("f.bin"), 1_500_000);
        let t = Arc::new(throttle::Throttle::default());
        // Video squeezed at 80 Mbps target: files get 8 Mbps = 1 MB/s.
        t.on_feedback(
            &v1::AbrFeedback {
                delay_trend_us: 50_000,
                ..Default::default()
            },
            80_000,
        );
        let rig = Rig::with_throttle(t);
        let conn = rig.connect().await;
        let hd = host_dir.to_str().unwrap();

        let t0 = std::time::Instant::now();
        client::upload(
            &conn,
            &src.join("f.bin"),
            hd,
            v1::Conflict::Rename,
            Arc::default(),
        )
        .await
        .unwrap();
        let up = t0.elapsed().as_secs_f64();
        let t0 = std::time::Instant::now();
        let remote = host_dir.join("f.bin").to_string_lossy().to_string();
        client::download(&conn, &remote, &back, v1::Conflict::Rename, Arc::default())
            .await
            .unwrap();
        let down = t0.elapsed().as_secs_f64();
        // 1.5 MB at 1 MB/s, less the first burst (and on loopback the
        // unthrottled transfer takes milliseconds).
        assert!(up > 1.0, "upload took {up:.2} s");
        assert!(down > 1.2, "download took {down:.2} s");
        assert!(up < 4.0 && down < 4.0, "not stalled: {up:.2} / {down:.2} s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn host_can_turn_file_transfer_off_and_on() {
        let d = tmp("perm");
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rig = Rig::with(Arc::default(), allowed.clone());
        let conn = rig.connect().await;
        let err = client::list(&conn, d.to_str().unwrap()).await.unwrap_err();
        assert!(
            format!("{err}").contains("turned file transfer off"),
            "{err}"
        );
        allowed.store(true, Ordering::Relaxed);
        client::list(&conn, d.to_str().unwrap()).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn browse_and_operate() {
        let d = tmp("ops");
        std::fs::write(d.join("a.txt"), "hello").unwrap();
        let rig = Rig::new();
        let conn = rig.connect().await;
        let p = d.to_str().unwrap();
        client::op(
            &conn,
            v1::FileOp::Mkdir,
            d.join("sub").to_str().unwrap(),
            "",
        )
        .await
        .unwrap();
        client::op(
            &conn,
            v1::FileOp::Rename,
            d.join("a.txt").to_str().unwrap(),
            "b.txt",
        )
        .await
        .unwrap();
        let l = client::list(&conn, p).await.unwrap();
        let names: Vec<_> = l
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.is_dir))
            .collect();
        assert_eq!(
            names,
            vec![("sub", true), ("b.txt", false)],
            "folders first"
        );
        assert!(
            client::op(
                &conn,
                v1::FileOp::Rename,
                d.join("b.txt").to_str().unwrap(),
                "../escape"
            )
            .await
            .is_err()
        );
        client::op(
            &conn,
            v1::FileOp::Delete,
            d.join("b.txt").to_str().unwrap(),
            "",
        )
        .await
        .unwrap();
        assert!(
            client::list(&conn, d.join("nope").to_str().unwrap())
                .await
                .is_err()
        );
        let home = client::list(&conn, "").await.unwrap();
        assert!(!home.path.is_empty(), "empty path lists the home folder");
    }

    /// §12 M6: "5 GB file transfers ... BLAKE3 verifies". Slow; run with
    /// `RELAY_BIG_TEST=1 cargo test --release -p relay-app five_gb -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn five_gb_upload_with_resume() {
        if std::env::var_os("RELAY_BIG_TEST").is_none() {
            return;
        }
        let (src, host_dir) = (tmp("5g-src"), tmp("5g-host"));
        let size: u64 = 5 * 1024 * 1024 * 1024;
        {
            use std::io::Write;
            let mut f =
                std::io::BufWriter::new(std::fs::File::create(src.join("five.bin")).unwrap());
            let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut block = vec![0u8; 1 << 20];
            for _ in 0..size >> 20 {
                for b in block.as_chunks_mut::<8>().0 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    b.copy_from_slice(&x.to_le_bytes());
                }
                f.write_all(&block).unwrap();
            }
        }
        let rig = Rig::new();
        let t0 = std::time::Instant::now();
        let conn = rig.connect().await;
        let p = Arc::new(Progress::default());
        let (c2, p2, s2, h2) = (conn.clone(), p.clone(), src.clone(), host_dir.clone());
        let t = tokio::spawn(async move {
            client::upload(
                &c2,
                &s2.join("five.bin"),
                h2.to_str().unwrap(),
                v1::Conflict::Rename,
                p2,
            )
            .await
        });
        while p.done.load(Ordering::Relaxed) < size / 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        conn.close(0u32.into(), b"network drop");
        let _ = t.await;
        let conn = rig.connect().await;
        let p = Arc::new(Progress::default());
        client::upload(
            &conn,
            &src.join("five.bin"),
            host_dir.to_str().unwrap(),
            v1::Conflict::Rename,
            p.clone(),
        )
        .await
        .unwrap();
        let el = t0.elapsed();
        eprintln!(
            "[5 GB] done in {:.1} s ({:.0} MB/s incl. hashing), resumed from {:.2} GB",
            el.as_secs_f64(),
            size as f64 / 1e6 / el.as_secs_f64(),
            p.resumed_from.load(Ordering::Relaxed) as f64 / 1e9
        );
        assert_eq!(
            std::fs::metadata(host_dir.join("five.bin")).unwrap().len(),
            size
        );
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&host_dir);
    }
}

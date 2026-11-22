//! Relay → P2P switch for a live session (FR-4).
//!
//! QUIC cannot move a connection to a new server address, so the switch
//! happens under QUIC: each side's [`Paths`](relay_net::demux::Paths)
//! keeps QUIC believing it talks to the relay address while datagrams go
//! straight to the peer. Keys, streams and congestion state carry on;
//! nothing reconnects or re-authenticates.
//!
//! 1. While relayed, both peers send authenticated probes to each other's
//!    candidate addresses and answer the probes they receive.
//! 2. The Controller gets an ack from address X: it starts accepting
//!    datagrams from X as if from the relay, and sends `PathSwitch`.
//! 3. The Host maps the relay address to the last verified probe source
//!    and answers `PathSwitched`.
//! 4. The Controller sends to X too. The relay entry idles out.
//!
//! Probes are MAC'd with a key from the session's TLS exporter (see
//! [`relay_net::ctl::probe_key`]), so a third party cannot redirect
//! traffic. Limitation: while a switched session is live, other
//! connections relayed to the same host through the same relay address
//! are misrouted. The host streams one session at a time.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use relay_net::ctl::{self, Kind};
use relay_net::demux::Paths;
use relay_net::quic::{self, Node};

use crate::rdv::{ControlHub, Expected};

/// Fast probing right after connecting, then a slow background cadence.
const FAST: Duration = Duration::from_millis(200);
const FAST_FOR: Duration = Duration::from_secs(5);
const SLOW: Duration = Duration::from_secs(2);

fn nonce() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("OS RNG");
    u64::from_le_bytes(b)
}

/// One side of a probing session.
pub struct Prober {
    hub: ControlHub,
    token: ctl::Token,
    /// Last address a verified probe or ack came from.
    latest: Arc<Mutex<Option<SocketAddr>>>,
    acked: tokio::sync::watch::Receiver<Option<SocketAddr>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Prober {
    fn drop(&mut self) {
        self.task.abort();
        self.hub.stop_probes(&self.token);
    }
}

impl Prober {
    /// Start probing `exp.candidates` and answering probes, for the
    /// session whose TLS exporter is `exporter`.
    pub fn start(node: &Node, hub: &ControlHub, exp: &Expected, exporter: &[u8; 32]) -> Self {
        let key = ctl::probe_key(exporter);
        let token = exp.token;
        let mut rx = hub.listen_probes(token);
        let raw = node.raw.clone();
        let candidates = exp.candidates.clone();
        let latest = Arc::new(Mutex::new(None));
        let (ack_tx, acked) = tokio::sync::watch::channel(None);
        let l = latest.clone();
        let task = tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            let mut next = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(next) => {
                        let p = ctl::encode_probe(Kind::Probe, &token, nonce(), &key);
                        for a in &candidates {
                            let _ = raw.send(*a, &p).await;
                        }
                        next += if started.elapsed() < FAST_FOR { FAST } else { SLOW };
                    }
                    m = rx.recv() => {
                        let Some((from, data)) = m else { return };
                        match ctl::decode_probe(&data, &key) {
                            Some((Kind::Probe, _, n)) => {
                                *l.lock().expect("poisoned") = Some(from);
                                let ack = ctl::encode_probe(Kind::ProbeAck, &token, n, &key);
                                let _ = raw.send(from, &ack).await;
                            }
                            Some((Kind::ProbeAck, _, _)) => {
                                *l.lock().expect("poisoned") = Some(from);
                                ack_tx.send_replace(Some(from));
                            }
                            _ => {} // forged or for another session
                        }
                    }
                }
            }
        });
        Self {
            hub: hub.clone(),
            token,
            latest,
            acked,
            task,
        }
    }

    /// Wait until a probe of ours is answered; returns where from.
    pub async fn first_ack(&mut self) -> Option<SocketAddr> {
        self.acked
            .wait_for(|a| a.is_some())
            .await
            .ok()
            .and_then(|a| *a)
    }

    pub fn latest(&self) -> Option<SocketAddr> {
        *self.latest.lock().expect("poisoned")
    }
}

/// Host side of [`relay_proto::v1::PathSwitch`]: send to `direct` from now
/// on, and accept the peer from there.
pub fn host_switch(paths: &Paths, relay: SocketAddr, direct: SocketAddr) {
    paths.set(relay, direct, true, true);
    tracing::info!("session moved from relay {relay} to direct path {direct}");
}

/// Controller, before asking: accept datagrams from `direct` as the relay.
pub fn controller_prepare(paths: &Paths, relay: SocketAddr, direct: SocketAddr) {
    paths.set(relay, direct, false, true);
}

/// Controller, after `PathSwitched`: send direct as well.
pub fn controller_commit(paths: &Paths, relay: SocketAddr, direct: SocketAddr) {
    paths.set(relay, direct, true, true);
    tracing::info!("session moved from relay {relay} to direct path {direct}");
}

/// The exporter needed by [`Prober::start`].
pub fn exporter(conn: &quinn::Connection) -> anyhow::Result<[u8; 32]> {
    Ok(quic::exporter(conn)?)
}

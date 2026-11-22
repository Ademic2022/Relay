//! Reaching a Host by Device ID (FR-4, §8.2 steps 1–3).
//!
//! Host: keeps a registration open with the rendezvous server (keep-alive
//! every 15 s) and, when a Controller looks it up, punches towards the
//! Controller's addresses and binds at the relay in parallel.
//!
//! Controller: looks the Host up, punches and tries every candidate
//! address at once (LAN addresses first); the first QUIC handshake that
//! completes within 1.5 s wins. Otherwise both use the relay.
//!
//! All of this runs on the peer's single [`Node`] socket, so the NAT
//! mapping the server observed is the one the session uses.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use relay_crypto::DeviceId;
use relay_net::ctl::{self, Kind, Token};
use relay_net::demux::{ControlRx, RawSender};
use relay_net::quic::{self, Connection, Node};
use relay_net::tls::Expect;
use relay_proto::rdv;
use tokio::sync::{oneshot, watch};

use crate::identity::Me;

const MAX_MSG: usize = 16 * 1024;
const RDV_KEEPALIVE: Duration = Duration::from_secs(15);
const PUNCH_EVERY: Duration = Duration::from_millis(100);
const PUNCH_FOR: Duration = Duration::from_secs(3);

/// Rendezvous server address and its pinned key (FR-41).
#[derive(Debug, Clone, Copy)]
pub struct Server {
    pub addr: SocketAddr,
    pub key: [u8; 32],
}

/// How the session is carried (shown in the stats, FR-19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Direct,
    PeerToPeer,
    Relay,
}

impl Path {
    pub fn label(self) -> &'static str {
        match self {
            Path::Direct => "direct",
            Path::PeerToPeer => "P2P",
            Path::Relay => "relay",
        }
    }
}

/// What the host was told about an incoming Controller, kept so a
/// relayed session can later look for a direct path (FR-4).
#[derive(Debug, Clone)]
pub struct Expected {
    pub token: Token,
    pub candidates: Vec<SocketAddr>,
    pub relay: SocketAddr,
}

type ProbeTx = tokio::sync::mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>;

/// Dispatches control packets received on the node's socket.
#[derive(Clone, Default)]
pub struct ControlHub {
    bound: Arc<Mutex<HashMap<Token, oneshot::Sender<()>>>>,
    probes: Arc<Mutex<HashMap<Token, ProbeTx>>>,
    /// Host: introductions by Controller key.
    pub expected: Arc<Mutex<HashMap<[u8; 32], Expected>>>,
}

impl ControlHub {
    pub fn spawn(mut rx: ControlRx) -> Self {
        let hub = Self::default();
        let h = hub.clone();
        tokio::spawn(async move {
            while let Some((from, data)) = rx.recv().await {
                if let Some((Kind::Bound, token)) = ctl::decode(&data)
                    && let Some(tx) = h.bound.lock().expect("poisoned").remove(&token)
                {
                    let _ = tx.send(());
                } else if let Some(token) = ctl::probe_token(&data)
                    && let Some(tx) = h.probes.lock().expect("poisoned").get(&token)
                {
                    let _ = tx.send((from, data));
                }
                // PUNCH packets only exist to open NAT mappings.
            }
        });
        hub
    }

    /// Probe packets carrying `token` (unauthenticated; the listener checks).
    pub fn listen_probes(
        &self,
        token: Token,
    ) -> tokio::sync::mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.probes.lock().expect("poisoned").insert(token, tx);
        rx
    }

    pub fn stop_probes(&self, token: &Token) {
        self.probes.lock().expect("poisoned").remove(token);
    }

    fn wait_bound(&self, token: Token) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.bound.lock().expect("poisoned").insert(token, tx);
        rx
    }
}

/// Addresses peers may reach this node at directly (LAN candidates).
pub fn local_candidates(node: &Node) -> Vec<String> {
    let Ok(local) = node.endpoint.local_addr() else {
        return Vec::new();
    };
    if local.ip().is_unspecified() {
        crate::host::local_ips()
            .into_iter()
            .map(|ip| SocketAddr::new(ip, local.port()).to_string())
            .collect()
    } else {
        vec![local.to_string()]
    }
}

fn parse_candidates(peer: &rdv::PeerInfo) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    // LAN first (FR-4), then the address the server observed.
    for s in peer
        .local_addrs
        .iter()
        .chain(std::iter::once(&peer.observed_addr))
    {
        if let Ok(a) = s.parse::<SocketAddr>()
            && a.is_ipv4()
            && !out.contains(&a)
        {
            out.push(a);
        }
    }
    out
}

fn relay_addr(rv: &rdv::Rendezvous, server: &Server) -> anyhow::Result<SocketAddr> {
    let mut a: SocketAddr = rv
        .relay_addr
        .parse()
        .context("bad relay address from server")?;
    if a.ip().is_unspecified() {
        a.set_ip(server.addr.ip());
    }
    Ok(a)
}

fn token_of(rv: &rdv::Rendezvous) -> anyhow::Result<Token> {
    rv.token.as_ref().try_into().context("bad session token")
}

/// Send `PUNCH` to every candidate for a while, opening NAT mappings.
fn spawn_punch(raw: RawSender, token: Token, to: Vec<SocketAddr>) {
    tokio::spawn(async move {
        let pkt = ctl::encode(Kind::Punch, &token);
        let deadline = tokio::time::Instant::now() + PUNCH_FOR;
        while tokio::time::Instant::now() < deadline {
            for a in &to {
                let _ = raw.send(*a, &pkt).await;
            }
            tokio::time::sleep(PUNCH_EVERY).await;
        }
    });
}

/// Bind at the relay until it confirms (or `give_up`).
async fn bind_relay(
    raw: &RawSender,
    hub: &ControlHub,
    token: Token,
    relay: SocketAddr,
    give_up: Duration,
) -> bool {
    let mut bound = hub.wait_bound(token);
    let pkt = ctl::encode(Kind::Bind, &token);
    let deadline = tokio::time::Instant::now() + give_up;
    loop {
        let _ = raw.send(relay, &pkt).await;
        tokio::select! {
            r = &mut bound => return r.is_ok(),
            _ = tokio::time::sleep(Duration::from_millis(150)) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
    }
}

async fn rendezvous_conn(node: &Node, me: &Me, server: &Server) -> anyhow::Result<Connection> {
    let (cfg, _) = quic::client_config(
        &me.tls,
        Expect::Key(server.key),
        quic::ALPN_RENDEZVOUS,
        RDV_KEEPALIVE,
    )?;
    tokio::time::timeout(
        Duration::from_secs(5),
        quic::connect(&node.endpoint, cfg, server.addr),
    )
    .await
    .with_context(|| format!("rendezvous server {} not answering", server.addr))?
    .context("rendezvous server")
}

fn msg(m: rdv::message::Msg) -> rdv::Message {
    rdv::Message { msg: Some(m) }
}

// ------------------------------------------------------------ host

/// Keep this host registered; reconnects with backoff. Publishes the
/// assigned Device ID on `id_tx`.
pub async fn host_loop(
    node: Arc<Node>,
    hub: ControlHub,
    me: Arc<Me>,
    server: Server,
    id_tx: watch::Sender<Option<DeviceId>>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match host_once(&node, &hub, &me, &server, &id_tx).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => tracing::warn!("rendezvous: {e:#}; retrying in {}s", backoff.as_secs()),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn host_once(
    node: &Node,
    hub: &ControlHub,
    me: &Me,
    server: &Server,
    id_tx: &watch::Sender<Option<DeviceId>>,
) -> anyhow::Result<()> {
    let conn = rendezvous_conn(node, me, server).await?;
    let (mut tx, mut rx) = conn.open_bi().await?;
    quic::send_msg(
        &mut tx,
        &msg(rdv::message::Msg::Register(rdv::Register {
            protocol_version: relay_proto::PROTOCOL_VERSION,
            local_addrs: local_candidates(node),
        })),
    )
    .await?;
    loop {
        match quic::recv_msg::<rdv::Message>(&mut rx, MAX_MSG).await? {
            Some(rdv::Message {
                msg: Some(rdv::message::Msg::Registered(r)),
            }) => {
                let id = DeviceId::from_u32(r.device_id).context("server sent an invalid ID")?;
                id_tx.send_replace(Some(id));
                tracing::info!("registered as {id}, public address {}", r.observed_addr);
            }
            Some(rdv::Message {
                msg: Some(rdv::message::Msg::Incoming(rv)),
            }) => {
                let peer = rv.peer.clone().unwrap_or_default();
                tracing::info!("incoming connection request from {}", peer.device_id);
                let token = token_of(&rv)?;
                let candidates = parse_candidates(&peer);
                spawn_punch(node.raw.clone(), token, candidates.clone());
                let (raw, hub) = (node.raw.clone(), hub.clone());
                let relay = relay_addr(&rv, server)?;
                if let Ok(key) = <[u8; 32]>::try_from(peer.public_key.as_ref()) {
                    hub.expected.lock().expect("poisoned").insert(
                        key,
                        Expected {
                            token,
                            candidates,
                            relay,
                        },
                    );
                }
                // Bind at the relay too, in case P2P fails.
                tokio::spawn(async move {
                    bind_relay(&raw, &hub, token, relay, Duration::from_secs(5)).await;
                });
            }
            Some(_) => {}
            None => bail!("server closed the registration"),
        }
    }
}

// ------------------------------------------------------------ controller

/// Find `target` through the server and connect to it.
pub async fn connect_by_id(
    node: &Node,
    hub: &ControlHub,
    me: &Me,
    server: &Server,
    target: DeviceId,
    force_relay: bool,
) -> anyhow::Result<(Connection, Path, Option<Expected>)> {
    let rdv = rendezvous_conn(node, me, server).await?;
    let (mut tx, mut rx) = rdv.open_bi().await?;
    quic::send_msg(
        &mut tx,
        &msg(rdv::message::Msg::Lookup(rdv::Lookup {
            target_id: target.as_u32(),
            local_addrs: local_candidates(node),
        })),
    )
    .await?;
    let rv = match quic::recv_msg::<rdv::Message>(&mut rx, MAX_MSG).await? {
        Some(rdv::Message {
            msg: Some(rdv::message::Msg::LookupResult(r)),
        }) => match r.result {
            Some(rdv::lookup_result::Result::Ok(rv)) => rv,
            Some(rdv::lookup_result::Result::Error(e)) => bail!("{target}: {e}"),
            None => bail!("empty answer from server"),
        },
        _ => bail!("no answer from server"),
    };
    rdv.close(0u32.into(), b"done");
    let peer = rv.peer.clone().context("server sent no peer")?;
    let host_key: [u8; 32] = peer
        .public_key
        .as_ref()
        .try_into()
        .context("bad host key")?;
    let token = token_of(&rv)?;
    let relay = relay_addr(&rv, server)?;
    let (cfg, _) = quic::client_config(
        &me.tls,
        Expect::Key(host_key),
        quic::ALPN,
        Duration::from_secs(2),
    )?;

    let candidates = parse_candidates(&peer);
    if !force_relay {
        spawn_punch(node.raw.clone(), token, candidates.clone());
        let mut attempts = tokio::task::JoinSet::new();
        for (i, a) in candidates.iter().copied().enumerate() {
            let ep = node.endpoint.clone();
            let cfg = cfg.clone();
            attempts.spawn(async move {
                // Stagger slightly so a LAN path, if any, wins.
                tokio::time::sleep(Duration::from_millis(20 * i as u64)).await;
                quic::connect(&ep, cfg, a).await.map(|c| (c, a))
            });
        }
        let won = tokio::time::timeout(relay_net::PUNCH_TIMEOUT, async {
            while let Some(r) = attempts.join_next().await {
                if let Ok(Ok((c, a))) = r {
                    return Some((c, a));
                }
            }
            None
        })
        .await;
        attempts.abort_all();
        if let Ok(Some((conn, a))) = won {
            tracing::info!("connected peer-to-peer via {a}");
            return Ok((conn, Path::PeerToPeer, None));
        }
        tracing::info!(
            "no direct path within {:?}; using relay {relay}",
            relay_net::PUNCH_TIMEOUT
        );
    }

    if !bind_relay(&node.raw, hub, token, relay, Duration::from_secs(5)).await {
        bail!("relay {relay} did not answer");
    }
    let conn = tokio::time::timeout(
        Duration::from_secs(5),
        quic::connect(&node.endpoint, cfg, relay),
    )
    .await
    .context("connection through relay timed out")??;
    Ok((
        conn,
        Path::Relay,
        Some(Expected {
            token,
            candidates,
            relay,
        }),
    ))
}

/// Parse "host[:port]" for the server, defaulting to the rendezvous port.
pub fn parse_server(s: &str) -> anyhow::Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    let with_port = if s.parse::<SocketAddr>().is_ok()
        || s.rsplit_once(':')
            .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        s.to_string()
    } else {
        format!("{s}:{}", relay_net::RENDEZVOUS_PORT)
    };
    with_port
        .to_socket_addrs()?
        .find(|a| a.is_ipv4())
        .with_context(|| format!("cannot resolve {s}"))
}

#[cfg(test)]
mod tests {
    //! End-to-end over localhost: rendezvous, P2P, relay, OPAQUE.
    use super::*;
    use crate::auth;
    use relay_crypto::pake::{HostPassword, PasswordKind};
    use relay_proto::v1;

    const MARKER: &[u8] = b"PLAINTEXT-MARKER-the-relay-must-never-see-this";

    fn lo() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    struct World {
        server: Server,
        host_id: DeviceId,
        tap: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        _host: Arc<Node>,
    }

    async fn world(password: &'static str) -> World {
        let dir = std::env::temp_dir().join(format!(
            "relay-e2e-{}",
            std::process::id() as u64 * 1000 + rand_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sid = relay_server::store::load_or_create_identity(&dir).unwrap();
        let tokens = relay_server::Tokens::default();
        let (tap_tx, tap) = tokio::sync::mpsc::unbounded_channel();
        let relay = relay_server::relay::start_tapped(lo(), tokens.clone(), 100_000, Some(tap_tx))
            .await
            .unwrap();
        let rdv_addr = relay_server::rendezvous::start(
            lo(),
            &sid,
            relay_server::store::Devices::open(&dir).unwrap(),
            tokens,
            SocketAddr::from(([0, 0, 0, 0], relay.port())),
        )
        .await
        .unwrap();
        let server = Server {
            addr: rdv_addr,
            key: sid.public_key,
        };

        // Host: registers, accepts one session at a time, echoes a stream.
        let me = Arc::new(crate::identity::ephemeral());
        let (node, control) = quic::node(lo(), &me.tls, Some(&[quic::ALPN])).unwrap();
        let node = Arc::new(node);
        let hub = ControlHub::spawn(control);
        let (id_tx, mut id_rx) = watch::channel(None);
        tokio::spawn(host_loop(
            node.clone(),
            hub.clone(),
            me.clone(),
            server,
            id_tx,
        ));
        let ep = node.endpoint.clone();
        let hnode = node.clone();
        tokio::spawn(async move {
            let verifier = HostPassword::new(password, PasswordKind::OneTime).unwrap();
            while let Some(i) = ep.accept().await {
                let Ok(conn) = i.await else { continue };
                let (mut tx, mut rx) = conn.accept_bi().await.unwrap();
                let peer = auth::host_hello(&conn, &mut rx).await.unwrap();
                match auth::host_login(&conn, &mut tx, &mut rx, &me, &peer, |_| Some(&verifier))
                    .await
                    .unwrap()
                {
                    auth::HostAuth::Ok(_) => auth::send_outcome(&mut tx, v1::AuthResult::Ok, 0)
                        .await
                        .unwrap(),
                    auth::HostAuth::BadPassword | auth::HostAuth::NotAccepted(_) => {
                        auth::send_outcome(&mut tx, v1::AuthResult::BadPassword, 0)
                            .await
                            .unwrap();
                        continue;
                    }
                }
                // Relayed? Look for a direct path and switch on request.
                let exp = hub.expected.lock().unwrap().get(&peer.key).cloned();
                let relayed = exp
                    .as_ref()
                    .is_some_and(|e| e.relay == conn.remote_address());
                let prober = if relayed {
                    let exporter = crate::migrate::exporter(&conn).unwrap();
                    Some(crate::migrate::Prober::start(
                        &hnode,
                        &hub,
                        exp.as_ref().unwrap(),
                        &exporter,
                    ))
                } else {
                    None
                };
                // Answer a switch request whenever it comes, while the
                // session carries on.
                let switch = prober.map(|p| {
                    let hnode = hnode.clone();
                    let relay = exp.clone().unwrap().relay;
                    tokio::spawn(async move {
                        let m: Option<v1::ControlMessage> =
                            quic::recv_msg(&mut rx, 1024).await.ok().flatten();
                        if matches!(
                            m,
                            Some(v1::ControlMessage {
                                msg: Some(v1::control_message::Msg::PathSwitch(_))
                            })
                        ) && let Some(direct) = p.latest()
                        {
                            crate::migrate::host_switch(&hnode.paths, relay, direct);
                            let _ = quic::send_msg(
                                &mut tx,
                                &auth::control(v1::control_message::Msg::PathSwitched(
                                    v1::PathSwitched {},
                                )),
                            )
                            .await;
                        }
                        // Keep the prober and stream alive for the session.
                        let _ = std::future::pending::<()>().await;
                        drop(p);
                    })
                });
                // Echo one uni stream back.
                if let Ok(mut s) = conn.accept_uni().await {
                    let data = s.read_to_end(1 << 16).await.unwrap();
                    let mut out = conn.open_uni().await.unwrap();
                    out.write_all(&data).await.unwrap();
                    out.finish().unwrap();
                    let _ = conn.closed().await;
                }
                if let Some(t) = switch {
                    t.abort();
                }
                hnode.paths.clear(conn.remote_address());
            }
        });
        let host_id = tokio::time::timeout(Duration::from_secs(5), id_rx.wait_for(|v| v.is_some()))
            .await
            .expect("host registers")
            .unwrap()
            .expect("id");
        World {
            server,
            host_id,
            tap,
            _host: node,
        }
    }

    fn rand_suffix() -> u64 {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).unwrap();
        u64::from(u32::from_le_bytes(b))
    }

    async fn controller(
        w: &mut World,
        password: &str,
        force_relay: bool,
    ) -> anyhow::Result<(Path, Vec<u8>)> {
        let me = crate::identity::ephemeral();
        let (node, control) = quic::node(lo(), &me.tls, None)?;
        let hub = ControlHub::spawn(control);
        let (conn, mut path, exp) =
            connect_by_id(&node, &hub, &me, &w.server, w.host_id, force_relay).await?;
        let (mut tx, mut rx) = conn.open_bi().await?;
        auth::controller(
            &conn,
            &mut tx,
            &mut rx,
            &me,
            "test",
            auth::Login::password(password),
        )
        .await?;
        if let Some(exp) = exp {
            // Relay → P2P switch (FR-4).
            let exporter = crate::migrate::exporter(&conn)?;
            let mut prober = crate::migrate::Prober::start(&node, &hub, &exp, &exporter);
            let direct = tokio::time::timeout(Duration::from_secs(5), prober.first_ack())
                .await?
                .expect("probe answered");
            crate::migrate::controller_prepare(&node.paths, exp.relay, direct);
            quic::send_msg(
                &mut tx,
                &auth::control(v1::control_message::Msg::PathSwitch(v1::PathSwitch {})),
            )
            .await?;
            let m: Option<v1::ControlMessage> = quic::recv_msg(&mut rx, 1024).await?;
            assert!(matches!(
                m,
                Some(v1::ControlMessage {
                    msg: Some(v1::control_message::Msg::PathSwitched(_))
                })
            ));
            crate::migrate::controller_commit(&node.paths, exp.relay, direct);
            path = Path::PeerToPeer;
            // Let in-flight relayed packets drain, then forget them.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let mut before = 0;
            while w.tap.try_recv().is_ok() {
                before += 1;
            }
            assert!(
                before > 5,
                "session was relayed before the switch ({before} packets)"
            );
        }
        let mut s = conn.open_uni().await?;
        s.write_all(MARKER).await?;
        s.finish()?;
        let mut back = conn.accept_uni().await?;
        let echoed = back.read_to_end(1 << 16).await?;
        conn.close(0u32.into(), b"bye");
        Ok((path, echoed))
    }

    #[tokio::test]
    async fn p2p_when_reachable() {
        let mut w = world("Pw3dEf7h").await;
        let (path, echoed) = controller(&mut w, "Pw3dEf7h", false).await.unwrap();
        assert_eq!(path, Path::PeerToPeer);
        assert_eq!(echoed, MARKER);
    }

    #[tokio::test]
    async fn relayed_session_switches_to_p2p() {
        let mut w = world("Pw3dEf7h").await;
        let (path, echoed) = controller(&mut w, "Pw3dEf7h", true).await.unwrap();
        assert_eq!(path, Path::PeerToPeer, "switched off the relay");
        assert_eq!(echoed, MARKER, "session survived the switch");
        let mut after = Vec::new();
        while let Ok(p) = w.tap.try_recv() {
            after.push(p);
        }
        assert!(
            after.len() <= 2,
            "relay still carrying traffic after the switch: {}",
            after.len()
        );
    }

    #[tokio::test]
    async fn relay_sees_only_ciphertext() {
        // Relayed handshake, auth and probing all cross the relay; none of
        // it may contain the password. (The marker is sent after the
        // switch in this flow, so check it on a relayed-only session.)
        let mut w = world("Pw3dEf7h").await;
        let me = crate::identity::ephemeral();
        let (node, control) = quic::node(lo(), &me.tls, None).unwrap();
        let hub = ControlHub::spawn(control);
        let (conn, path, _) = connect_by_id(&node, &hub, &me, &w.server, w.host_id, true)
            .await
            .unwrap();
        assert_eq!(path, Path::Relay);
        let (mut tx, mut rx) = conn.open_bi().await.unwrap();
        auth::controller(
            &conn,
            &mut tx,
            &mut rx,
            &me,
            "test",
            auth::Login::password("Pw3dEf7h"),
        )
        .await
        .unwrap();
        let mut s = conn.open_uni().await.unwrap();
        s.write_all(MARKER).await.unwrap();
        s.finish().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut forwarded = Vec::new();
        while let Ok(p) = w.tap.try_recv() {
            forwarded.push(p);
        }
        assert!(forwarded.len() > 5, "traffic went through the relay");
        for p in &forwarded {
            assert!(
                !p.windows(MARKER.len()).any(|win| win == MARKER),
                "relay saw plaintext"
            );
            assert!(
                !p.windows(8).any(|win| win == b"Pw3dEf7h"),
                "relay saw the password"
            );
        }
    }

    #[tokio::test]
    async fn wrong_password_is_refused() {
        let mut w = world("Pw3dEf7h").await;
        let err = controller(&mut w, "nope-nope", false).await.unwrap_err();
        assert!(err.to_string().contains("wrong password"), "{err:#}");
    }

    #[tokio::test]
    async fn unknown_id_is_reported() {
        let w = world("Pw3dEf7h").await;
        let me = crate::identity::ephemeral();
        let (node, control) = quic::node(lo(), &me.tls, None).unwrap();
        let hub = ControlHub::spawn(control);
        let other = DeviceId::from_u32(111_111_111).unwrap();
        let err = connect_by_id(&node, &hub, &me, &w.server, other, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown Device ID"), "{err:#}");
    }

    #[tokio::test]
    async fn wrong_server_key_is_refused() {
        let w = world("Pw3dEf7h").await;
        let me = crate::identity::ephemeral();
        let (node, control) = quic::node(lo(), &me.tls, None).unwrap();
        let hub = ControlHub::spawn(control);
        let fake = Server {
            addr: w.server.addr,
            key: [7; 32],
        };
        assert!(
            connect_by_id(&node, &hub, &me, &fake, w.host_id, false)
                .await
                .is_err()
        );
    }
}

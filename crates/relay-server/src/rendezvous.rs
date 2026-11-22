//! Rendezvous service (§8.2 steps 1–2).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use relay_net::quic::{self, Connection};
use relay_net::tls::Identity;
use relay_proto::rdv;
use tokio::sync::mpsc;

use crate::Tokens;
use crate::store::Devices;

const MAX_MSG: usize = 16 * 1024;

struct HostEntry {
    key: [u8; 32],
    conn_id: usize,
    observed: SocketAddr,
    local: Vec<String>,
    tx: mpsc::UnboundedSender<rdv::Message>,
}

#[derive(Clone)]
struct State {
    devices: Devices,
    tokens: Tokens,
    hosts: Arc<Mutex<HashMap<u32, HostEntry>>>,
    public_relay: SocketAddr,
}

pub async fn start(
    bind: SocketAddr,
    identity: &Identity,
    devices: Devices,
    tokens: Tokens,
    public_relay: SocketAddr,
) -> anyhow::Result<SocketAddr> {
    let (node, _control) = quic::node(bind, identity, Some(&[quic::ALPN_RENDEZVOUS]))?;
    let addr = node.endpoint.local_addr()?;
    let state = State {
        devices,
        tokens,
        hosts: Arc::default(),
        public_relay,
    };
    tokio::spawn(async move {
        let _keep = node.raw;
        while let Some(incoming) = node.endpoint.accept().await {
            let state = state.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        if let Err(e) = serve(conn, state).await {
                            tracing::debug!("rendezvous peer: {e:#}");
                        }
                    }
                    Err(e) => tracing::debug!("rendezvous handshake: {e}"),
                }
            });
        }
    });
    Ok(addr)
}

fn msg(m: rdv::message::Msg) -> rdv::Message {
    rdv::Message { msg: Some(m) }
}

async fn serve(conn: Connection, st: State) -> anyhow::Result<()> {
    let key =
        relay_net::tls::peer_public_key(&conn).ok_or_else(|| anyhow::anyhow!("no peer key"))?;
    let observed = conn.remote_address();
    let (mut tx, mut rx) =
        tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await??;
    let first: rdv::Message = quic::recv_msg(&mut rx, MAX_MSG)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no request"))?;
    match first.msg {
        Some(rdv::message::Msg::Register(r)) => {
            let id = st.devices.id_for(&key)?;
            let (out_tx, mut out_rx) = mpsc::unbounded_channel();
            let conn_id = conn.stable_id();
            st.hosts.lock().expect("poisoned").insert(
                id.as_u32(),
                HostEntry {
                    key,
                    conn_id,
                    observed,
                    local: r.local_addrs,
                    tx: out_tx,
                },
            );
            tracing::info!("host {id} online from {observed}");
            quic::send_msg(
                &mut tx,
                &msg(rdv::message::Msg::Registered(rdv::Registered {
                    device_id: id.as_u32(),
                    observed_addr: observed.to_string(),
                })),
            )
            .await?;
            // Relay introductions to the host until it goes away.
            loop {
                tokio::select! {
                    m = out_rx.recv() => match m {
                        Some(m) => quic::send_msg(&mut tx, &m).await?,
                        None => break,
                    },
                    m = quic::recv_msg::<rdv::Message>(&mut rx, MAX_MSG) => match m {
                        Ok(Some(rdv::Message { msg: Some(rdv::message::Msg::Register(r)) })) => {
                            if let Some(h) = st.hosts.lock().expect("poisoned").get_mut(&id.as_u32()) {
                                h.local = r.local_addrs;
                            }
                        }
                        Ok(Some(_)) => {}
                        _ => break,
                    },
                    _ = conn.closed() => break,
                }
            }
            let mut hosts = st.hosts.lock().expect("poisoned");
            if hosts
                .get(&id.as_u32())
                .is_some_and(|h| h.conn_id == conn_id)
            {
                hosts.remove(&id.as_u32());
                tracing::info!("host {id} offline");
            }
            Ok(())
        }
        Some(rdv::message::Msg::Lookup(l)) => {
            let ctl_id = st.devices.id_for(&key)?;
            let result = {
                let hosts = st.hosts.lock().expect("poisoned");
                match hosts.get(&l.target_id) {
                    None => rdv::lookup_result::Result::Error(
                        if st.devices.key_of(l.target_id).is_some() {
                            "device is offline".into()
                        } else {
                            "unknown Device ID".into()
                        },
                    ),
                    Some(h) => {
                        let token = st.tokens.issue();
                        let relay = st.public_relay.to_string();
                        let _ = h.tx.send(msg(rdv::message::Msg::Incoming(rdv::Rendezvous {
                            peer: Some(rdv::PeerInfo {
                                device_id: ctl_id.as_u32(),
                                public_key: key.to_vec().into(),
                                observed_addr: observed.to_string(),
                                local_addrs: l.local_addrs,
                            }),
                            token: token.to_vec().into(),
                            relay_addr: relay.clone(),
                        })));
                        rdv::lookup_result::Result::Ok(rdv::Rendezvous {
                            peer: Some(rdv::PeerInfo {
                                device_id: l.target_id,
                                public_key: h.key.to_vec().into(),
                                observed_addr: h.observed.to_string(),
                                local_addrs: h.local.clone(),
                            }),
                            token: token.to_vec().into(),
                            relay_addr: relay,
                        })
                    }
                }
            };
            quic::send_msg(
                &mut tx,
                &msg(rdv::message::Msg::LookupResult(rdv::LookupResult {
                    result: Some(result),
                })),
            )
            .await?;
            let _ = tx.finish();
            // Let the reply drain before the connection drops.
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            Ok(())
        }
        _ => anyhow::bail!("unexpected first message"),
    }
}

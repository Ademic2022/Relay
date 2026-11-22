//! QUIC endpoints (§5.1, §8).
//!
//! One connection carries everything (§8.1): reliable streams for control,
//! input, chat and files, unreliable datagrams for video. Endpoints run on
//! a [`DemuxSocket`](crate::demux::DemuxSocket) so the same UDP port also
//! carries rendezvous, hole-punch and relay control packets.
//!
//! Trust: both sides present certificates bound to their device keys
//! ([`crate::tls`]). The Controller checks the Host's key against the key
//! the rendezvous server returned, a pinned key, or records it on first
//! contact; the Host learns the Controller's verified key from TLS.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Endpoint, EndpointConfig, TransportConfig};

pub use quinn::{Connection, RecvStream, SendStream};

use crate::demux::{ControlRx, DemuxSocket, RawSender};
use crate::tls::{ClientKeyVerifier, Expect, Identity, SeenKey, ServerKeyVerifier, provider};

/// ALPN for Controller ↔ Host sessions.
pub const ALPN: &[u8] = b"relay/1";
/// ALPN for peers ↔ rendezvous server.
pub const ALPN_RENDEZVOUS: &[u8] = b"relay-rdv/1";
/// TLS server name used on every connection (identity is the key, not a name).
pub const SERVER_NAME: &str = "relay";

/// Datagram send buffer. Small on purpose: when the path is congested,
/// quinn drops the oldest queued datagrams instead of building latency
/// (§6 rule 3). ~128 KB ≈ one 1080p keyframe at Speed-mode bitrates.
const DATAGRAM_SEND_BUFFER: usize = 128 * 1024;
const DATAGRAM_RECV_BUFFER: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum QuicError {
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("identity: {0}")]
    Identity(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("connect: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("config: {0}")]
    Config(String),
}

/// Transport settings. `keep_alive`: 2 s for sessions; 15 s for the
/// rendezvous connection (§8.2 step 1).
pub fn transport(keep_alive: Duration) -> TransportConfig {
    let mut t = TransportConfig::default();
    t.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
    t.datagram_receive_buffer_size(Some(DATAGRAM_RECV_BUFFER));
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    t.keep_alive_interval(Some(keep_alive));
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(keep_alive.max(Duration::from_secs(5)) * 3).expect("valid"),
    ));
    t
}

/// An endpoint on its own UDP socket that can both accept (if
/// `accept_alpns` is given) and initiate connections.
#[derive(Debug)]
pub struct Node {
    pub endpoint: Endpoint,
    pub raw: RawSender,
    /// Path virtualisation for relay → P2P switching.
    pub paths: crate::demux::Paths,
}

pub fn node(
    bind: SocketAddr,
    identity: &Identity,
    accept_alpns: Option<&[&[u8]]>,
) -> Result<(Node, ControlRx), QuicError> {
    let sock = std::net::UdpSocket::bind(bind)?;
    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);
    let inner = runtime.wrap_udp_socket(sock)?;
    let (demux, control) = DemuxSocket::new(inner);
    let raw = demux.raw_sender();
    let paths = demux.paths();

    let server = match accept_alpns {
        Some(alpns) => {
            let mut tls = rustls::ServerConfig::builder_with_provider(provider())
                .with_protocol_versions(&[&rustls::version::TLS13])?
                .with_client_cert_verifier(Arc::new(ClientKeyVerifier {
                    provider: provider(),
                }))
                .with_single_cert(vec![identity.cert.clone()], identity.key())?;
            tls.alpn_protocols = alpns.iter().map(|a| a.to_vec()).collect();
            tls.max_early_data_size = 0;
            let crypto =
                QuicServerConfig::try_from(tls).map_err(|e| QuicError::Config(e.to_string()))?;
            let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
            cfg.transport_config(Arc::new(transport(Duration::from_secs(2))));
            Some(cfg)
        }
        None => None,
    };
    let endpoint =
        Endpoint::new_with_abstract_socket(EndpointConfig::default(), server, demux, runtime)?;
    Ok((
        Node {
            endpoint,
            raw,
            paths,
        },
        control,
    ))
}

/// Client configuration presenting `identity` and checking the server key.
pub fn client_config(
    identity: &Identity,
    expect: Expect,
    alpn: &[u8],
    keep_alive: Duration,
) -> Result<(quinn::ClientConfig, SeenKey), QuicError> {
    let seen = SeenKey::default();
    let verifier = Arc::new(ServerKeyVerifier {
        provider: provider(),
        expect,
        seen: seen.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert.clone()], identity.key())?;
    tls.alpn_protocols = vec![alpn.to_vec()];
    let crypto = QuicClientConfig::try_from(tls).map_err(|e| QuicError::Config(e.to_string()))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport(keep_alive)));
    Ok((cfg, seen))
}

pub async fn connect(
    ep: &Endpoint,
    cfg: quinn::ClientConfig,
    addr: SocketAddr,
) -> Result<Connection, QuicError> {
    Ok(ep.connect_with(cfg, addr, SERVER_NAME)?.await?)
}

/// 32 bytes of TLS keying material unique to this connection, for
/// channel-binding the password exchange (§8.2 step 5).
pub fn exporter(conn: &Connection) -> Result<[u8; 32], QuicError> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, b"EXPORTER-relay-opaque-v1", b"")
        .map_err(|_| QuicError::Config("TLS exporter unavailable".into()))?;
    Ok(out)
}

// ------------------------------------------------------------ message framing

/// Write one length-delimited protobuf message to a reliable stream.
pub async fn send_msg<M: prost::Message>(s: &mut SendStream, msg: &M) -> std::io::Result<()> {
    s.write_all(&msg.encode_length_delimited_to_vec())
        .await
        .map_err(std::io::Error::other)
}

/// Read one length-delimited protobuf message. `Ok(None)` on clean EOF.
pub async fn recv_msg<M: prost::Message + Default>(
    s: &mut RecvStream,
    max_len: usize,
) -> std::io::Result<Option<M>> {
    let mut len: u64 = 0;
    for i in 0..10 {
        let mut b = [0u8; 1];
        match s.read_exact(&mut b).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(0)) if i == 0 => return Ok(None),
            Err(e) => return Err(std::io::Error::other(e)),
        }
        len |= u64::from(b[0] & 0x7f) << (7 * i);
        if b[0] & 0x80 == 0 {
            let len = usize::try_from(len).unwrap_or(usize::MAX);
            if len > max_len {
                return Err(std::io::Error::other(format!(
                    "message of {len} bytes too large"
                )));
            }
            let mut buf = vec![0u8; len];
            s.read_exact(&mut buf)
                .await
                .map_err(std::io::Error::other)?;
            return M::decode(&buf[..]).map(Some).map_err(std::io::Error::other);
        }
    }
    Err(std::io::Error::other("bad length prefix"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[derive(Clone, PartialEq, prost::Message)]
    struct Ping {
        #[prost(string, tag = "1")]
        text: String,
    }

    fn any() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    #[tokio::test]
    async fn mutual_tls_streams_and_datagrams() {
        let (host_id, ctl_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (host, _) = node(any(), &host_id, Some(&[ALPN])).unwrap();
        let addr = host.endpoint.local_addr().unwrap();
        let ctl_pk = ctl_id.public_key;
        let srv = tokio::spawn(async move {
            let conn = host.endpoint.accept().await.unwrap().await.unwrap();
            assert_eq!(
                crate::tls::peer_public_key(&conn),
                Some(ctl_pk),
                "host sees controller key"
            );
            let (mut tx, mut rx) = conn.accept_bi().await.unwrap();
            let m: Ping = recv_msg(&mut rx, 1024).await.unwrap().unwrap();
            send_msg(
                &mut tx,
                &Ping {
                    text: m.text + " pong",
                },
            )
            .await
            .unwrap();
            conn.send_datagram(Bytes::from_static(b"frame")).unwrap();
            let e = exporter(&conn).unwrap();
            conn.closed().await;
            e
        });

        let (ctl, _) = node(any(), &ctl_id, None).unwrap();
        let (cfg, seen) = client_config(
            &ctl_id,
            Expect::Key(host_id.public_key),
            ALPN,
            Duration::from_secs(2),
        )
        .unwrap();
        let conn = connect(&ctl.endpoint, cfg, addr).await.unwrap();
        assert_eq!(seen.get(), Some(host_id.public_key));
        let (mut tx, mut rx) = conn.open_bi().await.unwrap();
        send_msg(
            &mut tx,
            &Ping {
                text: "ping".into(),
            },
        )
        .await
        .unwrap();
        let back: Ping = recv_msg(&mut rx, 1024).await.unwrap().unwrap();
        assert_eq!(back.text, "ping pong");
        assert_eq!(&conn.read_datagram().await.unwrap()[..], b"frame");
        let mine = exporter(&conn).unwrap();
        conn.close(0u32.into(), b"done");
        assert_eq!(
            srv.await.unwrap(),
            mine,
            "both ends derive the same exporter"
        );
    }

    #[tokio::test]
    async fn wrong_host_key_is_refused() {
        let (host_id, ctl_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (host, _) = node(any(), &host_id, Some(&[ALPN])).unwrap();
        let addr = host.endpoint.local_addr().unwrap();
        tokio::spawn(async move {
            while let Some(i) = host.endpoint.accept().await {
                let _ = i.await;
            }
        });
        let (ctl, _) = node(any(), &ctl_id, None).unwrap();
        let (cfg, _) =
            client_config(&ctl_id, Expect::Key([9; 32]), ALPN, Duration::from_secs(2)).unwrap();
        assert!(connect(&ctl.endpoint, cfg, addr).await.is_err());
    }

    #[tokio::test]
    async fn control_packets_bypass_quic() {
        let id = Identity::generate().unwrap();
        let (a, mut a_ctl) = node(any(), &id, Some(&[ALPN])).unwrap();
        let (b, _) = node(any(), &id, None).unwrap();
        let pkt = crate::ctl::encode(crate::ctl::Kind::Punch, &[5; 16]);
        b.raw
            .send(a.endpoint.local_addr().unwrap(), &pkt)
            .await
            .unwrap();
        let (from, data) = tokio::time::timeout(Duration::from_secs(2), a_ctl.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from, b.endpoint.local_addr().unwrap());
        assert_eq!(data, pkt);
    }

    #[tokio::test]
    async fn rejects_oversized_message() {
        let (host_id, ctl_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (host, _) = node(any(), &host_id, Some(&[ALPN])).unwrap();
        let addr = host.endpoint.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let conn = host.endpoint.accept().await.unwrap().await.unwrap();
            let (_tx, mut rx) = conn.accept_bi().await.unwrap();
            recv_msg::<Ping>(&mut rx, 8).await
        });
        let (ctl, _) = node(any(), &ctl_id, None).unwrap();
        let (cfg, _) =
            client_config(&ctl_id, Expect::AnyRecord, ALPN, Duration::from_secs(2)).unwrap();
        let conn = connect(&ctl.endpoint, cfg, addr).await.unwrap();
        let (mut tx, _rx) = conn.open_bi().await.unwrap();
        send_msg(
            &mut tx,
            &Ping {
                text: "x".repeat(100),
            },
        )
        .await
        .unwrap();
        assert!(srv.await.unwrap().is_err());
    }
}

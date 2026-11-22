//! QUIC endpoints for direct connections (M1, FR-7 direct IP).
//!
//! One connection carries everything (§8.1): reliable streams for control,
//! input, chat and files, unreliable datagrams for video.
//!
//! M1 trust model: the Host presents an ephemeral self-signed certificate
//! and the Controller accepts any certificate, printing its fingerprint.
//! The session code sent over the encrypted control stream gates access.
//! This is NOT MITM-resistant on a hostile network; M3 replaces it with
//! certificates bound to device Ed25519 keys and OPAQUE bound to the TLS
//! exporter (§8.2 steps 4–5).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Endpoint, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

pub use quinn::{Connection, RecvStream, SendStream};

pub const ALPN: &[u8] = b"relay/1";

/// Datagram send buffer. Small on purpose: when the path is congested,
/// quinn drops the oldest queued datagrams instead of building latency
/// (§6 rule 3). ~128 KB ≈ one 1080p keyframe at Speed-mode bitrates.
const DATAGRAM_SEND_BUFFER: usize = 128 * 1024;
const DATAGRAM_RECV_BUFFER: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum QuicError {
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate generation: {0}")]
    Cert(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("connect: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("config: {0}")]
    Config(String),
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport() -> TransportConfig {
    let mut t = TransportConfig::default();
    t.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
    t.datagram_receive_buffer_size(Some(DATAGRAM_RECV_BUFFER));
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(10)).expect("valid"),
    ));
    t
}

/// Host endpoint listening on `bind`. Returns the endpoint and the
/// certificate's SHA-256 fingerprint (hex) for display.
pub fn server(bind: SocketAddr) -> Result<(Endpoint, String), QuicError> {
    let ck = rcgen::generate_simple_self_signed(vec!["relay-host".into()])
        .map_err(|e| QuicError::Cert(e.to_string()))?;
    let cert_der: CertificateDer<'static> = ck.cert.der().clone();
    let fp = fingerprint(&cert_der);
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));

    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;

    let crypto = QuicServerConfig::try_from(tls).map_err(|e| QuicError::Config(e.to_string()))?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport()));
    Ok((Endpoint::server(cfg, bind)?, fp))
}

/// Controller endpoint. The returned [`SeenCert`] holds the fingerprint of
/// the certificate the server presented.
pub fn client() -> Result<(Endpoint, SeenCert), QuicError> {
    let seen = SeenCert::default();
    let verifier = Arc::new(AcceptAnyM1 {
        provider: provider(),
        seen: seen.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let crypto = QuicClientConfig::try_from(tls).map_err(|e| QuicError::Config(e.to_string()))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport()));
    let bind: SocketAddr = "[::]:0".parse().expect("valid");
    let mut ep = Endpoint::client(bind)
        .or_else(|_| Endpoint::client("0.0.0.0:0".parse().expect("valid")))?;
    ep.set_default_client_config(cfg);
    Ok((ep, seen))
}

/// Connect to a Host.
pub async fn connect(ep: &Endpoint, addr: SocketAddr) -> Result<Connection, QuicError> {
    Ok(ep.connect(addr, "relay-host")?.await?)
}

pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let d = ring_sha256(cert.as_ref());
    d.iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn ring_sha256(data: &[u8]) -> Vec<u8> {
    // blake3 would do, but SHA-256 matches what users see in other tools.
    let h = rustls::crypto::ring::default_provider()
        .cipher_suites
        .first()
        .and_then(|s| s.tls13())
        .map(|s| s.common.hash_provider.hash(data));
    h.map(|o| o.as_ref().to_vec()).unwrap_or_default()
}

/// Fingerprint of the certificate seen during the last handshake.
#[derive(Debug, Clone, Default)]
pub struct SeenCert(Arc<Mutex<Option<String>>>);

impl SeenCert {
    pub fn get(&self) -> Option<String> {
        self.0.lock().expect("poisoned").clone()
    }
}

/// M1 only: accept any server certificate but still check that the
/// handshake signature is valid for it. Replaced by key pinning in M3.
#[derive(Debug)]
struct AcceptAnyM1 {
    provider: Arc<CryptoProvider>,
    seen: SeenCert,
}

impl ServerCertVerifier for AcceptAnyM1 {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.0.lock().expect("poisoned") = Some(fingerprint(end_entity));
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
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

    #[tokio::test]
    async fn loopback_streams_and_datagrams() {
        let (server_ep, fp) = server("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server_ep.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let conn = server_ep.accept().await.unwrap().await.unwrap();
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
            conn.closed().await;
        });

        let (ep, seen) = client().unwrap();
        let conn = connect(&ep, addr).await.unwrap();
        assert_eq!(seen.get().as_deref(), Some(fp.as_str()));
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
        conn.close(0u32.into(), b"done");
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_oversized_message() {
        let (server_ep, _) = server("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server_ep.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let conn = server_ep.accept().await.unwrap().await.unwrap();
            let (_tx, mut rx) = conn.accept_bi().await.unwrap();
            recv_msg::<Ping>(&mut rx, 8).await
        });
        let (ep, _) = client().unwrap();
        let conn = connect(&ep, addr).await.unwrap();
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

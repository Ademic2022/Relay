//! LAN discovery by Device ID (FR-4, first step: "try LAN discovery").
//!
//! Lets `relay connect 379 539 367` work on the same network without a
//! rendezvous server.
//!
//! - The viewer broadcasts `RLYQ` + Device ID (u32 LE) to UDP [`PORT`].
//! - A host whose device key derives that ID answers `RLYA` + ID + its
//!   QUIC port (u16 LE), from its own address.
//!
//! The answer is only a hint about *where* to connect. The viewer then
//! checks in the TLS handshake that the host's key really derives to the
//! ID ([`key_matches`]), so another machine on the LAN cannot answer for
//! an ID it does not own. Nothing secret is sent.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use relay_crypto::DeviceId;
use tokio::net::UdpSocket;

pub const PORT: u16 = 21119;
const QUERY: &[u8; 4] = b"RLYQ";
const ANSWER: &[u8; 4] = b"RLYA";
/// Nonces the rendezvous server may have used for this key (collisions).
const NONCES: u32 = 8;

/// Does `key` own `id`?
pub fn key_matches(key: &[u8; 32], id: DeviceId) -> bool {
    (0..NONCES).any(|n| DeviceId::derive(key, n) == id)
}

fn query(id: DeviceId) -> [u8; 8] {
    let mut q = [0u8; 8];
    q[..4].copy_from_slice(QUERY);
    q[4..].copy_from_slice(&id.as_u32().to_le_bytes());
    q
}

/// Parse an answer for `id`; returns the host's QUIC port.
fn parse_answer(b: &[u8], id: DeviceId) -> Option<u16> {
    (b.len() == 10 && &b[..4] == ANSWER && b[4..8] == id.as_u32().to_le_bytes())
        .then(|| u16::from_le_bytes([b[8], b[9]]))
}

fn shared_socket() -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Several Relay hosts (agent + user host) may share one machine.
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.set_nonblocking(true)?;
    s.bind(&SocketAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT)).into())?;
    Ok(s.into())
}

/// Host side: answer queries for this key's IDs with `quic_port`.
pub fn spawn_responder(key: [u8; 32], quic_port: u16) {
    let sock = match shared_socket().and_then(UdpSocket::from_std) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "LAN discovery unavailable (UDP {PORT}: {e}); connect by address instead"
            );
            return;
        }
    };
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                continue;
            };
            if n != 8 || &buf[..4] != QUERY {
                continue;
            }
            let id = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            let Some(id) = DeviceId::from_u32(id) else {
                continue;
            };
            if !key_matches(&key, id) {
                continue;
            }
            let mut a = [0u8; 10];
            a[..4].copy_from_slice(ANSWER);
            a[4..8].copy_from_slice(&id.as_u32().to_le_bytes());
            a[8..].copy_from_slice(&quic_port.to_le_bytes());
            let _ = sock.send_to(&a, from).await;
        }
    });
}

/// Viewer side: where on the LAN is `id`? `None` after `wait`.
pub async fn find(id: DeviceId, wait: Duration) -> Option<SocketAddr> {
    let sock = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
        .await
        .ok()?;
    sock.set_broadcast(true).ok()?;
    let q = query(id);
    let targets = [
        SocketAddr::from((Ipv4Addr::BROADCAST, PORT)),
        // Same machine (and hosts that do not get broadcasts back).
        SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)),
    ];
    let deadline = tokio::time::Instant::now() + wait;
    let mut buf = [0u8; 64];
    let mut next_send = tokio::time::Instant::now();
    loop {
        if tokio::time::Instant::now() >= next_send {
            for t in targets {
                let _ = sock.send_to(&q, t).await;
            }
            next_send += Duration::from_millis(300);
        }
        let until = next_send.min(deadline);
        match tokio::time::timeout_at(until, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if let Some(port) = parse_answer(&buf[..n], id) {
                    return Some(SocketAddr::new(from.ip(), port));
                }
            }
            Ok(Err(_)) => return None,
            Err(_) if tokio::time::Instant::now() >= deadline => return None,
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_and_wire_format() {
        let key = [7u8; 32];
        let id = DeviceId::derive(&key, 0);
        assert!(key_matches(&key, id));
        assert!(
            !key_matches(&[8u8; 32], id),
            "another key cannot claim the ID"
        );
        let mut a = [0u8; 10];
        a[..4].copy_from_slice(ANSWER);
        a[4..8].copy_from_slice(&id.as_u32().to_le_bytes());
        a[8..].copy_from_slice(&21118u16.to_le_bytes());
        assert_eq!(parse_answer(&a, id), Some(21118));
        let other = DeviceId::derive(&[9u8; 32], 0);
        assert_eq!(
            parse_answer(&a, other),
            None,
            "answer for a different ID ignored"
        );
        assert_eq!(query(id).len(), 8);
    }

    #[tokio::test]
    async fn host_is_found_by_id() {
        let key = [42u8; 32];
        let id = DeviceId::derive(&key, 0);
        spawn_responder(key, 4242);
        let found = find(id, Duration::from_secs(2))
            .await
            .expect("found on loopback/LAN");
        assert_eq!(found.port(), 4242);
        let nobody = DeviceId::derive(&[43u8; 32], 0);
        assert!(find(nobody, Duration::from_millis(400)).await.is_none());
    }
}

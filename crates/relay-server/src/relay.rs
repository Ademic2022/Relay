//! Blind UDP relay (§8.3).
//!
//! Two peers send `BIND <token>` from the same socket they run QUIC on.
//! Once both have bound, every other datagram from one is forwarded to the
//! other unchanged. The relay only ever sees QUIC ciphertext.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use relay_net::ctl::{self, Kind, Token};
use tokio::net::UdpSocket;

use crate::Tokens;

const IDLE: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct Session {
    a: Option<SocketAddr>,
    b: Option<SocketAddr>,
    last: Instant,
    /// Token bucket in bytes.
    budget: f64,
    refilled: Instant,
}

impl Session {
    fn other(&self, from: SocketAddr) -> Option<SocketAddr> {
        match (self.a, self.b) {
            (Some(a), Some(b)) if a == from => Some(b),
            (Some(a), Some(b)) if b == from => Some(a),
            _ => None,
        }
    }
}

/// Copies of forwarded datagrams, for tests that check what the relay
/// can see.
pub type Tap = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;

pub async fn start(bind: SocketAddr, tokens: Tokens, kbps: u32) -> anyhow::Result<SocketAddr> {
    start_tapped(bind, tokens, kbps, None).await
}

pub async fn start_tapped(
    bind: SocketAddr,
    tokens: Tokens,
    kbps: u32,
    tap: Option<Tap>,
) -> anyhow::Result<SocketAddr> {
    let sock = Arc::new(UdpSocket::bind(bind).await?);
    let addr = sock.local_addr()?;
    tokio::spawn(run(sock, tokens, kbps, tap));
    Ok(addr)
}

async fn run(sock: Arc<UdpSocket>, tokens: Tokens, kbps: u32, tap: Option<Tap>) {
    let rate = f64::from(kbps) * 1000.0 / 8.0; // bytes/s
    let burst = rate.max(256.0 * 1024.0); // ≥ 256 KB: one keyframe
    let mut sessions: HashMap<Token, Session> = HashMap::new();
    let mut by_addr: HashMap<SocketAddr, Token> = HashMap::new();
    let mut buf = vec![0u8; 65_536];
    let mut last_gc = Instant::now();
    loop {
        let (n, from) = match sock.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("relay recv: {e}");
                continue;
            }
        };
        let now = Instant::now();
        let data = &buf[..n];

        if let Some((Kind::Bind, token)) = ctl::decode(data) {
            let s = match sessions.get_mut(&token) {
                Some(s) => s,
                None if tokens.valid(&token) => sessions.entry(token).or_insert(Session {
                    a: None,
                    b: None,
                    last: now,
                    budget: burst,
                    refilled: now,
                }),
                None => continue, // unknown or expired token
            };
            if s.a != Some(from) && s.b != Some(from) {
                if s.a.is_none() {
                    s.a = Some(from);
                } else if s.b.is_none() {
                    s.b = Some(from);
                } else {
                    continue; // both slots taken
                }
                by_addr.insert(from, token);
            }
            s.last = now;
            if let (Some(a), Some(b)) = (s.a, s.b) {
                let ack = ctl::encode(Kind::Bound, &token);
                let _ = sock.send_to(&ack, a).await;
                let _ = sock.send_to(&ack, b).await;
            }
            continue;
        }

        if ctl::is_control(data) {
            continue; // stray punch packets etc.
        }
        let Some(token) = by_addr.get(&from) else {
            continue;
        };
        let Some(s) = sessions.get_mut(token) else {
            continue;
        };
        let Some(to) = s.other(from) else { continue };
        // Per-session bandwidth cap.
        s.budget = (s.budget + now.duration_since(s.refilled).as_secs_f64() * rate).min(burst);
        s.refilled = now;
        if s.budget < n as f64 {
            continue;
        }
        s.budget -= n as f64;
        s.last = now;
        if let Some(t) = &tap {
            let _ = t.send(data.to_vec());
        }
        let _ = sock.send_to(data, to).await;

        if now.duration_since(last_gc) > Duration::from_secs(10) {
            last_gc = now;
            sessions.retain(|_, s| now.duration_since(s.last) < IDLE);
            by_addr.retain(|_, t| sessions.contains_key(t));
        }
    }
}

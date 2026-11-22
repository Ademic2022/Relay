//! Non-QUIC control packets sharing the peer's UDP socket (§8.2 step 3).
//!
//! QUIC v1 requires the "fixed bit" (0x40) in every packet's first byte,
//! so a datagram starting with 0x00 can never be QUIC. Control packets use
//! that space: `00 'R' 'L' 'Y' <type> <16-byte session token>`.

pub const MAGIC: [u8; 4] = [0x00, b'R', b'L', b'Y'];
pub const TOKEN_LEN: usize = 16;
pub const LEN: usize = MAGIC.len() + 1 + TOKEN_LEN;

pub type Token = [u8; TOKEN_LEN];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Peer → relay: pair me with the other holder of this token.
    Bind = 1,
    /// Relay → peer: both sides bound; forwarding is live.
    Bound = 2,
    /// Peer → peer: opens NAT mappings for hole punching. Ignored on
    /// receipt.
    Punch = 3,
    /// Peer → peer, during a relayed session: "can you hear me here?"
    /// Authenticated, see [`encode_probe`].
    Probe = 4,
    /// Answer to a [`Kind::Probe`], sent back to the probe's source.
    ProbeAck = 5,
}

pub fn encode(kind: Kind, token: &Token) -> [u8; LEN] {
    let mut out = [0u8; LEN];
    out[..4].copy_from_slice(&MAGIC);
    out[4] = kind as u8;
    out[5..].copy_from_slice(token);
    out
}

/// Probe packet length: header, token, 8-byte nonce, 16-byte MAC.
pub const PROBE_LEN: usize = LEN + 8 + 16;

/// Key for authenticating path probes, derived from the session's TLS
/// exporter: only the two ends of that QUIC connection know it, so nobody
/// else can make a peer move its traffic to another address.
pub fn probe_key(tls_exporter: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("relay v1 path probe", tls_exporter)
}

fn probe_mac(key: &[u8; 32], head: &[u8]) -> [u8; 16] {
    let mut m = [0u8; 16];
    m.copy_from_slice(&blake3::keyed_hash(key, head).as_bytes()[..16]);
    m
}

pub fn encode_probe(kind: Kind, token: &Token, nonce: u64, key: &[u8; 32]) -> [u8; PROBE_LEN] {
    let mut out = [0u8; PROBE_LEN];
    out[..LEN].copy_from_slice(&encode(kind, token));
    out[LEN..LEN + 8].copy_from_slice(&nonce.to_le_bytes());
    let mac = probe_mac(key, &out[..LEN + 8]);
    out[LEN + 8..].copy_from_slice(&mac);
    out
}

/// Parse and authenticate a probe or probe-ack.
pub fn decode_probe(b: &[u8], key: &[u8; 32]) -> Option<(Kind, Token, u64)> {
    if b.len() != PROBE_LEN || b[..4] != MAGIC {
        return None;
    }
    let kind = match b[4] {
        4 => Kind::Probe,
        5 => Kind::ProbeAck,
        _ => return None,
    };
    let mac = probe_mac(key, &b[..LEN + 8]);
    // Constant-time compare.
    if mac
        .iter()
        .zip(&b[LEN + 8..])
        .fold(0u8, |a, (x, y)| a | (x ^ y))
        != 0
    {
        return None;
    }
    let token: Token = b[5..LEN].try_into().ok()?;
    let nonce = u64::from_le_bytes(b[LEN..LEN + 8].try_into().ok()?);
    Some((kind, token, nonce))
}

/// The token of a probe without authenticating it (to find its session).
pub fn probe_token(b: &[u8]) -> Option<Token> {
    (b.len() == PROBE_LEN && b[..4] == MAGIC && matches!(b[4], 4 | 5))
        .then(|| b[5..LEN].try_into().ok())?
}

pub fn decode(b: &[u8]) -> Option<(Kind, Token)> {
    if b.len() != LEN || b[..4] != MAGIC {
        return None;
    }
    let kind = match b[4] {
        1 => Kind::Bind,
        2 => Kind::Bound,
        3 => Kind::Punch,
        _ => return None,
    };
    Some((kind, b[5..].try_into().ok()?))
}

/// True for datagrams that are not QUIC (fixed bit clear).
pub fn is_control(b: &[u8]) -> bool {
    b.first() == Some(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let t = [7u8; 16];
        for k in [Kind::Bind, Kind::Bound, Kind::Punch] {
            assert_eq!(decode(&encode(k, &t)), Some((k, t)));
        }
        assert!(is_control(&encode(Kind::Punch, &t)));
        assert!(!is_control(&[0x40, 1, 2]));
        assert!(decode(&[0, b'R', b'L', b'Y', 9]).is_none());
    }

    #[test]
    fn probes_are_authenticated() {
        let (t, k) = ([3u8; 16], probe_key(&[1; 32]));
        let p = encode_probe(Kind::Probe, &t, 42, &k);
        assert!(is_control(&p));
        assert_eq!(probe_token(&p), Some(t));
        assert_eq!(decode_probe(&p, &k), Some((Kind::Probe, t, 42)));
        assert_eq!(decode_probe(&p, &probe_key(&[2; 32])), None, "wrong key");
        let mut forged = p;
        forged[LEN] ^= 1;
        assert_eq!(decode_probe(&forged, &k), None, "tampered nonce");
        assert_eq!(decode(&p), None, "not a plain control packet");
    }
}

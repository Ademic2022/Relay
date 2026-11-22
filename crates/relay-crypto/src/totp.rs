//! TOTP second factor for unattended access (FR-36).
//!
//! RFC 6238 with the parameters every authenticator app supports:
//! HMAC-SHA1, 6 digits, 30-second steps. The Host accepts the current step
//! and one step either side (clock drift), and never the same step twice,
//! so a code seen over someone's shoulder cannot be replayed.
//!
//! The code is checked only after the permanent password has passed
//! OPAQUE, inside the authenticated QUIC connection: it adds a factor and
//! never replaces the password.

use hmac::{Hmac, Mac};
use sha1::Sha1;

pub const DIGITS: usize = 6;
pub const STEP_SECS: u64 = 30;
/// Steps either side of now that are accepted.
const SKEW: u64 = 1;

/// 160-bit shared secret (RFC 4226 recommends at least 128 bits).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret([u8; 20]);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl Secret {
    pub fn generate() -> Result<Self, crate::CryptoError> {
        Ok(Self(crate::random_bytes()?))
    }

    pub fn from_bytes(b: [u8; 20]) -> Self {
        Self(b)
    }

    /// RFC 4648 base32, no padding: the form authenticator apps take.
    pub fn to_base32(&self) -> String {
        base32_encode(&self.0)
    }

    pub fn from_base32(s: &str) -> Option<Self> {
        base32_decode(s)?.try_into().ok().map(Self)
    }

    /// `otpauth://` URI for a QR code. `account` names this computer.
    pub fn uri(&self, account: &str) -> String {
        let label: String = account
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        format!(
            "otpauth://totp/Relay:{label}?secret={}&issuer=Relay&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
            self.to_base32()
        )
    }

    /// The code for time step `step` (unix seconds / 30).
    pub fn code(&self, step: u64) -> u32 {
        let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(&self.0).expect("any key length");
        mac.update(&step.to_be_bytes());
        let h = mac.finalize().into_bytes();
        let off = (h[19] & 0x0f) as usize;
        let bin = u32::from_be_bytes([h[off] & 0x7f, h[off + 1], h[off + 2], h[off + 3]]);
        bin % 10u32.pow(DIGITS as u32)
    }

    /// Check `code` at `unix_secs`. Returns the matched step on success;
    /// steps at or before `last_used` are refused (no replay).
    pub fn verify(&self, code: &str, unix_secs: u64, last_used: Option<u64>) -> Option<u64> {
        let code = code.trim();
        if code.len() != DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let want: u32 = code.parse().ok()?;
        let now = unix_secs / STEP_SECS;
        // Check every candidate step without stopping early.
        let mut hit = None;
        for step in now.saturating_sub(SKEW)..=now + SKEW {
            if self.code(step) == want && last_used.is_none_or(|l| step > l) {
                hit = Some(step);
            }
        }
        hit
    }
}

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0);
    for c in s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=' && *c != '-')
    {
        let v = ALPHABET
            .iter()
            .position(|&a| a == c.to_ascii_uppercase() as u8)? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rfc() -> Secret {
        Secret(*b"12345678901234567890")
    }

    #[test]
    fn rfc6238_vectors() {
        // RFC 6238 appendix B (SHA1), last 6 of the 8 digits.
        for (t, code) in [
            (59u64, 287_082u32),
            (1_111_111_109, 81_804),
            (1_234_567_890, 5_924),
            (2_000_000_000, 279_037),
        ] {
            assert_eq!(rfc().code(t / STEP_SECS), code, "t={t}");
        }
    }

    #[test]
    fn verify_window_and_replay() {
        let s = rfc();
        let t = 1_234_567_890;
        assert_eq!(s.verify("005924", t, None), Some(t / 30));
        assert!(s.verify(" 005924 ", t, None).is_some(), "spaces trimmed");
        assert!(
            s.verify("005924", t + 30, None).is_some(),
            "one step late ok"
        );
        assert!(s.verify("005924", t + 90, None).is_none(), "too late");
        assert!(s.verify("005924", t, Some(t / 30)).is_none(), "no replay");
        assert!(s.verify("123456", t, None).is_none());
        assert!(s.verify("5924", t, None).is_none(), "wrong length");
        assert!(s.verify("00592a", t, None).is_none());
    }

    #[test]
    fn base32_roundtrip() {
        assert_eq!(rfc().to_base32(), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        let s = Secret::generate().unwrap();
        assert_eq!(Secret::from_base32(&s.to_base32()), Some(s.clone()));
        assert_eq!(
            Secret::from_base32(&s.to_base32().to_lowercase()),
            Some(s.clone())
        );
        assert!(Secret::from_base32("GEZD").is_none(), "wrong length");
        assert!(Secret::from_base32("!!!!").is_none());
        assert!(
            s.uri("my mac")
                .starts_with("otpauth://totp/Relay:mymac?secret=")
        );
    }
}

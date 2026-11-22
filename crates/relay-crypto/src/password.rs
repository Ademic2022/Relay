//! One-time passwords (FR-2) and permanent-password rules (FR-31).

use std::fmt;

use crate::{CryptoError, random_bytes};

/// Alphabet for one-time passwords: alphanumeric without look-alikes
/// (0/O, 1/l/I), because users read them aloud over the phone.
const OTP_ALPHABET: &[u8] = b"23456789abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";
pub const OTP_LEN: usize = 8;
pub const PERMANENT_MIN_LEN: usize = 12;

/// An 8-character one-time password. Rotates after each session or on click.
#[derive(Clone, PartialEq, Eq)]
pub struct OneTimePassword(String);

impl OneTimePassword {
    pub fn generate() -> Result<Self, CryptoError> {
        let mut out = String::with_capacity(OTP_LEN);
        // Rejection sampling: no modulo bias.
        let limit = 256 - (256 % OTP_ALPHABET.len());
        while out.len() < OTP_LEN {
            for b in random_bytes::<16>()? {
                if (b as usize) < limit && out.len() < OTP_LEN {
                    out.push(OTP_ALPHABET[b as usize % OTP_ALPHABET.len()] as char);
                }
            }
        }
        Ok(Self(out))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OneTimePassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OneTimePassword(..)")
    }
}

/// Strength rating shown by the setup wizard's meter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    TooShort,
    Weak,
    Fair,
    Strong,
}

/// Rate a permanent password. Anything below [`Strength::Fair`] is rejected.
///
/// Rough entropy estimate (length x log2(charset)); good enough for a meter.
/// A dictionary-aware estimator can replace it without API change.
pub fn permanent_strength(pw: &str) -> Strength {
    let len = pw.chars().count();
    if len < PERMANENT_MIN_LEN {
        return Strength::TooShort;
    }
    let mut pool = 0u32;
    if pw.chars().any(|c| c.is_ascii_lowercase()) {
        pool += 26;
    }
    if pw.chars().any(|c| c.is_ascii_uppercase()) {
        pool += 26;
    }
    if pw.chars().any(|c| c.is_ascii_digit()) {
        pool += 10;
    }
    if pw.chars().any(|c| !c.is_ascii_alphanumeric()) {
        pool += 33;
    }
    let distinct = {
        let mut v: Vec<char> = pw.chars().collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    };
    // Repetitive passwords ("aaaaaaaaaaaa") count by distinct chars.
    let effective_len = len.min(distinct * 3) as f64;
    let bits = effective_len * f64::from(pool.max(1)).log2();
    match bits {
        b if b < 50.0 => Strength::Weak,
        b if b < 70.0 => Strength::Fair,
        _ => Strength::Strong,
    }
}

pub fn permanent_acceptable(pw: &str) -> bool {
    permanent_strength(pw) >= Strength::Fair
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otp_shape() {
        for _ in 0..200 {
            let p = OneTimePassword::generate().unwrap();
            assert_eq!(p.as_str().len(), OTP_LEN);
            assert!(p.as_str().bytes().all(|b| OTP_ALPHABET.contains(&b)));
        }
    }

    #[test]
    fn otp_rotates() {
        let a = OneTimePassword::generate().unwrap();
        let b = OneTimePassword::generate().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn otp_debug_hides_value() {
        let p = OneTimePassword::generate().unwrap();
        assert!(!format!("{p:?}").contains(p.as_str()));
    }

    #[test]
    fn strength() {
        assert_eq!(permanent_strength("short"), Strength::TooShort);
        assert_eq!(permanent_strength("aaaaaaaaaaaaaaaa"), Strength::Weak);
        assert!(!permanent_acceptable("aaaaaaaaaaaaaaaa"));
        assert!(permanent_acceptable("correct-horse-battery"));
        assert_eq!(permanent_strength("Tr0ub4dor&3-Xy!q9"), Strength::Strong);
    }
}

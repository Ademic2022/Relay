//! Signed update manifests (FR-42).
//!
//! ```text
//! relay-update v1
//! version 0.2.0
//! artifact macos-aarch64 https://example.com/relay-0.2.0-macos-aarch64 9473024 <blake3 hex>
//! artifact windows-x86_64 https://example.com/relay-0.2.0.exe 9121792 <blake3 hex>
//! ```
//!
//! The manifest is signed with an Ed25519 release key (`manifest.sig`:
//! 128 hex chars). Clients pin the public key at build time, check the
//! signature before reading anything, refuse versions that are not newer
//! (no downgrades), and check each download's size and BLAKE3 hash.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub platform: String,
    pub url: String,
    pub size: u64,
    pub blake3: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: Version,
    pub artifacts: Vec<Artifact>,
}

/// `major.minor.patch`, compared numerically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let mut it = s
            .trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.parse::<u32>().ok());
        let v = Version(it.next()??, it.next()??, it.next()??);
        it.next().is_none().then_some(v)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    #[error("update signature is invalid")]
    BadSignature,
    #[error("malformed manifest: {0}")]
    Malformed(String),
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    let b = hex(s)?;
    b.try_into().ok()
}

fn hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    (s.len().is_multiple_of(2) && s.is_ascii()).then(|| {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
            .collect()
    })?
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Manifest {
    pub fn render(&self) -> String {
        let mut s = format!("relay-update v1\nversion {}\n", self.version);
        for a in &self.artifacts {
            s.push_str(&format!(
                "artifact {} {} {} {}\n",
                a.platform,
                a.url,
                a.size,
                to_hex(&a.blake3)
            ));
        }
        s
    }

    /// Parse a manifest whose signature was already checked.
    fn parse(text: &str) -> Result<Self, UpdateError> {
        let bad = |m: &str| UpdateError::Malformed(m.into());
        let mut lines = text.lines();
        if lines.next() != Some("relay-update v1") {
            return Err(bad("header"));
        }
        let version = lines
            .next()
            .and_then(|l| l.strip_prefix("version "))
            .and_then(Version::parse)
            .ok_or_else(|| bad("version"))?;
        let mut artifacts = Vec::new();
        for l in lines.filter(|l| !l.is_empty()) {
            let f: Vec<&str> = l.split(' ').collect();
            match f.as_slice() {
                ["artifact", platform, url, size, h] => artifacts.push(Artifact {
                    platform: (*platform).into(),
                    url: (*url).into(),
                    size: size.parse().map_err(|_| bad("size"))?,
                    blake3: hex32(h).ok_or_else(|| bad("hash"))?,
                }),
                _ => return Err(bad("line")),
            }
        }
        Ok(Self { version, artifacts })
    }

    /// Check `sig_hex` over `text` with `public_key`, then parse.
    pub fn verify(text: &str, sig_hex: &str, public_key: &[u8; 32]) -> Result<Self, UpdateError> {
        let key = VerifyingKey::from_bytes(public_key).map_err(|_| UpdateError::BadSignature)?;
        let sig: [u8; 64] = hex(sig_hex)
            .and_then(|b| b.try_into().ok())
            .ok_or(UpdateError::BadSignature)?;
        key.verify(text.as_bytes(), &Signature::from_bytes(&sig))
            .map_err(|_| UpdateError::BadSignature)?;
        Self::parse(text)
    }

    pub fn artifact(&self, platform: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.platform == platform)
    }
}

/// Sign a rendered manifest (release tooling).
pub fn sign(text: &str, secret: &[u8; 32]) -> String {
    to_hex(
        &SigningKey::from_bytes(secret)
            .sign(text.as_bytes())
            .to_bytes(),
    )
}

/// This build's platform key, e.g. `macos-aarch64`.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            version: Version(0, 2, 0),
            artifacts: vec![Artifact {
                platform: "macos-aarch64".into(),
                url: "https://example.com/relay".into(),
                size: 42,
                blake3: [7; 32],
            }],
        }
    }

    #[test]
    fn sign_verify_tamper() {
        let secret = [3u8; 32];
        let public = SigningKey::from_bytes(&secret).verifying_key().to_bytes();
        let text = sample().render();
        let sig = sign(&text, &secret);
        assert_eq!(Manifest::verify(&text, &sig, &public).unwrap(), sample());
        let tampered = text.replace("https://example.com/relay", "https://evil.example/relay");
        assert_eq!(
            Manifest::verify(&tampered, &sig, &public),
            Err(UpdateError::BadSignature)
        );
        assert_eq!(
            Manifest::verify(&text, &sig, &[9; 32]),
            Err(UpdateError::BadSignature)
        );
        assert_eq!(
            Manifest::verify(&text, "zz", &public),
            Err(UpdateError::BadSignature)
        );
    }

    #[test]
    fn versions() {
        assert!(Version::parse("0.10.0").unwrap() > Version::parse("0.9.9").unwrap());
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
    }
}

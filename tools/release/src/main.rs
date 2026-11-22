//! `relay-release` — update signing (FR-42).
//!
//! ```text
//! relay-release keygen                       # prints secret + public key
//! relay-release manifest VERSION BASE_URL platform=FILE ...  > manifest
//! relay-release sign SECRET_KEY_FILE manifest > manifest.sig
//! ```
//!
//! Build clients with `RELAY_UPDATE_KEY=<public hex>` to pin the key.
//! Keep the secret offline or in the CI secret store only.

use std::process::ExitCode;

use relay_crypto::update::{Artifact, Manifest, Version, sign};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("keygen") => {
            let mut secret = [0u8; 32];
            getrandom::fill(&mut secret).map_err(|e| e.to_string())?;
            let public = ed25519_dalek::SigningKey::from_bytes(&secret).verifying_key().to_bytes();
            println!("secret {}", hex(&secret));
            println!("public {}", hex(&public));
            eprintln!("Store the secret safely; build clients with RELAY_UPDATE_KEY={}", hex(&public));
            Ok(())
        }
        Some("manifest") => {
            let version = args.get(1).and_then(|v| Version::parse(v)).ok_or("VERSION must be x.y.z")?;
            let base = args.get(2).ok_or("BASE_URL missing")?.trim_end_matches('/');
            if !base.starts_with("https://") {
                return Err("BASE_URL must be https://".into());
            }
            let mut artifacts = Vec::new();
            for spec in &args[3..] {
                let (platform, file) = spec.split_once('=').ok_or("artifacts are platform=FILE")?;
                let data = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
                let name = std::path::Path::new(file).file_name().ok_or("bad file name")?.to_string_lossy();
                artifacts.push(Artifact {
                    platform: platform.into(),
                    url: format!("{base}/{name}"),
                    size: data.len() as u64,
                    blake3: *blake3::hash(&data).as_bytes(),
                });
            }
            if artifacts.is_empty() {
                return Err("no artifacts".into());
            }
            print!("{}", Manifest { version, artifacts }.render());
            Ok(())
        }
        Some("sign") => {
            let key_file = args.get(1).ok_or("SECRET_KEY_FILE missing")?;
            let manifest = args.get(2).ok_or("manifest missing")?;
            let k = std::fs::read_to_string(key_file).map_err(|e| e.to_string())?;
            let k = k.trim().trim_start_matches("secret ").trim();
            let secret: [u8; 32] = (0..32)
                .map(|i| k.get(i * 2..i * 2 + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
                .collect::<Option<Vec<u8>>>()
                .and_then(|v| v.try_into().ok())
                .ok_or("secret key must be 64 hex characters")?;
            let text = std::fs::read_to_string(manifest).map_err(|e| e.to_string())?;
            println!("{}", sign(&text, &secret));
            Ok(())
        }
        _ => Err("usage: relay-release keygen | manifest VERSION BASE_URL platform=FILE... | sign KEYFILE MANIFEST".into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}

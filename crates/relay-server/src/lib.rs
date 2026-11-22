//! Rendezvous + relay server (§8.3).
//!
//! - **Rendezvous** (QUIC, default :21116): assigns Device IDs, keeps a
//!   live registration per online Host, and introduces Controllers to
//!   Hosts with each other's observed and local addresses plus a session
//!   token. It never carries session data.
//! - **Relay** (UDP, default :21117): pairs the two peers that present
//!   the same token and forwards their datagrams blindly. Sessions are
//!   end-to-end encrypted QUIC, so the relay cannot read them.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod relay;
pub mod rendezvous;
pub mod store;

pub use relay_net::ctl::Token;

#[derive(Debug, Clone)]
pub struct Config {
    pub rendezvous: Option<SocketAddr>,
    pub relay: Option<SocketAddr>,
    /// Relay address handed to peers. Unspecified IP = "same host as the
    /// rendezvous server".
    pub public_relay: Option<SocketAddr>,
    pub data_dir: PathBuf,
    /// Per-session relay bandwidth cap.
    pub relay_kbps: u32,
}

/// Tokens issued by the rendezvous server, redeemable at the relay.
#[derive(Debug, Default, Clone)]
pub struct Tokens(Arc<Mutex<HashMap<Token, Instant>>>);

/// How long an issued token can be bound at the relay.
pub const TOKEN_TTL: Duration = Duration::from_secs(60);

impl Tokens {
    pub fn issue(&self) -> Token {
        let mut t = [0u8; 16];
        getrandom::fill(&mut t).expect("OS RNG");
        let mut m = self.0.lock().expect("poisoned");
        let now = Instant::now();
        m.retain(|_, exp| *exp > now);
        m.insert(t, now + TOKEN_TTL);
        t
    }

    pub fn valid(&self, t: &Token) -> bool {
        self.0
            .lock()
            .expect("poisoned")
            .get(t)
            .is_some_and(|exp| *exp > Instant::now())
    }
}

/// Running server; dropping it does not stop the tasks.
#[derive(Debug)]
pub struct Running {
    pub rendezvous_addr: Option<SocketAddr>,
    pub relay_addr: Option<SocketAddr>,
    pub public_key: [u8; 32],
}

pub async fn start(cfg: Config) -> anyhow::Result<Running> {
    std::fs::create_dir_all(&cfg.data_dir)?;
    let identity = store::load_or_create_identity(&cfg.data_dir)?;
    let tokens = Tokens::default();
    let mut relay_addr = None;
    if let Some(bind) = cfg.relay {
        relay_addr = Some(relay::start(bind, tokens.clone(), cfg.relay_kbps).await?);
    }
    let mut rendezvous_addr = None;
    if let Some(bind) = cfg.rendezvous {
        let public_relay = cfg
            .public_relay
            .or(relay_addr
                .map(|a| SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), a.port())))
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], relay_net::RELAY_PORT)));
        rendezvous_addr = Some(
            rendezvous::start(
                bind,
                &identity,
                store::Devices::open(&cfg.data_dir)?,
                tokens,
                public_relay,
            )
            .await?,
        );
    }
    Ok(Running {
        rendezvous_addr,
        relay_addr,
        public_key: identity.public_key,
    })
}

pub use relay_net::{hex, parse_hex32};

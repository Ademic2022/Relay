//! `relay-server [--rendezvous] [--relay]` (§8.3).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use relay_server::{Config, hex};

const USAGE: &str = "usage: relay-server [--rendezvous] [--relay] [--bind IP] [--data-dir DIR]
                    [--public-relay IP:PORT] [--relay-kbps N]

  --rendezvous        run the rendezvous service on UDP :21116
  --relay             run the relay on UDP :21117
                      (neither flag: run both)
  --bind IP           listen address (default 0.0.0.0)
  --data-dir DIR      server key and device registry (default ./relay-data)
  --public-relay A    relay address to give peers (default: this host, :21117)
  --relay-kbps N      per-session relay bandwidth cap (default 50000)";

fn parse(args: impl IntoIterator<Item = String>) -> Result<Config, String> {
    let mut rdv = false;
    let mut relay = false;
    let mut bind: std::net::IpAddr = [0, 0, 0, 0].into();
    let mut data_dir = PathBuf::from("relay-data");
    let mut public_relay = None;
    let mut relay_kbps = 50_000;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--rendezvous" => rdv = true,
            "--relay" => relay = true,
            "--bind" => bind = val()?.parse().map_err(|e| format!("--bind: {e}"))?,
            "--data-dir" => data_dir = val()?.into(),
            "--public-relay" => {
                public_relay = Some(
                    val()?
                        .parse::<SocketAddr>()
                        .map_err(|e| format!("--public-relay: {e}"))?,
                )
            }
            "--relay-kbps" => {
                relay_kbps = val()?.parse().map_err(|e| format!("--relay-kbps: {e}"))?
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if !rdv && !relay {
        rdv = true;
        relay = true;
    }
    Ok(Config {
        rendezvous: rdv.then_some(SocketAddr::new(bind, relay_net::RENDEZVOUS_PORT)),
        relay: relay.then_some(SocketAddr::new(bind, relay_net::RELAY_PORT)),
        public_relay,
        data_dir,
        relay_kbps,
    })
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .with_target(false)
        .init();
    let cfg = match parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}\n");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async {
        match relay_server::start(cfg).await {
            Ok(r) => {
                println!("relay-server running");
                if let Some(a) = r.rendezvous_addr {
                    println!("  rendezvous  udp {a}");
                }
                if let Some(a) = r.relay_addr {
                    println!("  relay       udp {a}");
                }
                println!("  server key  {}", hex(&r.public_key));
                println!("Clients pin this key: --server-key {}", hex(&r.public_key));
                let _ = tokio::signal::ctrl_c().await;
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("relay-server: {e:#}");
                ExitCode::FAILURE
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_both() {
        let c = parse(Vec::<String>::new()).unwrap();
        assert!(c.rendezvous.is_some() && c.relay.is_some());
        let c = parse(["--relay".to_string()]).unwrap();
        assert!(c.rendezvous.is_none() && c.relay.is_some());
        assert!(parse(["--bogus".to_string()]).is_err());
        assert!(parse(["--bind".to_string()]).is_err());
    }
}

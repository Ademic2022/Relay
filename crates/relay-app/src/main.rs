//! `relay` — command-line host and viewer.
//!
//! ```text
//! relay host    [--server HOST --server-key KEY] [--display ID] [--view-only]
//! relay connect <ID> --server HOST --server-key KEY
//! relay view    <ip[:port]>                      (direct, no server: FR-7)
//! relay forget  <ID | ip:port>                   (drop a pinned host key)
//! ```
//!
//! `--server` / `--server-key` can also come from `RELAY_SERVER` and
//! `RELAY_SERVER_KEY`.

use std::net::SocketAddr;

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};

mod auth;
mod clock;
mod consent;
mod feedback;
mod host;
mod host_input;
mod identity;
mod input_wire;
mod migrate;
#[cfg(test)]
mod netem_tests;
mod pins;
mod rdv;
mod rt;
mod sender;
mod viewer;

/// Default port for direct (no-rendezvous) connections.
pub const DIRECT_PORT: u16 = 21118;

#[derive(Debug, Parser)]
#[command(name = "relay", version, about = "Low-latency remote desktop")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CodecChoice {
    /// Hardware if available, else software.
    Auto,
    /// Hardware only (VideoToolbox / Media Foundation).
    Hw,
    /// Software openh264.
    Sw,
}

/// Rendezvous server settings (FR-41).
#[derive(Debug, Clone, Args)]
struct ServerArgs {
    /// Rendezvous server, host[:port] (default port 21116).
    #[arg(long, env = "RELAY_SERVER")]
    server: Option<String>,
    /// The server's public key, as printed by relay-server.
    #[arg(long, env = "RELAY_SERVER_KEY")]
    server_key: Option<String>,
}

impl ServerArgs {
    fn resolve(&self) -> anyhow::Result<Option<rdv::Server>> {
        match (&self.server, &self.server_key) {
            (None, _) => Ok(None),
            (Some(_), None) => {
                anyhow::bail!("--server needs --server-key (printed by relay-server)")
            }
            (Some(s), Some(k)) => Ok(Some(rdv::Server {
                addr: rdv::parse_server(s)?,
                key: relay_net::parse_hex32(k).context("--server-key must be 64 hex characters")?,
            })),
        }
    }
}

#[derive(Debug, Clone, Args)]
struct ViewerArgs {
    /// Password shown by the host (prompted for if omitted).
    #[arg(long, alias = "code")]
    password: Option<String>,
    /// Measure glass-to-glass latency from the host's --test-pattern
    /// timestamp stripe (clocks must be synced across machines).
    #[arg(long)]
    measure: bool,
    /// Do not swap ⌘ and Ctrl between macOS and Windows (FR-15).
    #[arg(long)]
    no_cmd_ctrl_swap: bool,
    /// Send typed characters instead of physical keys, for mismatched
    /// keyboard layouts (FR-14).
    #[arg(long)]
    translate_chars: bool,
    /// Starting quality mode; Ctrl+Alt+Q cycles it in the viewer.
    #[arg(long, value_enum, default_value_t = Quality::Speed)]
    quality: Quality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[allow(clippy::enum_variant_names)] // CLI spelling: --quality quality
enum Quality {
    /// Lowest latency (default).
    Speed,
    Balanced,
    /// Higher bitrate and resolution held longer (design work).
    Quality,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Share this machine's screen.
    Host {
        #[arg(long, default_value_t = SocketAddr::from(([0, 0, 0, 0], DIRECT_PORT)))]
        bind: SocketAddr,
        #[command(flatten)]
        server: ServerArgs,
        /// Display ID to share (default: primary). See --list-displays.
        #[arg(long)]
        display: Option<u32>,
        #[arg(long)]
        list_displays: bool,
        /// Stream a synthetic test pattern instead of the screen (no
        /// Screen Recording permission needed). Value: WIDTHxHEIGHT.
        #[arg(long, value_name = "WxH")]
        test_pattern: Option<String>,
        #[arg(long, value_enum, default_value_t = CodecChoice::Auto)]
        codec: CodecChoice,
        /// Starting bitrate in kbit/s (ABR adapts from here).
        #[arg(long, default_value_t = 8_000)]
        bitrate: u32,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// Let viewers watch but not control (FR-11).
        #[arg(long)]
        view_only: bool,
    },
    /// Connect to a host by its 9-digit ID.
    Connect {
        /// Device ID, e.g. "123 456 789".
        id: String,
        #[command(flatten)]
        server: ServerArgs,
        #[command(flatten)]
        viewer: ViewerArgs,
        /// Skip peer-to-peer and go through the relay (testing).
        #[arg(long)]
        force_relay: bool,
    },
    /// Connect directly to a host's IP address (no server; FR-7).
    View {
        /// Host address, e.g. 192.168.1.20 (port defaults to 21118).
        addr: String,
        #[command(flatten)]
        viewer: ViewerArgs,
    },
    /// Forget a host's pinned device key (after a reinstall).
    Forget {
        /// Device ID or ip:port, as shown in the warning.
        name: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,quinn=warn,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()
            }),
        )
        .with_target(false)
        .init();

    match Cli::parse().cmd {
        Cmd::Host {
            bind,
            server,
            display,
            list_displays,
            test_pattern,
            codec,
            bitrate,
            fps,
            view_only,
        } => host::run(host::Options {
            bind,
            server: server.resolve()?,
            view_only,
            display,
            list_displays,
            test_pattern: test_pattern.map(|s| parse_size(&s)).transpose()?,
            codec,
            bitrate_kbps: bitrate,
            fps,
        }),
        Cmd::Connect {
            id,
            server,
            viewer,
            force_relay,
        } => {
            let id = relay_crypto::DeviceId::parse(&id).context("a Device ID has 9 digits")?;
            let server = server
                .resolve()?
                .context("connecting by ID needs --server and --server-key (or RELAY_SERVER / RELAY_SERVER_KEY)")?;
            run_viewer(viewer::Target::Id { id, server }, viewer, force_relay)
        }
        Cmd::View { addr, viewer } => {
            run_viewer(viewer::Target::Direct(parse_addr(&addr)?), viewer, false)
        }
        Cmd::Forget { name } => {
            let name: String = name.chars().filter(|c| !c.is_whitespace()).collect();
            pins::forget(&name);
            println!("Forgot the pinned key for {name}.");
            Ok(())
        }
    }
}

fn run_viewer(target: viewer::Target, v: ViewerArgs, force_relay: bool) -> anyhow::Result<()> {
    let password = match v.password {
        Some(p) => p,
        None => rpassword::prompt_password(format!("Password for {target}: "))?,
    };
    viewer::run(viewer::ViewOptions {
        target,
        password: password.trim().to_string(),
        force_relay,
        measure: v.measure,
        cmd_ctrl_swap: !v.no_cmd_ctrl_swap,
        translate_chars: v.translate_chars,
        quality: match v.quality {
            Quality::Speed => relay_proto::v1::QualityMode::Speed,
            Quality::Balanced => relay_proto::v1::QualityMode::Balanced,
            Quality::Quality => relay_proto::v1::QualityMode::Quality,
        },
    })
}

fn parse_size(s: &str) -> anyhow::Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("expected WIDTHxHEIGHT, got {s}"))?;
    Ok((w.parse()?, h.parse()?))
}

fn parse_addr(s: &str) -> anyhow::Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    let with_port = if s.parse::<SocketAddr>().is_ok()
        || s.rsplit_once(':')
            .is_some_and(|(h, p)| !h.ends_with(':') && p.parse::<u16>().is_ok() && !h.contains(':'))
    {
        s.to_string()
    } else {
        format!("{s}:{DIRECT_PORT}")
    };
    with_port
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve {s}"))
}

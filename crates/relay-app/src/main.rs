//! `relay` — M1 command-line host and viewer.
//!
//! ```text
//! relay host [--bind 0.0.0.0:21118] [--display ID] [--codec auto|hw|sw]
//! relay view <host:port> --code <session code>
//! ```

use std::net::SocketAddr;

use clap::{Parser, Subcommand, ValueEnum};

mod clock;
mod host;
mod host_input;
mod input_wire;
mod rt;
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

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Share this machine's screen.
    Host {
        #[arg(long, default_value_t = SocketAddr::from(([0, 0, 0, 0], DIRECT_PORT)))]
        bind: SocketAddr,
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
    /// View a host's screen.
    View {
        /// Host address, e.g. 192.168.1.20:21118 (port defaults to 21118).
        addr: String,
        /// Session code shown by the host.
        #[arg(long)]
        code: String,
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
            display,
            list_displays,
            test_pattern,
            codec,
            bitrate,
            fps,
            view_only,
        } => host::run(host::Options {
            view_only,
            bind,
            display,
            list_displays,
            test_pattern: test_pattern.map(|s| parse_size(&s)).transpose()?,
            codec,
            bitrate_kbps: bitrate,
            fps,
        }),
        Cmd::View {
            addr,
            code,
            measure,
            no_cmd_ctrl_swap,
            translate_chars,
        } => {
            let addr = parse_addr(&addr)?;
            viewer::run(viewer::ViewOptions {
                addr,
                code,
                measure,
                cmd_ctrl_swap: !no_cmd_ctrl_swap,
                translate_chars,
            })
        }
    }
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

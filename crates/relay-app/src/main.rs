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
use tracing_subscriber::Layer;

mod auth;
mod banner;
mod chat;
mod clipboard;
mod clock;
mod consent;
mod feedback;
mod files;
mod host;
mod host_input;
mod identity;
mod input_wire;
mod lan;
mod logs;
mod migrate;
#[cfg(test)]
mod netem_tests;
mod pins;
mod rdv;
mod rt;
mod sender;
mod settings;
mod stdin;
mod ui;
mod unattended;
mod update;
mod viewer;
mod wol;

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
        // Flags / env first, then settings.conf (FR-41).
        let st = settings::Settings::load();
        let server = self.server.clone().or_else(|| st.get("server"));
        let key = self.server_key.clone().or_else(|| st.get("server_key"));
        match (&server, &key) {
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
    /// Read the password from the first line of stdin (used by the app,
    /// so it never appears in the process list).
    #[arg(long, conflicts_with = "password")]
    password_stdin: bool,
    /// Two-factor code from the host owner's authenticator app, if the
    /// host asks for one (FR-36). Prompted for when needed if omitted.
    #[arg(long, value_name = "CODE")]
    totp: Option<String>,
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
    /// Do not sync the clipboard with the host (FR-17).
    #[arg(long)]
    no_clipboard: bool,
    /// Starting quality mode (default from settings); Ctrl+Alt+Q cycles it.
    #[arg(long, value_enum)]
    quality: Option<Quality>,
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
enum UnattendedCmd {
    /// Set a permanent password and install the service (FR-31).
    Setup {
        /// Only allow these Controller Device IDs (repeatable).
        #[arg(long = "allow", value_name = "ID")]
        allow: Vec<String>,
        /// Store the password but do not install the service.
        #[arg(long)]
        no_service: bool,
        /// Print what would be done and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Also require a code from an authenticator app (FR-36).
        #[arg(long)]
        totp: bool,
    },
    /// Remove the service and invalidate the permanent password (FR-38).
    Disable,
    Status,
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    List,
    Get { key: String },
    Set { key: String, value: String },
    Unset { key: String },
}

#[derive(Debug, Subcommand)]
enum UpdateCmd {
    /// Show whether a newer, correctly signed version is available.
    Check,
    /// Download, verify and install it.
    Apply,
}

#[derive(Debug, Subcommand)]
enum ServiceCmd {
    Run,
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
        /// Do not sync the clipboard with the viewer (FR-17).
        #[arg(long)]
        no_clipboard: bool,
        /// Do not let the viewer browse or transfer files (FR-26).
        #[arg(long)]
        no_files: bool,
        /// Run as the unattended agent (started by the service; FR-32).
        #[arg(long, hide = true)]
        unattended: bool,
        /// Driven by the Relay app over stdin/stdout (see ui.rs).
        #[arg(long, hide = true)]
        ui: bool,
    },
    /// Unattended access to this computer (run as administrator).
    Unattended {
        #[command(subcommand)]
        cmd: UnattendedCmd,
    },
    /// The background service (started by the OS).
    #[command(hide = true)]
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// View or change settings (FR-41).
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Check for and install updates (FR-42).
    Update {
        #[command(subcommand)]
        cmd: UpdateCmd,
    },
    /// Show this computer's connection audit log (FR-39).
    Audit {
        /// The unattended agent's log instead of this user's.
        #[arg(long)]
        unattended: bool,
    },
    /// Show where the diagnostic logs are (FR-40).
    Logs {
        #[arg(long)]
        unattended: bool,
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
    /// Wake a sleeping computer through another Relay device on its
    /// network (FR-37). That device needs unattended access; you log in
    /// with its permanent password.
    Wake {
        /// The computer to wake: its Device ID or address, as you
        /// connected to it before (its MAC was remembered then).
        device: String,
        /// A Relay device on the same network that is awake.
        #[arg(long, value_name = "ID_OR_ADDRESS")]
        via: String,
        /// MAC address to wake instead of the remembered ones (repeatable).
        #[arg(long = "mac", value_name = "AA:BB:CC:DD:EE:FF")]
        macs: Vec<String>,
        /// Read the password from the first line of stdin.
        #[arg(long)]
        password_stdin: bool,
        /// Two-factor code for the --via device, if it asks (FR-36).
        #[arg(long, value_name = "CODE")]
        totp: Option<String>,
        #[command(flatten)]
        server: ServerArgs,
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

fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            "info,quinn=warn,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()
        })
    };
    let stderr = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(filter());
    // FR-40: also to a rotating file (10 MB cap) for "copy logs".
    let file = logs::Rotating::open(logs::data_dir(), logs::DIAG_CAP)
        .ok()
        .map(|r| {
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(false)
                .with_writer(logs::DiagLog(std::sync::Mutex::new(r)))
                .with_filter(filter())
        });
    tracing_subscriber::registry()
        .with(stderr)
        .with(file)
        .init();
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The agent and service keep their logs in the system folder.
    let system = matches!(
        cli.cmd,
        Cmd::Host {
            unattended: true,
            ..
        } | Cmd::Service { .. }
            | Cmd::Unattended { .. }
            | Cmd::Audit { unattended: true }
            | Cmd::Logs { unattended: true }
    );
    if system {
        logs::set_data_dir(unattended::system_dir());
    }
    init_logging();

    match cli.cmd {
        Cmd::Unattended { cmd } => match cmd {
            UnattendedCmd::Setup {
                allow,
                no_service,
                dry_run,
                totp,
            } => unattended::setup(unattended::SetupArgs {
                allow,
                no_service,
                dry_run,
                password: None,
                totp,
            }),
            UnattendedCmd::Disable => unattended::disable(),
            UnattendedCmd::Status => unattended::status(),
        },
        Cmd::Service {
            cmd: ServiceCmd::Run,
        } => relay_service::run(&relay_service::current_binary()?).map_err(anyhow::Error::from),
        Cmd::Config { cmd } => match cmd {
            ConfigCmd::List => {
                settings::list();
                Ok(())
            }
            ConfigCmd::Get { key } => {
                println!(
                    "{}",
                    settings::Settings::load().get(&key).unwrap_or_default()
                );
                Ok(())
            }
            ConfigCmd::Set { key, value } => settings::set(&key, &value),
            ConfigCmd::Unset { key } => settings::unset(&key),
        },
        Cmd::Update { cmd } => match cmd {
            UpdateCmd::Check => update::check_cli(),
            UpdateCmd::Apply => update::apply_cli(),
        },
        Cmd::Audit { .. } => {
            match logs::read_audit() {
                Ok(t) => print!("{t}"),
                Err(_) => println!("(no connections recorded yet)"),
            }
            Ok(())
        }
        Cmd::Logs { .. } => {
            println!("{}", logs::data_dir().join("diag.log").display());
            Ok(())
        }
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
            no_clipboard,
            no_files,
            unattended,
            ui: ui_mode,
        } => host::run(host::Options {
            ui: {
                if ui_mode {
                    ui::enable();
                }
                ui_mode
            },
            unattended,
            clipboard: !no_clipboard,
            files: !no_files,
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
            // Without a server, IDs work on the local network (lan.rs).
            let server = server.resolve()?;
            run_viewer(viewer::Target::Id { id, server }, viewer, force_relay)
        }
        Cmd::Wake {
            device,
            via,
            macs,
            password_stdin,
            totp,
            server,
        } => {
            let server = server.resolve()?;
            let device = parse_target(&device, server)?;
            let macs: Vec<wol::Mac> = if macs.is_empty() {
                wol::known(&device.pin_name())
            } else {
                macs.iter()
                    .map(|m| wol::parse(m).with_context(|| format!("not a MAC address: {m}")))
                    .collect::<anyhow::Result<_>>()?
            };
            if macs.is_empty() {
                anyhow::bail!(
                    "no MAC address known for {device}: connect to it once while it is awake, \
                     or pass --mac"
                );
            }
            let via = parse_target(&via, server)?;
            let password = if password_stdin {
                let mut l = String::new();
                std::io::stdin().read_line(&mut l)?;
                l
            } else {
                rpassword::prompt_password(format!("Permanent password for {via}: "))?
            };
            let ask = ask_code(totp, password_stdin, via.to_string());
            let n = tokio::runtime::Runtime::new()?.block_on(wol::wake_via(
                &via,
                password.trim(),
                ask,
                &macs,
            ))?;
            println!(
                "{via} sent {n} magic packets for {device} ({}). It should be online in a minute \
                 if Wake-on-LAN is enabled in its firmware and network settings.",
                macs.iter().map(wol::format).collect::<Vec<_>>().join(", ")
            );
            Ok(())
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
    let st = settings::Settings::load();
    let password = match v.password {
        Some(p) => p,
        None if v.password_stdin => {
            let mut l = String::new();
            std::io::stdin().read_line(&mut l)?;
            l
        }
        None => rpassword::prompt_password(format!("Password for {target}: "))?,
    };
    let target_label = target.to_string();
    viewer::run(viewer::ViewOptions {
        target,
        password: password.trim().to_string(),
        ask_code: Some(ask_code(v.totp.clone(), v.password_stdin, target_label)),
        force_relay,
        measure: v.measure,
        cmd_ctrl_swap: !v.no_cmd_ctrl_swap && st.flag("cmd_ctrl_swap"),
        translate_chars: v.translate_chars,
        clipboard: !v.no_clipboard && st.flag("clipboard"),
        quality: match v.quality.or(match st.get("quality").as_deref() {
            Some("balanced") => Some(Quality::Balanced),
            Some("quality") => Some(Quality::Quality),
            _ => None,
        }) {
            Some(Quality::Balanced) => relay_proto::v1::QualityMode::Balanced,
            Some(Quality::Quality) => relay_proto::v1::QualityMode::Quality,
            _ => relay_proto::v1::QualityMode::Speed,
        },
    })
}

/// A Device ID ("123 456 789") or an address.
fn parse_target(s: &str, server: Option<rdv::Server>) -> anyhow::Result<viewer::Target> {
    match relay_crypto::DeviceId::parse(s) {
        Some(id) => Ok(viewer::Target::Id { id, server }),
        None => Ok(viewer::Target::Direct(parse_addr(s)?)),
    }
}

/// FR-36: a `--totp` code is used once; after that (a reconnect needs a
/// fresh code) ask the terminal, or the app over the stdin pipe.
fn ask_code(given: Option<String>, from_app: bool, target: String) -> auth::AskCode {
    let given = std::sync::Mutex::new(given);
    auth::AskCode(std::sync::Arc::new(move || {
        if let Some(c) = given.lock().expect("poisoned").take() {
            return Ok(c);
        }
        if from_app {
            ui::enable();
            ui::emit("totp_required", serde_json::json!({ "target": target }));
            let mut l = String::new();
            std::io::stdin().read_line(&mut l)?;
            if l.trim().is_empty() {
                anyhow::bail!(auth::Refused("two-factor code cancelled".into()));
            }
            return Ok(l);
        }
        Ok(rpassword::prompt_password(format!(
            "Two-factor code for {target} (from the authenticator app): "
        ))?)
    }))
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

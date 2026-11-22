//! Viewer: QUIC receive → reassemble → decode → present.
//!
//! Threads: winit/wgpu on the main thread (required on macOS); tokio for
//! the network; one decode thread. Encoded frames reach the decoder in
//! order (H.264 P-frames depend on their predecessors); decoded frames
//! reach the renderer through a newest-wins slot, so presentation never
//! lags behind decoding (§6 rule 3).

use std::net::SocketAddr;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use relay_codec::{DecodedFrame, Decoder};
use relay_net::quic;
use relay_net::video::{CompleteFrame, Reassembler};
use relay_proto::MAX_MESSAGE_LEN;
use relay_proto::v1::{self, control_message::Msg};
use relay_session::latest;
use tokio::sync::mpsc as tmpsc;

mod keys;
mod overlay;
mod render;

/// How long a partially received frame may wait for missing slices. Long
/// enough for a big keyframe to cross a slow link; it adds no latency,
/// because any newer complete frame replaces it at once (§6 rule 3).
const REASSEMBLY_DEADLINE: Duration = Duration::from_millis(500);
/// Encoded frames queued for the decoder before we give up and resync.
const DECODE_QUEUE: usize = 6;

/// Events from the network/decoder threads to the window.
#[derive(Debug)]
pub enum UserEvent {
    Frame,
    Cursor(v1::CursorMessage),
    Status(String),
    Closed(String),
}

/// Live numbers for the window title / stats overlay.
#[derive(Debug, Default, Clone)]
pub struct LiveStats {
    pub fps: f64,
    pub mbps: f64,
    pub decode_ms: f64,
    pub dropped: u64,
    pub rtt_ms: f64,
    pub path: &'static str,
    pub loss_pct: f64,
    pub recovered: u64,
    pub queue_ms: f64,
}

pub struct Shared {
    pub stats: Mutex<LiveStats>,
    pub frames: latest::Receiver<DecodedFrame>,
    pub host_name: Mutex<String>,
    pub host: Mutex<HostInfo>,
}

/// What the host told us about itself during setup.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostInfo {
    pub os: Option<relay_input::Os>,
    pub control: bool,
}

#[derive(Debug, Clone)]
pub struct ViewOptions {
    /// Initial quality mode (FR-19, §6 rule 10).
    pub quality: v1::QualityMode,
    pub target: Target,
    pub password: String,
    pub force_relay: bool,
    pub measure: bool,
    pub cmd_ctrl_swap: bool,
    pub translate_chars: bool,
}

pub fn run(opts: ViewOptions) -> anyhow::Result<()> {
    let target = opts.target.clone();
    let opts2 = opts.clone();
    let event_loop = winit::event_loop::EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let (frame_tx, frame_rx) = latest::channel::<DecodedFrame>();
    let shared = Arc::new(Shared {
        stats: Mutex::new(LiveStats {
            path: "direct",
            ..Default::default()
        }),
        frames: frame_rx,
        host_name: Mutex::new(target.to_string()),
        host: Mutex::new(HostInfo::default()),
    });
    let (quit_tx, quit_rx) = tokio::sync::oneshot::channel::<()>();
    // Input from the window to the network. Unbounded: events are tiny and
    // must never be dropped or block the UI thread.
    let (input_tx, input_rx) = tmpsc::unbounded_channel::<v1::InputEvent>();
    // Control messages from the window (quality mode).
    let (ui_tx, ui_rx) = tmpsc::unbounded_channel::<v1::ControlMessage>();

    let net = {
        let proxy = proxy.clone();
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("relay-net".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = proxy.send_event(UserEvent::Closed(e.to_string()));
                        return;
                    }
                };
                let res = rt.block_on(network(
                    opts2,
                    frame_tx,
                    proxy.clone(),
                    shared,
                    quit_rx,
                    input_rx,
                    ui_rx,
                ));
                let msg = match res {
                    Ok(m) => m,
                    Err(e) => format!("{e:#}"),
                };
                let _ = proxy.send_event(UserEvent::Closed(msg));
            })?
    };

    let mut app = render::App::new(shared, quit_tx, input_tx, ui_tx, &opts);
    event_loop.run_app(&mut app)?;
    let _ = net.join();
    if let Some(msg) = app.close_reason() {
        eprintln!("{msg}");
    }
    Ok(())
}

/// Where to connect.
#[derive(Debug, Clone)]
pub enum Target {
    /// FR-7: direct IP, no rendezvous.
    Direct(SocketAddr),
    /// By Device ID through the rendezvous server.
    Id {
        id: relay_crypto::DeviceId,
        server: crate::rdv::Server,
    },
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Direct(a) => write!(f, "{a}"),
            Target::Id { id, .. } => write!(f, "{id}"),
        }
    }
}

impl Target {
    /// Name under which the host key is pinned (FR-5).
    pub fn pin_name(&self) -> String {
        match self {
            Target::Direct(a) => a.to_string(),
            Target::Id { id, .. } => id.as_u32().to_string(),
        }
    }
}

/// A connected, key-verified (not yet password-authenticated) session.
pub struct Established {
    /// Owns the UDP socket; must outlive the connection.
    pub node: relay_net::quic::Node,
    pub conn: quinn::Connection,
    pub path: crate::rdv::Path,
    pub hub: crate::rdv::ControlHub,
    /// Set when relayed: what to probe for a direct path (FR-4).
    pub relay: Option<crate::rdv::Expected>,
}

pub async fn establish(
    me: &crate::identity::Me,
    target: &Target,
    force_relay: bool,
) -> anyhow::Result<Established> {
    let (node, control) = quic::node(SocketAddr::from(([0, 0, 0, 0], 0)), &me.tls, None)?;
    let hub = crate::rdv::ControlHub::spawn(control);
    let (conn, path, relay) = match target {
        Target::Direct(addr) => {
            let (cfg, _) = quic::client_config(
                &me.tls,
                relay_net::tls::Expect::AnyRecord,
                quic::ALPN,
                Duration::from_secs(2),
            )?;
            let conn = tokio::time::timeout(
                Duration::from_secs(5),
                quic::connect(&node.endpoint, cfg, *addr),
            )
            .await
            .with_context(|| {
                format!("no answer from {addr} (is `relay host` running and reachable?)")
            })??;
            (conn, crate::rdv::Path::Direct, None)
        }
        Target::Id { id, server } => {
            crate::rdv::connect_by_id(&node, &hub, me, server, *id, force_relay).await?
        }
    };
    let key = relay_net::tls::peer_public_key(&conn).context("host presented no device key")?;
    let name = target.pin_name();
    match crate::pins::check(&name, &key) {
        relay_crypto::PinCheck::Match => {}
        relay_crypto::PinCheck::New => {
            tracing::info!(
                "first connection to {target}; remembering its key {}",
                relay_net::hex(&key)
            );
            crate::pins::pin(&name, &key);
        }
        relay_crypto::PinCheck::Changed => {
            conn.close(1u32.into(), b"key changed");
            bail!(
                "WARNING: the device key of {target} has changed since you last connected.\n\
                 It may have been reinstalled, or someone may be impersonating it.\n\
                 If you trust the change, run `relay forget {name}` and connect again."
            );
        }
    }
    tracing::info!("connected to {target} ({})", path.label());
    Ok(Established {
        node,
        conn,
        path,
        hub,
        relay,
    })
}

fn control(msg: Msg) -> v1::ControlMessage {
    v1::ControlMessage { msg: Some(msg) }
}

async fn network(
    ViewOptions {
        target,
        password,
        force_relay,
        quality,
        ..
    }: ViewOptions,
    frames_out: latest::Sender<DecodedFrame>,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    shared: Arc<Shared>,
    mut quit: tokio::sync::oneshot::Receiver<()>,
    mut input_rx: tmpsc::UnboundedReceiver<v1::InputEvent>,
    mut ui_rx: tmpsc::UnboundedReceiver<v1::ControlMessage>,
) -> anyhow::Result<String> {
    let _ = proxy.send_event(UserEvent::Status(format!("Connecting to {}…", target)));
    let me = crate::identity::load()?;
    let est = establish(&me, &target, force_relay).await?;
    let conn = est.conn.clone();
    shared.stats.lock().expect("poisoned").path = est.path.label();
    *shared.host_name.lock().expect("poisoned") = target.to_string();
    let (mut tx, mut rx) = conn.open_bi().await?;
    let name = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "viewer".into());
    let _ = proxy.send_event(UserEvent::Status("Waiting for the host to accept…".into()));
    crate::auth::controller(&conn, &mut tx, &mut rx, &me, &name, &password).await?;
    let mut stream_cfg = None;
    while stream_cfg.is_none() {
        match quic::recv_msg::<v1::ControlMessage>(&mut rx, MAX_MESSAGE_LEN).await? {
            Some(v1::ControlMessage {
                msg: Some(Msg::StreamConfig(c)),
            }) => stream_cfg = Some(c),
            Some(v1::ControlMessage {
                msg: Some(Msg::Hello(h)),
            }) => {
                let os = match v1::OsKind::try_from(h.os) {
                    Ok(v1::OsKind::Macos) => Some(relay_input::Os::MacOs),
                    Ok(v1::OsKind::Windows) => Some(relay_input::Os::Windows),
                    _ => None,
                };
                shared.host.lock().expect("poisoned").os = os;
                if !h.device_name.is_empty() {
                    *shared.host_name.lock().expect("poisoned") = h.device_name.clone();
                }
            }
            Some(v1::ControlMessage {
                msg: Some(Msg::Permissions(p)),
            }) => {
                shared.host.lock().expect("poisoned").control = p.control;
                if !p.control {
                    tracing::info!("host allows viewing only");
                }
            }
            Some(v1::ControlMessage {
                msg: Some(Msg::Monitors(m)),
            }) => {
                if let Some(d) = m.displays.first() {
                    let mut n = shared.host_name.lock().expect("poisoned");
                    *n = format!("{n} · {}x{}", d.width, d.height);
                }
            }
            Some(_) => {}
            None => bail!("host closed the control stream"),
        }
    }
    let cfg = stream_cfg.expect("set");
    tracing::info!("stream {}x{} @{} fps", cfg.width, cfg.height, cfg.fps);
    let _ = proxy.send_event(UserEvent::Status("Waiting for first frame…".into()));

    // Control writer: IDR requests and ABR feedback share the stream.
    let (ctl_tx, mut ctl_rx) = tmpsc::unbounded_channel::<v1::ControlMessage>();
    let writer = tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if quic::send_msg(&mut tx, &m).await.is_err() {
                break;
            }
        }
        let _ = tx.finish();
    });

    // Window → host control messages; start in the requested mode.
    if quality != v1::QualityMode::Speed {
        let _ = ctl_tx.send(control(Msg::SetQuality(v1::SetQuality {
            mode: quality as i32,
        })));
    }
    let ui_forward = {
        let ctl = ctl_tx.clone();
        tokio::spawn(async move {
            while let Some(m) = ui_rx.recv().await {
                if ctl.send(m).is_err() {
                    break;
                }
            }
        })
    };

    // Input: its own unidirectional stream at top priority (§6 rule 8),
    // so it is never queued behind other stream data.
    let input_writer = {
        let mut s = conn.open_uni().await?;
        let _ = s.set_priority(relay_net::channel::Channel::Input.priority());
        s.write_all(&[relay_net::channel::Channel::Input as u8])
            .await?;
        tokio::spawn(async move {
            while let Some(ev) = input_rx.recv().await {
                if quic::send_msg(&mut s, &ev).await.is_err() {
                    break;
                }
            }
            let _ = s.finish();
        })
    };

    // Streams opened by the host (cursor).
    let streams = {
        let conn = conn.clone();
        let proxy = proxy.clone();
        tokio::spawn(async move {
            while let Ok(mut rx) = conn.accept_uni().await {
                let mut tag = [0u8; 1];
                if rx.read_exact(&mut tag).await.is_err() {
                    continue;
                }
                if relay_net::channel::Channel::from_u8(tag[0])
                    == Some(relay_net::channel::Channel::Cursor)
                {
                    let proxy = proxy.clone();
                    tokio::spawn(async move {
                        // 256x256 RGBA plus headers.
                        while let Ok(Some(m)) =
                            quic::recv_msg::<v1::CursorMessage>(&mut rx, 300_000).await
                        {
                            if proxy.send_event(UserEvent::Cursor(m)).is_err() {
                                break;
                            }
                        }
                    });
                }
            }
        })
    };

    // Decoder thread.
    let (dec_tx, dec_rx) = mpsc::sync_channel::<CompleteFrame>(DECODE_QUEUE);
    let decoder = {
        let ctl = ctl_tx.clone();
        let proxy = proxy.clone();
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("relay-decode".into())
            .spawn(move || {
                crate::rt::set_realtime();
                decode_loop(dec_rx, frames_out, ctl, proxy, shared)
            })?
    };

    // Relayed: keep probing for a direct path and move the session onto
    // it as soon as one answers (FR-4).
    let switched = Arc::new(tokio::sync::Notify::new());
    let migration = match est.relay.clone() {
        Some(exp) => {
            let mut prober =
                crate::migrate::Prober::start(&est.node, &est.hub, &exp, &quic::exporter(&conn)?);
            let paths = est.node.paths.clone();
            let (ctl, shared, switched) = (ctl_tx.clone(), shared.clone(), switched.clone());
            Some(tokio::spawn(async move {
                let Some(direct) = prober.first_ack().await else {
                    return;
                };
                crate::migrate::controller_prepare(&paths, exp.relay, direct);
                loop {
                    let _ = ctl.send(control(Msg::PathSwitch(v1::PathSwitch {})));
                    tokio::select! {
                        _ = switched.notified() => break,
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                }
                crate::migrate::controller_commit(&paths, exp.relay, direct);
                shared.stats.lock().expect("poisoned").path = "P2P (was relay)";
                // Keep answering the host's probes for the rest of the session.
                std::future::pending::<()>().await;
                drop(prober);
            }))
        }
        None => None,
    };

    let host_closed = receive(
        &conn, dec_tx, &ctl_tx, &shared, &mut rx, &mut quit, &switched,
    )
    .await;
    if let Some(m) = migration {
        m.abort();
    }
    let _ = ctl_tx.send(control(Msg::End(v1::SessionEnd {
        reason: v1::EndReason::ControllerLeft as i32,
        detail: String::new(),
    })));
    drop(ctl_tx);
    input_writer.abort();
    streams.abort();
    ui_forward.abort();
    let _ = tokio::time::timeout(Duration::from_millis(300), writer).await;
    conn.close(0u32.into(), b"viewer closed");
    let _ = tokio::task::spawn_blocking(move || decoder.join()).await;
    est.node.endpoint.wait_idle().await;
    Ok(host_closed)
}

/// Datagram receive loop. Returns why the session ended.
async fn receive(
    conn: &quinn::Connection,
    dec_tx: SyncSender<CompleteFrame>,
    ctl: &tmpsc::UnboundedSender<v1::ControlMessage>,
    shared: &Shared,
    ctl_rx: &mut quinn::RecvStream,
    quit: &mut tokio::sync::oneshot::Receiver<()>,
    switched: &tokio::sync::Notify,
) -> String {
    let mut reasm = Reassembler::new(REASSEMBLY_DEADLINE);
    let mut fb = crate::feedback::Feedback::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut need_key = false;
    loop {
        tokio::select! {
            dg = conn.read_datagram() => {
                let dg = match dg {
                    Ok(d) => d,
                    Err(e) => return format!("connection closed: {e}"),
                };
                let now = Instant::now();
                fb.bytes += dg.len();
                fb.on_datagram(&dg);
                let Some(frame) = ({
                    let _s = tracing::trace_span!("reassemble").entered();
                    reasm.push(dg, now)
                }) else { continue };
                if need_key && !frame.is_key {
                    continue; // waiting for the requested keyframe
                }
                need_key = false;
                if frame.gap_before {
                    let _ = ctl.send(idr_request(frame.frame_no));
                }
                match dec_tx.try_send(frame) {
                    Ok(()) => {}
                    Err(TrySendError::Full(f)) => {
                        // Decoder is behind: skip ahead to a fresh keyframe
                        // rather than build latency.
                        need_key = true;
                        let _ = ctl.send(idr_request(f.frame_no));
                    }
                    Err(TrySendError::Disconnected(_)) => return "decoder stopped".into(),
                }
            }
            _ = tick.tick() => {
                reasm.expire(Instant::now());
                let st = reasm.stats();
                let report = fb.report(&st);
                {
                    let mut s = shared.stats.lock().expect("poisoned");
                    s.mbps = f64::from(report.receive_kbps) / 1000.0;
                    s.dropped = st.frames_dropped;
                    s.recovered = st.frames_recovered;
                    s.loss_pct = f64::from(report.loss_permille) / 10.0;
                    s.queue_ms = f64::from(report.delay_trend_us.max(0)) / 1000.0;
                    s.rtt_ms = conn.rtt().as_secs_f64() * 1000.0;
                }
                let _ = ctl.send(control(Msg::AbrFeedback(report)));
            }
            m = quic::recv_msg::<v1::ControlMessage>(ctl_rx, MAX_MESSAGE_LEN) => match m {
                Ok(Some(v1::ControlMessage { msg: Some(Msg::End(e)) })) => {
                    return match v1::EndReason::try_from(e.reason) {
                        Ok(v1::EndReason::HostEnded) => "The host ended the session.".into(),
                        _ => "Session ended by host.".into(),
                    };
                }
                Ok(Some(v1::ControlMessage { msg: Some(Msg::PathSwitched(_)) })) => switched.notify_one(),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return "host closed the connection".into(),
            },
            _ = &mut *quit => return String::new(),
        }
    }
}

fn idr_request(frame_no: u32) -> v1::ControlMessage {
    control(Msg::IdrRequest(v1::IdrRequest {
        stream_id: 0,
        last_good_frame: u64::from(frame_no),
    }))
}

fn decode_loop(
    rx: mpsc::Receiver<CompleteFrame>,
    out: latest::Sender<DecodedFrame>,
    ctl: tmpsc::UnboundedSender<v1::ControlMessage>,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    shared: Arc<Shared>,
) {
    let mut dec = match relay_codec::openh264::OpenH264Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Closed(format!("decoder: {e}")));
            return;
        }
    };
    let (mut n, mut total, mut since) = (0u32, Duration::ZERO, Instant::now());
    while let Ok(frame) = rx.recv() {
        let t = Instant::now();
        let res = {
            let _s = tracing::trace_span!("decode", frame_no = frame.frame_no).entered();
            dec.decode(&frame.data, frame.capture_ts_us)
        };
        match res {
            Ok(Some(d)) => {
                total += t.elapsed();
                n += 1;
                out.send(d);
                if proxy.send_event(UserEvent::Frame).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("decode error on frame {}: {e}", frame.frame_no);
                let _ = ctl.send(idr_request(frame.frame_no));
            }
        }
        let el = since.elapsed();
        if el >= Duration::from_millis(500) {
            let mut s = shared.stats.lock().expect("poisoned");
            s.fps = f64::from(n) / el.as_secs_f64();
            s.decode_ms = if n > 0 {
                total.as_secs_f64() * 1000.0 / f64::from(n)
            } else {
                0.0
            };
            (n, total, since) = (0, Duration::ZERO, Instant::now());
        }
    }
}

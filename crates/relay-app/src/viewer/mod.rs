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
use relay_proto::v1::{self, control_message::Msg};
use relay_proto::{MAX_MESSAGE_LEN, PROTOCOL_VERSION};
use relay_session::latest;
use tokio::sync::mpsc as tmpsc;

use crate::clock;

mod keys;
mod render;

/// How long a partially received frame may wait for missing slices.
const REASSEMBLY_DEADLINE: Duration = Duration::from_millis(60);
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
    pub addr: SocketAddr,
    pub code: String,
    pub measure: bool,
    pub cmd_ctrl_swap: bool,
    pub translate_chars: bool,
}

pub fn run(opts: ViewOptions) -> anyhow::Result<()> {
    let ViewOptions { addr, code, .. } = opts.clone();
    let event_loop = winit::event_loop::EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let (frame_tx, frame_rx) = latest::channel::<DecodedFrame>();
    let shared = Arc::new(Shared {
        stats: Mutex::new(LiveStats {
            path: "direct",
            ..Default::default()
        }),
        frames: frame_rx,
        host_name: Mutex::new(addr.to_string()),
        host: Mutex::new(HostInfo::default()),
    });
    let (quit_tx, quit_rx) = tokio::sync::oneshot::channel::<()>();
    // Input from the window to the network. Unbounded: events are tiny and
    // must never be dropped or block the UI thread.
    let (input_tx, input_rx) = tmpsc::unbounded_channel::<v1::InputEvent>();

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
                    addr,
                    code,
                    frame_tx,
                    proxy.clone(),
                    shared,
                    quit_rx,
                    input_rx,
                ));
                let msg = match res {
                    Ok(m) => m,
                    Err(e) => format!("{e:#}"),
                };
                let _ = proxy.send_event(UserEvent::Closed(msg));
            })?
    };

    let mut app = render::App::new(shared, quit_tx, input_tx, &opts);
    event_loop.run_app(&mut app)?;
    let _ = net.join();
    if let Some(msg) = app.close_reason() {
        eprintln!("{msg}");
    }
    Ok(())
}

fn control(msg: Msg) -> v1::ControlMessage {
    v1::ControlMessage { msg: Some(msg) }
}

async fn network(
    addr: SocketAddr,
    code: String,
    frames_out: latest::Sender<DecodedFrame>,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    shared: Arc<Shared>,
    mut quit: tokio::sync::oneshot::Receiver<()>,
    mut input_rx: tmpsc::UnboundedReceiver<v1::InputEvent>,
) -> anyhow::Result<String> {
    let _ = proxy.send_event(UserEvent::Status(format!("Connecting to {addr}…")));
    let (ep, seen) = quic::client()?;
    let conn = tokio::time::timeout(Duration::from_secs(5), quic::connect(&ep, addr))
        .await
        .with_context(|| {
            format!("no answer from {addr} (is `relay host` running and reachable?)")
        })??;
    tracing::info!(
        "connected to {addr}, host certificate {}",
        seen.get().unwrap_or_default()
    );
    let (mut tx, mut rx) = conn.open_bi().await?;
    let name = std::env::var("USER").unwrap_or_else(|_| "viewer".into());
    quic::send_msg(
        &mut tx,
        &control(Msg::Hello(v1::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: name,
            os: if cfg!(target_os = "macos") {
                v1::OsKind::Macos
            } else {
                v1::OsKind::Windows
            } as i32,
            capabilities: Some(v1::Capabilities {
                decoders: vec![v1::Codec::H264 as i32],
                ..Default::default()
            }),
            ..Default::default()
        })),
    )
    .await?;
    quic::send_msg(
        &mut tx,
        &control(Msg::Auth(v1::AuthMessage {
            step: 0,
            payload: code.trim().as_bytes().to_vec().into(),
        })),
    )
    .await?;

    match quic::recv_msg::<v1::ControlMessage>(&mut rx, MAX_MESSAGE_LEN).await? {
        Some(v1::ControlMessage {
            msg: Some(Msg::AuthOutcome(o)),
        }) => match v1::AuthResult::try_from(o.result).unwrap_or(v1::AuthResult::Unspecified) {
            v1::AuthResult::Ok => {}
            v1::AuthResult::BadPassword => bail!("wrong session code"),
            v1::AuthResult::LockedOut => {
                bail!(
                    "too many attempts; try again in {} s",
                    o.retry_after_ms.div_ceil(1000)
                )
            }
            other => bail!("host refused the connection: {other:?}"),
        },
        _ => bail!("host closed the connection during authentication"),
    }
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

    let host_closed = receive(&conn, dec_tx, &ctl_tx, &shared, &mut rx, &mut quit).await;
    let _ = ctl_tx.send(control(Msg::End(v1::SessionEnd {
        reason: v1::EndReason::ControllerLeft as i32,
        detail: String::new(),
    })));
    drop(ctl_tx);
    input_writer.abort();
    streams.abort();
    let _ = tokio::time::timeout(Duration::from_millis(300), writer).await;
    conn.close(0u32.into(), b"viewer closed");
    let _ = tokio::task::spawn_blocking(move || decoder.join()).await;
    ep.wait_idle().await;
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
) -> String {
    let mut reasm = Reassembler::new(REASSEMBLY_DEADLINE);
    let mut fb = Feedback::new();
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
                let Some(frame) = ({
                    let _s = tracing::trace_span!("reassemble").entered();
                    reasm.push(dg, now)
                }) else { continue };
                fb.on_frame(frame.capture_ts_us);
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
                let report = fb.report(st.frames_delivered, st.frames_dropped);
                {
                    let mut s = shared.stats.lock().expect("poisoned");
                    s.mbps = f64::from(report.receive_kbps) / 1000.0;
                    s.dropped = st.frames_dropped;
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

/// Builds the 100 ms ABR report (§6 rule 7).
///
/// One-way delay is measured as `arrival - capture_ts`. The clocks are not
/// synchronised, so the absolute value is meaningless, but its growth
/// above the recent minimum is queueing delay.
struct Feedback {
    bytes: usize,
    since: Instant,
    window_min: Option<i64>,
    history: std::collections::VecDeque<i64>,
    last_delivered: u64,
    last_dropped: u64,
}

impl Feedback {
    const BASELINE_WINDOWS: usize = 50; // 5 s

    fn new() -> Self {
        Self {
            bytes: 0,
            since: Instant::now(),
            window_min: None,
            history: Default::default(),
            last_delivered: 0,
            last_dropped: 0,
        }
    }

    fn on_frame(&mut self, capture_ts_us: u64) {
        let owd = clock::now_us() as i64 - capture_ts_us as i64;
        self.window_min = Some(self.window_min.map_or(owd, |m| m.min(owd)));
    }

    fn report(&mut self, delivered: u64, dropped: u64) -> v1::AbrFeedback {
        let el = self.since.elapsed().as_secs_f64().max(1e-3);
        let receive_kbps = (self.bytes as f64 * 8.0 / el / 1000.0) as u32;
        let d_del = delivered - self.last_delivered;
        let d_drop = dropped - self.last_dropped;
        let loss_permille = (d_drop * 1000).checked_div(d_del + d_drop).unwrap_or(0) as u32;
        let mut delay_trend_us = 0;
        if let Some(m) = self.window_min.take() {
            let baseline = self.history.iter().copied().min().unwrap_or(m).min(m);
            delay_trend_us = i32::try_from(m - baseline).unwrap_or(i32::MAX);
            self.history.push_back(m);
            if self.history.len() > Self::BASELINE_WINDOWS {
                self.history.pop_front();
            }
        }
        self.bytes = 0;
        self.since = Instant::now();
        self.last_delivered = delivered;
        self.last_dropped = dropped;
        v1::AbrFeedback {
            receive_kbps,
            loss_permille,
            delay_trend_us,
            decode_time_us: 0,
            frames_dropped: d_drop as u32,
        }
    }
}

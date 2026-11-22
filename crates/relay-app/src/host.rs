//! Host: capture → encode → send over QUIC datagrams.
//!
//! Pipeline threads (§6 rule 9): the capture backend hands frames to a
//! newest-wins slot; one real-time thread encodes and sends. Control
//! (auth, IDR requests, ABR feedback) runs on tokio.
//!
//! M1 safety stand-ins until the Tauri UI (FR-8/9/10/39): access needs the
//! session code; the terminal prints a live "being viewed" banner and
//! audit lines; Ctrl+C ends the session immediately; the code rotates
//! after every session.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use relay_capture::CapturedFrame;
use relay_codec::{Codec, Encoder, EncoderConfig};
use relay_crypto::pake::{HostPassword, PasswordKind};
use relay_crypto::password::OneTimePassword;
use relay_net::quic::{self, Connection};
use relay_net::video::{DEFAULT_MAX_DATAGRAM, FrameMeta, slice_frame};
use relay_proto::v1::{self, control_message::Msg};
use relay_proto::{MAX_MESSAGE_LEN, PROTOCOL_VERSION};
use relay_session::abr::{AbrConfig, AbrController};
use relay_session::auth_limit::AuthLimiter;
use relay_session::idr::IdrLimiter;

use crate::{CodecChoice, clock, rt};

#[derive(Debug, Clone)]
pub struct Options {
    pub bind: SocketAddr,
    pub display: Option<u32>,
    pub list_displays: bool,
    pub test_pattern: Option<(u32, u32)>,
    pub codec: CodecChoice,
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub view_only: bool,
    pub server: Option<crate::rdv::Server>,
}

/// Keep-alive cadence when the screen is static (§6 rule 4).
const KEEPALIVE: Duration = Duration::from_secs(1);

fn capturer(opts: &Options) -> anyhow::Result<Box<dyn relay_capture::ScreenCapturer>> {
    Ok(match opts.test_pattern {
        Some((w, h)) => Box::new(relay_capture::pattern::PatternCapturer::new(w, h, opts.fps)),
        None => relay_capture::platform_capturer().context("screen capture unavailable")?,
    })
}

pub fn run(opts: Options) -> anyhow::Result<()> {
    let cap = capturer(&opts)?;
    let displays = cap.displays().map_err(|e| match e {
        relay_capture::CaptureError::PermissionDenied if cfg!(target_os = "macos") => anyhow::anyhow!(
            "Screen Recording permission is not granted.\n  Open System Settings → Privacy & Security → Screen & System Audio Recording,\n  enable your terminal app, then restart it."
        ),
        e => e.into(),
    })?;
    if opts.list_displays {
        for d in &displays {
            println!(
                "{:>10}  {}x{} @{:.1}x  at ({},{}){}",
                d.id,
                d.width,
                d.height,
                d.scale,
                d.x,
                d.y,
                if d.primary { "  primary" } else { "" }
            );
        }
        return Ok(());
    }
    let display = match opts.display {
        Some(id) => displays
            .iter()
            .find(|d| d.id == id)
            .with_context(|| format!("no display {id}; see --list-displays"))?,
        None => displays
            .iter()
            .find(|d| d.primary)
            .or(displays.first())
            .context("no displays")?,
    }
    .clone();
    drop(cap);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(opts, display))
}

/// Per-session credentials. The one-time password rotates after every
/// session (FR-2); only its OPAQUE verifier is kept.
struct Credentials {
    otp: OneTimePassword,
    verifier: HostPassword,
}

impl Credentials {
    fn new() -> anyhow::Result<Self> {
        let otp = OneTimePassword::generate()?;
        let verifier = HostPassword::new(otp.as_str(), PasswordKind::OneTime)?;
        Ok(Self { otp, verifier })
    }
}

/// FR-35: failures are counted per source IP and per Controller key.
#[derive(Default)]
pub struct Limiters {
    ip: AuthLimiter<IpAddr>,
    key: AuthLimiter<[u8; 32]>,
}

struct HostState {
    node: Arc<relay_net::quic::Node>,
    hub: crate::rdv::ControlHub,
    opts: Options,
    display: relay_capture::DisplayInfo,
    me: Arc<crate::identity::Me>,
    creds: Mutex<Arc<Credentials>>,
    limiters: Mutex<Limiters>,
    /// Held for the whole streaming session: one Controller at a time.
    streaming: tokio::sync::Mutex<()>,
    quit: tokio::sync::Notify,
}

impl HostState {
    fn rotate(&self) {
        match Credentials::new() {
            Ok(c) => {
                println!("  New password: {}\n", c.otp.as_str());
                *self.creds.lock().expect("poisoned") = Arc::new(c);
            }
            Err(e) => tracing::error!("could not rotate the password: {e}"),
        }
    }
}

async fn serve(opts: Options, display: relay_capture::DisplayInfo) -> anyhow::Result<()> {
    let me = Arc::new(crate::identity::load()?);
    let (node, control) = quic::node(opts.bind, &me.tls, Some(&[quic::ALPN]))?;
    let node = Arc::new(node);
    let hub = crate::rdv::ControlHub::spawn(control);
    let creds = Credentials::new()?;
    let limiters = Limiters::default();

    println!();
    println!("  Relay host ready");
    let (id_tx, mut id_rx) = tokio::sync::watch::channel(None);
    if let Some(server) = opts.server {
        tokio::spawn(crate::rdv::host_loop(
            node.clone(),
            hub.clone(),
            me.clone(),
            server,
            id_tx,
        ));
        let _ = tokio::time::timeout(Duration::from_secs(5), id_rx.wait_for(|v| v.is_some())).await;
        match *id_rx.borrow() {
            Some(id) => println!("  Your ID       {id}"),
            None => println!(
                "  Your ID       (rendezvous server {} not reachable yet; retrying)",
                server.addr
            ),
        }
    }
    println!("  Password      {}", creds.otp.as_str());
    let port = node.endpoint.local_addr()?.port();
    for c in crate::rdv::local_candidates(&node) {
        let ip = c
            .rsplit_once(':')
            .map_or(c.as_str(), |(ip, _)| ip)
            .to_string();
        println!("  Direct        relay view {ip}:{port}");
    }
    println!("  Device key    {}", relay_net::hex(&me.public_key()));
    println!(
        "  Sharing       display {} ({}x{}){}",
        display.id,
        display.width,
        display.height,
        if opts.view_only { ", view only" } else { "" }
    );
    println!("  Press Ctrl+C to quit.\n");

    let st = Arc::new(HostState {
        node: node.clone(),
        hub: hub.clone(),
        opts: opts.clone(),
        display: display.clone(),
        me: me.clone(),
        creds: Mutex::new(Arc::new(creds)),
        limiters: Mutex::new(limiters),
        streaming: tokio::sync::Mutex::new(()),
        quit: tokio::sync::Notify::new(),
    });
    loop {
        let incoming = tokio::select! {
            i = node.endpoint.accept() => match i { Some(i) => i, None => break },
            _ = id_rx.changed() => {
                if let Some(id) = *id_rx.borrow() {
                    println!("  Your ID       {id}");
                }
                continue;
            }
            _ = st.quit.notified() => break,
            _ = tokio::signal::ctrl_c() => break,
        };
        // Each connection authenticates on its own task, so a slow or
        // stalled client cannot block others; streaming is exclusive.
        let st = st.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!("handshake from {remote} failed: {e}");
                    return;
                }
            };
            match session(&conn, &st).await {
                Ok(SessionOutcome::Rejected) => {}
                Ok(SessionOutcome::Ended { quit }) => {
                    st.rotate();
                    if quit {
                        st.quit.notify_one();
                    }
                }
                Err(e) => tracing::debug!("connection from {remote}: {e:#}"),
            }
        });
    }
    node.endpoint.close(0u32.into(), b"host shutting down");
    node.endpoint.wait_idle().await;
    Ok(())
}

enum SessionOutcome {
    Rejected,
    Ended { quit: bool },
}

fn control(msg: Msg) -> v1::ControlMessage {
    v1::ControlMessage { msg: Some(msg) }
}

async fn session(conn: &Connection, st: &HostState) -> anyhow::Result<SessionOutcome> {
    use crate::auth;
    let (opts, display, me) = (&st.opts, &st.display, &*st.me);
    let creds = st.creds.lock().expect("poisoned").clone();
    let remote = conn.remote_address();
    let ip = remote.ip();
    let (mut tx, mut rx) = tokio::time::timeout(Duration::from_secs(5), conn.accept_bi())
        .await
        .context("no control stream")??;
    let peer = auth::host_hello(conn, &mut rx).await?;
    let name = format!(
        "{} ({}, {remote})",
        if peer.name.is_empty() {
            "unknown device"
        } else {
            &peer.name
        },
        peer.id
            .map_or_else(|| "unregistered".to_string(), |id| format!("ID {id}")),
    );

    let reject = async |tx: &mut quic::SendStream,
                        r: v1::AuthResult,
                        retry_ms: u32|
           -> anyhow::Result<SessionOutcome> {
        auth::send_outcome(tx, r, retry_ms).await?;
        let _ = tx.finish();
        tokio::time::sleep(Duration::from_millis(200)).await;
        conn.close(1u32.into(), b"rejected");
        Ok(SessionOutcome::Rejected)
    };

    // FR-35: refuse locked-out sources before doing any work for them.
    let now = Instant::now();
    let checked = {
        let l = st.limiters.lock().expect("poisoned");
        l.ip.check(&ip, now).and(l.key.check(&peer.key, now))
    };
    if let Err(wait) = checked {
        audit(&format!(
            "REJECTED {name}: locked out for {}s",
            wait.as_secs()
        ));
        return reject(&mut tx, v1::AuthResult::LockedOut, wait.as_millis() as u32).await;
    }
    // Count the attempt before the exchange. With OPAQUE the Controller
    // learns from KE2 whether its guess was right, so an attacker could
    // hang up before KE3; every started login must cost one try.
    let lock = {
        let mut l = st.limiters.lock().expect("poisoned");
        let a = l.ip.record_failure(&ip, now);
        a.max(l.key.record_failure(&peer.key, now))
    };
    let result = auth::host_login(conn, &mut tx, &mut rx, me, &peer, &creds.verifier).await;
    match result {
        Ok(auth::HostAuth::Ok) => {
            let mut l = st.limiters.lock().expect("poisoned");
            l.ip.record_success(&ip);
            l.key.record_success(&peer.key);
        }
        Ok(auth::HostAuth::BadPassword) | Err(_) => {
            audit(&format!("FAILED login from {name}"));
            println!("  ! Failed connection attempt from {name}");
            if result.is_err() {
                conn.close(1u32.into(), b"auth aborted");
                return Ok(SessionOutcome::Rejected);
            }
            return match lock {
                Some(d) => reject(&mut tx, v1::AuthResult::LockedOut, d.as_millis() as u32).await,
                None => reject(&mut tx, v1::AuthResult::BadPassword, 0).await,
            };
        }
    }

    // One session (and one consent dialog) at a time.
    let Ok(_streaming) = st.streaming.try_lock() else {
        println!("  ○ {name} tried to connect while another session is active.");
        return reject(&mut tx, v1::AuthResult::Declined, 0).await;
    };

    // FR-8: an attended session needs the local user's explicit yes.
    if !crate::consent::ask(&name).await {
        audit(&format!("DECLINED {name}"));
        println!("  ○ Declined {name}.");
        return reject(&mut tx, v1::AuthResult::Declined, 0).await;
    }
    auth::send_outcome(&mut tx, v1::AuthResult::Ok, 0).await?;

    // Relayed? Keep looking for a direct path for the whole session (FR-4).
    let expected = st
        .hub
        .expected
        .lock()
        .expect("poisoned")
        .get(&peer.key)
        .cloned();
    let relayed = expected.filter(|e| e.relay == remote);
    let prober = match &relayed {
        Some(e) => Some(crate::migrate::Prober::start(
            &st.node,
            &st.hub,
            e,
            &crate::migrate::exporter(conn)?,
        )),
        None => None,
    };

    let started = Instant::now();
    audit(&format!("SESSION START {name} (attended)"));
    println!(
        "  ● {} by {name}. Press Ctrl+C to end the session.",
        if opts.view_only {
            "Being viewed"
        } else {
            "Being viewed and controlled"
        }
    );

    let (w, h) = (display.width & !1, display.height & !1);
    quic::send_msg(
        &mut tx,
        &control(Msg::Hello(v1::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: hostname(),
            os: if cfg!(target_os = "macos") {
                v1::OsKind::Macos
            } else {
                v1::OsKind::Windows
            } as i32,
            ..Default::default()
        })),
    )
    .await?;
    quic::send_msg(
        &mut tx,
        &control(Msg::Permissions(v1::Permissions {
            view: true,
            control: !opts.view_only,
            clipboard: false,
            file_transfer: false,
        })),
    )
    .await?;
    // Cursor channel (§6 rule 5): its own stream, below input priority.
    let (cursor_tx, mut cursor_rx) = tokio::sync::mpsc::unbounded_channel::<v1::CursorMessage>();
    let cursor_task = {
        let conn = conn.clone();
        tokio::spawn(async move {
            let Ok(mut s) = conn.open_uni().await else {
                return;
            };
            let _ = s.set_priority(relay_net::channel::Channel::Cursor.priority());
            if s.write_all(&[relay_net::channel::Channel::Cursor as u8])
                .await
                .is_err()
            {
                return;
            }
            while let Some(m) = cursor_rx.recv().await {
                if quic::send_msg(&mut s, &m).await.is_err() {
                    break;
                }
            }
        })
    };
    let input_task = tokio::spawn(crate::host_input::serve(
        conn.clone(),
        relay_input::DisplayTarget {
            id: display.id,
            x: display.x,
            y: display.y,
            width: display.width,
            height: display.height,
        },
        !opts.view_only,
    ));
    quic::send_msg(
        &mut tx,
        &control(Msg::Monitors(v1::MonitorList {
            displays: vec![v1::Display {
                id: display.id,
                name: display.name.clone(),
                x: display.x,
                y: display.y,
                width: display.width,
                height: display.height,
                scale: display.scale,
                primary: display.primary,
            }],
        })),
    )
    .await?;
    quic::send_msg(
        &mut tx,
        &control(Msg::StreamConfig(v1::StreamConfig {
            stream_id: 0,
            codec: v1::Codec::H264 as i32,
            width: w,
            height: h,
            fps: opts.fps,
            bitrate_kbps: opts.bitrate_kbps,
        })),
    )
    .await?;

    let shared = Arc::new(PipelineShared {
        stop: AtomicBool::new(false),
        force_key: AtomicBool::new(true),
        bitrate_kbps: AtomicU32::new(opts.bitrate_kbps),
        error: Mutex::new(None),
    });
    let pipeline = {
        let shared = shared.clone();
        let conn = conn.clone();
        let opts = opts.clone();
        let display_id = display.id;
        let cursor_tx = cursor_tx.clone();
        std::thread::Builder::new()
            .name("relay-encode".into())
            .spawn(move || {
                rt::set_realtime();
                if let Err(e) = pipeline(&conn, &opts, display_id, w, h, &shared, &cursor_tx) {
                    *shared.error.lock().expect("poisoned") = Some(format!("{e:#}"));
                    conn.close(2u32.into(), b"host pipeline error");
                }
            })?
    };

    let mut idr = IdrLimiter::default();
    let mut abr = AbrController::new(AbrConfig {
        start_kbps: opts.bitrate_kbps,
        ..AbrConfig::SPEED_1080P
    });
    let mut quit = false;
    let reason = loop {
        tokio::select! {
            m = quic::recv_msg::<v1::ControlMessage>(&mut rx, MAX_MESSAGE_LEN) => match m {
                Ok(Some(v1::ControlMessage { msg: Some(Msg::PathSwitch(_)) })) => {
                    if let (Some(e), Some(direct)) = (&relayed, prober.as_ref().and_then(|p| p.latest())) {
                        crate::migrate::host_switch(&st.node.paths, e.relay, direct);
                        println!("  ↳ Session moved off the relay to a direct path ({direct}).");
                        let _ = quic::send_msg(&mut tx, &control(Msg::PathSwitched(v1::PathSwitched {}))).await;
                    }
                }
                Ok(Some(v1::ControlMessage { msg: Some(Msg::IdrRequest(_)) })) => {
                    if idr.request(Instant::now()) {
                        shared.force_key.store(true, Ordering::Relaxed);
                    }
                }
                Ok(Some(v1::ControlMessage { msg: Some(Msg::AbrFeedback(fb)) })) => {
                    if idr.poll(Instant::now()) {
                        shared.force_key.store(true, Ordering::Relaxed);
                    }
                    if let Some(t) = abr.on_feedback(&fb) {
                        // M1 applies bitrate only; fps/scale steps land in M4.
                        shared.bitrate_kbps.store(t.bitrate_kbps, Ordering::Relaxed);
                    }
                }
                Ok(Some(v1::ControlMessage { msg: Some(Msg::End(_)) })) | Ok(None) => break "viewer disconnected",
                Ok(Some(_)) => {}
                Err(_) => break "connection lost",
            },
            _ = tokio::signal::ctrl_c() => {
                let _ = quic::send_msg(&mut tx, &control(Msg::End(v1::SessionEnd {
                    reason: v1::EndReason::HostEnded as i32,
                    detail: String::new(),
                }))).await;
                let _ = tx.finish();
                quit = true;
                break "ended by host (Ctrl+C)";
            }
            e = conn.closed() => {
                tracing::debug!("connection closed: {e}");
                break "connection closed";
            }
        }
    };
    shared.stop.store(true, Ordering::Relaxed);
    if let Some(e) = &relayed {
        st.node.paths.clear(e.relay);
    }
    drop(prober);
    input_task.abort();
    drop(cursor_tx);
    cursor_task.abort();
    let _ = tokio::task::spawn_blocking(move || pipeline.join()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    conn.close(0u32.into(), b"session ended");
    if let Some(e) = shared.error.lock().expect("poisoned").take() {
        tracing::error!("pipeline: {e}");
    }
    println!("  ○ Session ended: {reason}.");
    audit(&format!(
        "SESSION END {name}: {reason}, duration {}s",
        started.elapsed().as_secs()
    ));
    Ok(SessionOutcome::Ended { quit })
}

struct PipelineShared {
    stop: AtomicBool,
    force_key: AtomicBool,
    bitrate_kbps: AtomicU32,
    error: Mutex<Option<String>>,
}

fn make_encoder(
    choice: CodecChoice,
    cfg: EncoderConfig,
    test_pattern: bool,
) -> anyhow::Result<Box<dyn Encoder>> {
    // On Windows the test pattern lives in system memory, not a D3D11
    // texture, so it goes through the software encoder.
    let choice = if test_pattern && !cfg!(target_os = "macos") && choice == CodecChoice::Auto {
        CodecChoice::Sw
    } else {
        choice
    };
    #[cfg(target_os = "windows")]
    if choice != CodecChoice::Sw {
        match relay_codec::mediafoundation::MediaFoundationEncoder::new(cfg.clone()) {
            Ok(e) => {
                tracing::info!("encoder: Media Foundation (hardware)");
                return Ok(Box::new(e));
            }
            Err(e) if choice == CodecChoice::Hw => bail!("hardware encoder unavailable: {e}"),
            Err(e) => tracing::warn!("hardware encoder unavailable ({e}); using openh264"),
        }
    }
    #[cfg(target_os = "macos")]
    if choice != CodecChoice::Sw {
        match relay_codec::videotoolbox::VideoToolboxEncoder::new(cfg.clone()) {
            Ok(e) => {
                tracing::info!("encoder: VideoToolbox (hardware)");
                return Ok(Box::new(e));
            }
            Err(e) if choice == CodecChoice::Hw => bail!("hardware encoder unavailable: {e}"),
            Err(e) => tracing::warn!("hardware encoder unavailable ({e}); using openh264"),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    if choice == CodecChoice::Hw {
        bail!("hardware encoder not implemented on this OS yet");
    }
    tracing::info!("encoder: openh264 (software)");
    Ok(Box::new(relay_codec::openh264::OpenH264Encoder::new(cfg)?))
}

fn pipeline(
    conn: &Connection,
    opts: &Options,
    display_id: u32,
    w: u32,
    h: u32,
    shared: &PipelineShared,
    cursor_tx: &tokio::sync::mpsc::UnboundedSender<v1::CursorMessage>,
) -> anyhow::Result<()> {
    let mut cap = capturer(opts)?;
    cap.select(display_id)?;
    let mut enc = make_encoder(
        opts.codec,
        EncoderConfig::low_latency(Codec::H264, w, h, opts.fps, opts.bitrate_kbps),
        opts.test_pattern.is_some(),
    )?;

    let mut frame_no: u32 = 0;
    let mut last: Option<CapturedFrame> = None;
    let mut last_sent = Instant::now();
    let mut stats = Stats::new();
    let mut cursor = CursorSync::default();

    while !shared.stop.load(Ordering::Relaxed) {
        let frame = {
            let _s = tracing::trace_span!("capture_wait").entered();
            // Short wait so pointer-only changes are picked up promptly.
            cap.next_frame(Duration::from_millis(16))?
        };
        if let Some(c) = cap.cursor() {
            cursor.update(&c, cursor_tx);
        }
        let (frame, keepalive) = match frame {
            Some(f) => (f, false),
            // Static screen: nothing to send except a 1 fps keep-alive.
            None if last_sent.elapsed() >= KEEPALIVE => match last.clone() {
                Some(f) => (f, true),
                None => continue,
            },
            None => continue,
        };

        let kbps = shared.bitrate_kbps.load(Ordering::Relaxed);
        if kbps != enc.config().bitrate_kbps {
            enc.set_bitrate(kbps)?;
        }
        let force = shared.force_key.swap(false, Ordering::Relaxed);
        let ts = if keepalive {
            clock::now_us()
        } else {
            clock::us_of(frame.captured_at)
        };
        let t_enc = Instant::now();
        let encoded = {
            let _s = tracing::trace_span!("encode", frame_no).entered();
            enc.encode(&frame, ts, force)?
        };
        let enc_time = t_enc.elapsed();

        let max = conn
            .max_datagram_size()
            .context("peer does not accept datagrams")?
            .min(DEFAULT_MAX_DATAGRAM);
        let meta = FrameMeta {
            stream_id: 0,
            frame_no,
            is_key: encoded.is_key,
            capture_ts_us: ts,
        };
        {
            let _s = tracing::trace_span!("send", bytes = encoded.data.len()).entered();
            for dg in slice_frame(meta, &encoded.data, max)? {
                match conn.send_datagram(dg) {
                    Ok(()) => {}
                    Err(quinn::SendDatagramError::ConnectionLost(_)) => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        frame_no = frame_no.wrapping_add(1);
        last_sent = Instant::now();
        stats.record(
            encoded.data.len(),
            enc_time,
            frame.captured_at.elapsed(),
            encoded.is_key,
        );
        if !keepalive {
            last = Some(frame);
        }
        stats.maybe_log(kbps);
    }
    Ok(())
}

/// Sends cursor shape/position changes; shapes are sent in full once and
/// then referenced by hash.
#[derive(Default)]
struct CursorSync {
    known: std::collections::HashSet<u64>,
    last: Option<(bool, u64, (f32, f32))>,
}

impl CursorSync {
    fn update(
        &mut self,
        c: &relay_capture::CursorState,
        tx: &tokio::sync::mpsc::UnboundedSender<v1::CursorMessage>,
    ) {
        let hash = c.image.as_ref().map_or(0, |i| i.hash);
        if let Some(img) = &c.image
            && self.known.insert(img.hash)
        {
            let _ = tx.send(v1::CursorMessage {
                msg: Some(v1::cursor_message::Msg::Shape(v1::CursorShape {
                    hash: img.hash,
                    width: img.width,
                    height: img.height,
                    hotspot_x: img.hot_x,
                    hotspot_y: img.hot_y,
                    rgba: img.rgba.clone().into(),
                    scale_milli: img.scale_milli,
                })),
            });
        }
        let now = (c.visible, hash, (c.x, c.y));
        if self.last != Some(now) {
            self.last = Some(now);
            let _ = tx.send(v1::CursorMessage {
                msg: Some(v1::cursor_message::Msg::Position(v1::CursorPosition {
                    x: c.x,
                    y: c.y,
                    visible: c.visible,
                    shape_hash: hash,
                })),
            });
        }
    }
}

struct Stats {
    since: Instant,
    frames: u32,
    keys: u32,
    bytes: usize,
    enc: Duration,
    glass_to_send: Duration,
}

impl Stats {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            keys: 0,
            bytes: 0,
            enc: Duration::ZERO,
            glass_to_send: Duration::ZERO,
        }
    }

    fn record(&mut self, bytes: usize, enc: Duration, capture_to_sent: Duration, key: bool) {
        self.frames += 1;
        self.keys += key as u32;
        self.bytes += bytes;
        self.enc += enc;
        self.glass_to_send += capture_to_sent;
    }

    fn maybe_log(&mut self, target_kbps: u32) {
        let el = self.since.elapsed();
        if el < Duration::from_secs(5) || self.frames == 0 {
            return;
        }
        let n = self.frames;
        tracing::info!(
            "host: {:.0} fps, {:.1} Mbit/s (target {:.1}), encode {:.1} ms, capture→sent {:.1} ms, {} keyframes",
            f64::from(n) / el.as_secs_f64(),
            self.bytes as f64 * 8.0 / el.as_secs_f64() / 1e6,
            f64::from(target_kbps) / 1000.0,
            self.enc.as_secs_f64() * 1000.0 / f64::from(n),
            self.glass_to_send.as_secs_f64() * 1000.0 / f64::from(n),
            self.keys
        );
        *self = Self::new();
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "relay-host".into())
}

fn audit(line: &str) {
    // FR-39 stand-in: the durable, tamper-resistant audit log lands in M7.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    eprintln!("  [audit {ts}] {line}");
}

/// Best-effort list of this machine's LAN addresses for the banner.
pub fn local_ips() -> Vec<IpAddr> {
    // Connecting a UDP socket sends nothing; it only picks a route.
    let mut out = Vec::new();
    if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0")
        && s.connect("192.0.2.1:9").is_ok()
        && let Ok(a) = s.local_addr()
    {
        out.push(a.ip());
    }
    if out.is_empty() {
        out.push(IpAddr::from([127, 0, 0, 1]));
    }
    out
}

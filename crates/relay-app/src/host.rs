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
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use relay_crypto::pake::{HostPassword, PasswordKind};
use relay_crypto::password::OneTimePassword;
use relay_net::quic::{self, Connection};
use relay_proto::v1::{self, control_message::Msg};
use relay_proto::{MAX_MESSAGE_LEN, PROTOCOL_VERSION};
use relay_session::auth_limit::AuthLimiter;

use crate::{CodecChoice, rt};

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
    /// Sync clipboard text with the Controller (FR-17).
    pub clipboard: bool,
    /// Let the Controller browse and transfer files (FR-26).
    pub files: bool,
    pub server: Option<crate::rdv::Server>,
    /// Run as the unattended agent (FR-32): system identity, permanent
    /// password, no Accept prompt.
    pub unattended: bool,
}

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

    crate::banner::run_with_banner(move |banner| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(serve(opts, display, displays, banner))
    })
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
    /// Every display, for the monitor picker (FR-20).
    displays: Vec<relay_capture::DisplayInfo>,
    me: Arc<crate::identity::Me>,
    /// Unattended configuration (agent mode only).
    unattended: Option<crate::unattended::Config>,
    banner: crate::banner::Banner,
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

async fn serve(
    opts: Options,
    display: relay_capture::DisplayInfo,
    displays: Vec<relay_capture::DisplayInfo>,
    banner: crate::banner::Banner,
) -> anyhow::Result<()> {
    let (me, unattended_cfg, bind) = if opts.unattended {
        let cfg = crate::unattended::load()?.context(
            "unattended access is not enabled (run `relay unattended setup` as administrator)",
        )?;
        let key = relay_crypto::keystore::load_or_create_device_key(
            crate::unattended::keystore().as_ref(),
        )?;
        // FR-38: the moment unattended access is disabled, stop serving.
        tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if !crate::unattended::enabled() {
                    crate::logs::audit("unattended access disabled; agent exiting");
                    std::process::exit(0);
                }
            }
        });
        // Several agents may come and go (login window, user sessions):
        // any free port; they are reached by ID.
        (
            crate::identity::from_key(key)?,
            Some(cfg),
            SocketAddr::from(([0, 0, 0, 0], 0)),
        )
    } else {
        (crate::identity::load()?, None, opts.bind)
    };
    let me = Arc::new(me);
    let (node, control) = quic::node(bind, &me.tls, Some(&[quic::ALPN]))?;
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
    if opts.unattended {
        println!("  Mode          unattended (permanent password)");
    } else {
        println!("  Password      {}", creds.otp.as_str());
    }
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
        displays,
        me: me.clone(),
        unattended: unattended_cfg,
        banner,
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
    // The agent only takes the permanent password; an attended host only
    // its one-time password (FR-31, FR-2).
    let result = auth::host_login(conn, &mut tx, &mut rx, me, &peer, |kind| {
        match (kind, &st.unattended) {
            (v1::PasswordKind::Permanent, Some(cfg)) => Some(&cfg.verifier),
            (v1::PasswordKind::OneTime, None) => Some(&creds.verifier),
            _ => None,
        }
    })
    .await;
    let unattended = match result {
        Ok(auth::HostAuth::Ok(kind)) => {
            let mut l = st.limiters.lock().expect("poisoned");
            l.ip.record_success(&ip);
            l.key.record_success(&peer.key);
            kind == v1::PasswordKind::Permanent
        }
        Ok(auth::HostAuth::NotAccepted(kind)) => {
            audit(&format!(
                "REFUSED {name}: {kind:?} password not accepted here"
            ));
            return reject(&mut tx, v1::AuthResult::BadPassword, 0).await;
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
    };
    // Allow-list (FR-31), checked after the password so it reveals nothing.
    if unattended && st.unattended.as_ref().is_some_and(|c| !c.allows(peer.id)) {
        audit(&format!("REFUSED {name}: not on the allow-list"));
        return reject(&mut tx, v1::AuthResult::NotWhitelisted, 0).await;
    }

    // One session (and one consent dialog) at a time.
    let Ok(_streaming) = st.streaming.try_lock() else {
        println!("  ○ {name} tried to connect while another session is active.");
        return reject(&mut tx, v1::AuthResult::Declined, 0).await;
    };

    // FR-8: an attended session needs the local user's explicit yes.
    // FR-33: unattended sessions skip it (the banner still shows).
    if !unattended && !crate::consent::ask(&name).await {
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
    audit(&format!(
        "SESSION START {name} ({})",
        if unattended { "unattended" } else { "attended" }
    ));
    println!(
        "  ● {} by {name}. Press Ctrl+C to end the session.",
        if opts.view_only {
            "Being viewed"
        } else {
            "Being viewed and controlled"
        }
    );
    // FR-9 / FR-33: the banner shows whenever someone is at the Host,
    // attended or not. The Controller has no way to hide it.
    let banner_shown = !unattended || relay_service::console_user_present();
    if banner_shown {
        st.banner.show(&name);
    }

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
            clipboard: opts.clipboard && !opts.view_only,
            file_transfer: opts.files && !opts.view_only,
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
    // Input follows the selected display (FR-21).
    let (target_tx, target_rx) = tokio::sync::watch::channel(target_of(display));
    let input_task = tokio::spawn(crate::host_input::serve(
        conn.clone(),
        target_rx,
        !opts.view_only,
        opts.clipboard && !opts.view_only,
        if peer.name.is_empty() {
            "viewer".into()
        } else {
            peer.name.clone()
        },
    ));
    // File transfer (FR-26–28): one task per request stream.
    let files_task = (opts.files && !opts.view_only)
        .then(|| tokio::spawn(crate::files::accept_loop(conn.clone())));
    // Chat (FR-24): lines typed in this terminal go to the viewer.
    let chat = crate::chat::ChatLink::start(conn);
    println!("  Type a message and press Enter to chat with the viewer.");
    let chat_task = {
        let mut lines = crate::stdin::lines();
        tokio::spawn(async move {
            while let Ok(l) = lines.recv().await {
                let sent = chat.send(&l);
                if !sent.is_empty() {
                    println!(
                        "  💬 you {}: {sent}",
                        crate::chat::clock(crate::chat::now_ms())
                    );
                }
            }
        })
    };
    // FR-20: every display, with resolution, scale and arrangement.
    quic::send_msg(
        &mut tx,
        &control(Msg::Monitors(v1::MonitorList {
            displays: st.displays.iter().map(proto_display).collect(),
        })),
    )
    .await?;
    quic::send_msg(
        &mut tx,
        &control(Msg::SelectMonitor(v1::SelectMonitor {
            display_id: display.id,
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

    let shared = Arc::new(crate::sender::SenderControl::new(
        opts.bitrate_kbps,
        opts.fps,
    ));
    shared.display_id.store(display.id, Ordering::Relaxed);
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
                let params = crate::sender::SenderParams {
                    codec: opts.codec,
                    max_fps: opts.fps,
                    cpu_source: opts.test_pattern.is_some() && !cfg!(target_os = "macos"),
                };
                let res = capturer(&opts).and_then(|cap| {
                    crate::sender::run(&conn, cap, display_id, params, &shared, Some(&cursor_tx))
                });
                if let Err(e) = res {
                    *shared.error.lock().expect("poisoned") = Some(format!("{e:#}"));
                    conn.close(2u32.into(), b"host pipeline error");
                }
            })?
    };

    let mut control_state = crate::sender::ControlState::new(opts.bitrate_kbps);
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
                    control_state.on_idr_request(&shared);
                }
                Ok(Some(v1::ControlMessage { msg: Some(Msg::AbrFeedback(fb)) })) => {
                    control_state.on_feedback(&fb, &shared);
                }
                // FR-34: remote reboot, unattended sessions with control only.
                Ok(Some(v1::ControlMessage { msg: Some(Msg::Power(p)) })) => {
                    let safe = p.action == v1::PowerAction::RebootSafeMode as i32;
                    if unattended && !opts.view_only {
                        audit(&format!("REBOOT{} requested by {name}", if safe { " (safe mode)" } else { "" }));
                        let _ = quic::send_msg(&mut tx, &control(Msg::End(v1::SessionEnd {
                            reason: v1::EndReason::Reboot as i32,
                            detail: String::new(),
                        }))).await;
                        let _ = tx.finish();
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        if let Err(e) = relay_service::reboot(safe) {
                            tracing::error!("reboot failed: {e}");
                        }
                        break "host rebooting";
                    }
                }
                // FR-14: Ctrl+Alt+Del, performed by the Windows service.
                Ok(Some(v1::ControlMessage { msg: Some(Msg::SendSas(_)) })) => {
                    if unattended && !opts.view_only {
                        match relay_service::send_sas() {
                            Ok(()) => audit(&format!("CTRL+ALT+DEL sent by {name}")),
                            Err(e) => tracing::warn!("Ctrl+Alt+Del: {e}"),
                        }
                    }
                }
                Ok(Some(v1::ControlMessage { msg: Some(Msg::SelectMonitor(sel)) })) => {
                    // FR-21: switch the captured display; only it is encoded.
                    if let Some(d) = st.displays.iter().find(|d| d.id == sel.display_id) {
                        shared.display_id.store(d.id, Ordering::Relaxed);
                        let _ = target_tx.send(target_of(d));
                        println!("  Sharing display {} ({}x{}).", d.id, d.width, d.height);
                        let _ = quic::send_msg(&mut tx, &control(Msg::SelectMonitor(v1::SelectMonitor { display_id: d.id }))).await;
                    }
                }
                Ok(Some(v1::ControlMessage { msg: Some(Msg::SetQuality(q)) })) => {
                    let mode = v1::QualityMode::try_from(q.mode).unwrap_or(v1::QualityMode::Speed);
                    control_state.set_mode(mode, &shared);
                    println!("  Quality mode: {mode:?}");
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
            // FR-10: the Host user clicked "End session" on the banner.
            _ = crate::banner::end_requested().notified() => {
                let _ = quic::send_msg(&mut tx, &control(Msg::End(v1::SessionEnd {
                    reason: v1::EndReason::HostEnded as i32,
                    detail: String::new(),
                }))).await;
                let _ = tx.finish();
                break "ended by host";
            }
            e = conn.closed() => {
                tracing::debug!("connection closed: {e}");
                break "connection closed";
            }
        }
    };
    if banner_shown {
        st.banner.hide();
    }
    shared.stop.store(true, Ordering::Relaxed);
    if let Some(e) = &relayed {
        st.node.paths.clear(e.relay);
    }
    drop(prober);
    input_task.abort();
    chat_task.abort();
    if let Some(t) = files_task {
        t.abort();
    }
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

fn target_of(d: &relay_capture::DisplayInfo) -> relay_input::DisplayTarget {
    relay_input::DisplayTarget {
        id: d.id,
        x: d.x,
        y: d.y,
        width: d.width,
        height: d.height,
    }
}

fn proto_display(d: &relay_capture::DisplayInfo) -> v1::Display {
    v1::Display {
        id: d.id,
        name: d.name.clone(),
        x: d.x,
        y: d.y,
        width: d.width,
        height: d.height,
        scale: d.scale,
        primary: d.primary,
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
    crate::logs::audit(line);
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

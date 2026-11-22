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

async fn serve(opts: Options, display: relay_capture::DisplayInfo) -> anyhow::Result<()> {
    let (ep, fp) = quic::server(opts.bind)?;
    let mut code = OneTimePassword::generate()?;
    let mut limiter: AuthLimiter<IpAddr> = AuthLimiter::default();

    println!();
    println!("  Relay host ready  (M1 direct mode)");
    println!("  Listening on  {}", ep.local_addr()?);
    let ips = if opts.bind.ip().is_unspecified() {
        local_ips()
    } else {
        vec![opts.bind.ip()]
    };
    for ip in ips {
        println!(
            "    relay view {ip}:{} --code {}",
            opts.bind.port(),
            code.as_str()
        );
    }
    println!("  Session code  {}", code.as_str());
    println!("  Certificate   {fp}");
    println!(
        "  Sharing       display {} ({}x{})",
        display.id, display.width, display.height
    );
    println!("  Press Ctrl+C to quit.\n");

    loop {
        let incoming = tokio::select! {
            i = ep.accept() => match i { Some(i) => i, None => break },
            _ = tokio::signal::ctrl_c() => break,
        };
        let remote = incoming.remote_address();
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("handshake from {remote} failed: {e}");
                continue;
            }
        };
        match session(&conn, &opts, &display, &code, &mut limiter).await {
            Ok(SessionOutcome::Rejected) => {}
            Ok(SessionOutcome::Ended { quit }) => {
                code = OneTimePassword::generate()?; // FR-2
                println!("  New session code: {}\n", code.as_str());
                if quit {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!("session with {remote} ended with error: {e:#}");
                code = OneTimePassword::generate()?;
                println!("  New session code: {}\n", code.as_str());
            }
        }
    }
    ep.close(0u32.into(), b"host shutting down");
    ep.wait_idle().await;
    Ok(())
}

enum SessionOutcome {
    Rejected,
    Ended { quit: bool },
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn control(msg: Msg) -> v1::ControlMessage {
    v1::ControlMessage { msg: Some(msg) }
}

async fn session(
    conn: &Connection,
    opts: &Options,
    display: &relay_capture::DisplayInfo,
    code: &OneTimePassword,
    limiter: &mut AuthLimiter<IpAddr>,
) -> anyhow::Result<SessionOutcome> {
    let remote = conn.remote_address();
    let ip = remote.ip();
    let (mut tx, mut rx) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
        .await
        .context("no control stream")??;

    let hello = match quic::recv_msg::<v1::ControlMessage>(&mut rx, MAX_MESSAGE_LEN).await? {
        Some(v1::ControlMessage {
            msg: Some(Msg::Hello(h)),
        }) => h,
        _ => bail!("expected Hello"),
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        bail!("protocol version {} unsupported", hello.protocol_version);
    }
    let auth = match quic::recv_msg::<v1::ControlMessage>(&mut rx, 4096).await? {
        Some(v1::ControlMessage {
            msg: Some(Msg::Auth(a)),
        }) => a,
        _ => bail!("expected Auth"),
    };
    let name = if hello.device_name.is_empty() {
        remote.to_string()
    } else {
        format!("{} ({remote})", hello.device_name)
    };

    // FR-35: per-IP backoff.
    let now = Instant::now();
    let outcome = if let Err(wait) = limiter.check(&ip, now) {
        audit(&format!(
            "REJECTED {name}: locked out for {}s",
            wait.as_secs()
        ));
        v1::AuthOutcome {
            result: v1::AuthResult::LockedOut as i32,
            retry_after_ms: wait.as_millis() as u32,
        }
    } else if ct_eq(&auth.payload, code.as_str().as_bytes()) {
        limiter.record_success(&ip);
        v1::AuthOutcome {
            result: v1::AuthResult::Ok as i32,
            retry_after_ms: 0,
        }
    } else {
        let lock = limiter.record_failure(&ip, now);
        audit(&format!("FAILED login from {name}"));
        v1::AuthOutcome {
            result: if lock.is_some() {
                v1::AuthResult::LockedOut
            } else {
                v1::AuthResult::BadPassword
            } as i32,
            retry_after_ms: lock.map_or(0, |d| d.as_millis() as u32),
        }
    };
    let ok = outcome.result == v1::AuthResult::Ok as i32;
    quic::send_msg(&mut tx, &control(Msg::AuthOutcome(outcome))).await?;
    if !ok {
        let _ = tx.finish();
        tokio::time::sleep(Duration::from_millis(200)).await;
        conn.close(1u32.into(), b"auth failed");
        return Ok(SessionOutcome::Rejected);
    }

    let started = Instant::now();
    audit(&format!("SESSION START {name} (attended, direct)"));
    println!("  ● Being viewed by {name}. Press Ctrl+C to end the session.");

    let (w, h) = (display.width & !1, display.height & !1);
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
        std::thread::Builder::new()
            .name("relay-encode".into())
            .spawn(move || {
                rt::set_realtime();
                if let Err(e) = pipeline(&conn, &opts, display_id, w, h, &shared) {
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

    while !shared.stop.load(Ordering::Relaxed) {
        let frame = {
            let _s = tracing::trace_span!("capture_wait").entered();
            cap.next_frame(Duration::from_millis(100))?
        };
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

fn audit(line: &str) {
    // FR-39 stand-in: the durable, tamper-resistant audit log lands in M7.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    eprintln!("  [audit {ts}] {line}");
}

/// Best-effort list of this machine's LAN addresses for the banner.
fn local_ips() -> Vec<IpAddr> {
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

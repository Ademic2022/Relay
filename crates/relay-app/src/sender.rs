//! The video sender: capture → encode → FEC → QUIC datagrams, and the
//! Host's reaction to Controller feedback (§6, M4).
//!
//! One real-time thread runs [`run`] (§6 rule 9). The tokio side feeds it
//! decisions through [`SenderControl`] atomics: bitrate, frame rate and
//! resolution scale from the ABR controller (§6 rule 7), keyframe requests
//! (rate-limited, rule 6), and the measured packet loss that sizes the
//! FEC recovery shards.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use relay_capture::{CapturedFrame, ScreenCapturer};
use relay_codec::{Codec, Encoder, EncoderConfig};
use relay_net::quic::Connection;
use relay_net::video::{DEFAULT_MAX_DATAGRAM, FrameMeta, slice_for_loss};
use relay_proto::v1;
use relay_session::abr::{AbrConfig, AbrController};
use relay_session::idr::IdrLimiter;

use crate::{CodecChoice, clock};

/// Keep-alive cadence when the screen is static (§6 rule 4).
const KEEPALIVE: Duration = Duration::from_secs(1);

/// Decisions shared between the control task and the sender thread.
#[derive(Debug)]
pub struct SenderControl {
    pub stop: AtomicBool,
    pub force_key: AtomicBool,
    pub bitrate_kbps: AtomicU32,
    pub fps: AtomicU32,
    /// Resolution scale × 1000 (1000 = native).
    pub scale_milli: AtomicU32,
    /// Packet loss reported by the Controller, per mille.
    pub loss_permille: AtomicU32,
    pub error: Mutex<Option<String>>,
}

impl SenderControl {
    pub fn new(bitrate_kbps: u32, fps: u32) -> Self {
        Self {
            stop: AtomicBool::new(false),
            force_key: AtomicBool::new(true),
            bitrate_kbps: AtomicU32::new(bitrate_kbps),
            fps: AtomicU32::new(fps),
            scale_milli: AtomicU32::new(1000),
            loss_permille: AtomicU32::new(0),
            error: Mutex::new(None),
        }
    }
}

/// Settings per quality mode (FR-19, §6 rule 10).
pub fn abr_config(mode: v1::QualityMode, start_kbps: u32) -> AbrConfig {
    let base = match mode {
        // Latency first: the 1080p60 Speed envelope (§9).
        v1::QualityMode::Speed => AbrConfig::SPEED_1080P,
        v1::QualityMode::Balanced => AbrConfig {
            floor_kbps: 4_000,
            max_kbps: 25_000,
            start_kbps: 12_000,
            min_kbps: 400,
        },
        // Design work: keep resolution and bitrate high as long as the
        // link allows (4:4:4 / sharpening are follow-ups).
        v1::QualityMode::Quality => AbrConfig {
            floor_kbps: 6_000,
            max_kbps: 40_000,
            start_kbps: 20_000,
            min_kbps: 600,
        },
    };
    AbrConfig {
        start_kbps: start_kbps.clamp(base.min_kbps, base.max_kbps),
        ..base
    }
}

/// The Host's control-stream reactions (feedback, keyframe requests,
/// quality mode). Lives on the tokio side.
#[derive(Debug)]
pub struct ControlState {
    idr: IdrLimiter,
    abr: AbrController,
    pub mode: v1::QualityMode,
    /// The last report showed queueing delay: hold keyframes back.
    congested: bool,
}

impl ControlState {
    pub fn new(start_kbps: u32) -> Self {
        let mode = v1::QualityMode::Speed;
        Self {
            idr: IdrLimiter::default(),
            abr: AbrController::new(abr_config(mode, start_kbps)),
            mode,
            congested: false,
        }
    }

    /// A keyframe request. Under congestion it waits until the queue
    /// clears (bounded), so recovery does not become a keyframe storm.
    pub fn on_idr_request(&mut self, ctl: &SenderControl) {
        let now = Instant::now();
        self.idr.defer(now);
        if self.idr.poll_congested(now, self.congested) {
            ctl.force_key.store(true, Ordering::Relaxed);
        }
    }

    pub fn on_feedback(&mut self, fb: &v1::AbrFeedback, ctl: &SenderControl) {
        self.congested = fb.delay_trend_us > relay_session::abr::PROBE_MAX_DELAY_US * 2;
        if self.idr.poll_congested(Instant::now(), self.congested) {
            ctl.force_key.store(true, Ordering::Relaxed);
        }
        ctl.loss_permille.store(fb.loss_permille, Ordering::Relaxed);
        if let Some(t) = self.abr.on_feedback(fb) {
            tracing::debug!(
                "ABR → {} kbps, {} fps, scale {:.2}",
                t.bitrate_kbps,
                t.fps,
                t.scale
            );
            ctl.bitrate_kbps.store(t.bitrate_kbps, Ordering::Relaxed);
            ctl.fps.store(t.fps, Ordering::Relaxed);
            ctl.scale_milli
                .store((t.scale * 1000.0).round() as u32, Ordering::Relaxed);
        }
    }

    pub fn set_mode(&mut self, mode: v1::QualityMode, ctl: &SenderControl) {
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        let cfg = abr_config(mode, ctl.bitrate_kbps.load(Ordering::Relaxed));
        self.abr = AbrController::new(AbrConfig {
            start_kbps: cfg
                .start_kbps
                .max(ctl.bitrate_kbps.load(Ordering::Relaxed))
                .min(cfg.max_kbps),
            ..cfg
        });
        let t = self.abr.target();
        ctl.bitrate_kbps.store(t.bitrate_kbps, Ordering::Relaxed);
        ctl.fps.store(t.fps, Ordering::Relaxed);
        ctl.scale_milli
            .store((t.scale * 1000.0).round() as u32, Ordering::Relaxed);
        tracing::info!("quality mode {mode:?}");
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SenderParams {
    pub codec: CodecChoice,
    pub max_fps: u32,
    /// The source is in system memory (test pattern on Windows), so the
    /// hardware encoder cannot take it.
    pub cpu_source: bool,
}

/// Run until `ctl.stop` or the connection ends.
pub fn run(
    conn: &Connection,
    mut cap: Box<dyn ScreenCapturer>,
    display_id: u32,
    params: SenderParams,
    ctl: &SenderControl,
    cursor_tx: Option<&tokio::sync::mpsc::UnboundedSender<v1::CursorMessage>>,
) -> anyhow::Result<()> {
    cap.select(display_id)?;
    let native = cap
        .displays()?
        .into_iter()
        .find(|d| d.id == display_id)
        .map(|d| (d.width & !1, d.height & !1))
        .context("display vanished")?;
    let mut size = native;
    let mut scale_milli = 1000;
    let new_encoder = |(w, h): (u32, u32), kbps: u32| {
        make_encoder(
            params.codec,
            EncoderConfig::low_latency(Codec::H264, w, h, params.max_fps, kbps),
            params.cpu_source,
        )
    };
    let mut enc = new_encoder(size, ctl.bitrate_kbps.load(Ordering::Relaxed))?;

    let mut frame_no: u32 = 0;
    let mut last: Option<CapturedFrame> = None;
    let mut last_sent = Instant::now();
    let mut last_encoded: Option<Instant> = None;
    let mut stats = Stats::new();
    let mut cursor = CursorSync::default();

    while !ctl.stop.load(Ordering::Relaxed) {
        let frame = {
            let _s = tracing::trace_span!("capture_wait").entered();
            // Short wait so pointer-only changes are picked up promptly.
            cap.next_frame(Duration::from_millis(16))?
        };
        if let (Some(c), Some(tx)) = (cap.cursor(), cursor_tx) {
            cursor.update(&c, tx);
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

        // ABR step 3: resolution (§6 rule 7). Needs a new encoder and a
        // keyframe; skipped if the capturer cannot scale.
        let want_scale = ctl.scale_milli.load(Ordering::Relaxed).clamp(250, 1000);
        if want_scale != scale_milli {
            match cap.set_scale(want_scale as f32 / 1000.0) {
                Ok(new) => {
                    scale_milli = want_scale;
                    size = new;
                    enc = new_encoder(size, ctl.bitrate_kbps.load(Ordering::Relaxed))?;
                    last = None;
                    tracing::info!(
                        "resolution {}x{} (scale {:.2})",
                        size.0,
                        size.1,
                        f64::from(want_scale) / 1000.0
                    );
                    continue; // next frame arrives at the new size
                }
                Err(e) => {
                    tracing::debug!("capturer cannot scale: {e}");
                    scale_milli = want_scale; // do not retry every frame
                }
            }
        }
        if (frame.width, frame.height) != size {
            continue; // stale frame from before a resize
        }

        // ABR step 2: frame rate. Drop captures that come too soon.
        let force = ctl.force_key.load(Ordering::Relaxed);
        let fps = ctl
            .fps
            .load(Ordering::Relaxed)
            .clamp(1, params.max_fps.max(1));
        let min_gap = Duration::from_secs(1) / fps;
        if !keepalive
            && !force
            && last_encoded.is_some_and(|t| t.elapsed() + Duration::from_millis(2) < min_gap)
        {
            last = Some(frame);
            continue;
        }

        // Newest wins at the source (§6 rule 3): while earlier frames still
        // wait in QUIC's datagram buffer (the path is slower than our
        // rate), do not encode another one. Skipping the encode, not
        // dropping encoded frames, keeps the reference chain intact.
        let kbps = ctl.bitrate_kbps.load(Ordering::Relaxed);
        let backlog =
            relay_net::quic::DATAGRAM_SEND_BUFFER.saturating_sub(conn.datagram_send_buffer_space());
        let frame_bytes = (kbps as usize * 1000 / 8 / fps as usize).max(16 * 1024);
        if !keepalive && backlog > 2 * frame_bytes {
            stats.backlogged += 1;
            last = Some(frame);
            continue;
        }

        // ABR step 1: bitrate. The ABR target is the wire rate; FEC parity
        // comes out of it, so recovery can never push past what the path
        // was measured to carry.
        let loss = ctl.loss_permille.load(Ordering::Relaxed) as f32 / 1000.0;
        let enc_kbps = ((kbps as f32 * relay_net::video::fec_overhead(loss)) as u32).max(50);
        // Avoid retuning the encoder for sub-5% wobble.
        if enc_kbps.abs_diff(enc.config().bitrate_kbps) * 20 > enc.config().bitrate_kbps {
            enc.set_bitrate(enc_kbps)?;
        }
        ctl.force_key.store(false, Ordering::Relaxed);
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
        last_encoded = Some(t_enc);
        if encoded.data.is_empty() {
            // Rate controller skipped it; a keyframe request stays pending.
            if force {
                ctl.force_key.store(true, Ordering::Relaxed);
            }
            continue;
        }

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
        let datagrams = slice_for_loss(meta, &encoded.data, max, loss)?;
        let wire: usize = datagrams.iter().map(|d| d.len()).sum();
        {
            let _s = tracing::trace_span!("send", bytes = wire).entered();
            for dg in datagrams {
                match conn.send_datagram(dg) {
                    Ok(()) => {}
                    Err(quinn::SendDatagramError::ConnectionLost(_)) => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        frame_no = frame_no.wrapping_add(1);
        last_sent = Instant::now();
        stats.record(wire, enc_time, frame.captured_at.elapsed(), encoded.is_key);
        if !keepalive {
            last = Some(frame);
        }
        stats.maybe_log(kbps, fps, scale_milli);
    }
    Ok(())
}

pub fn make_encoder(
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
    backlogged: u32,
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
            backlogged: 0,
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

    fn maybe_log(&mut self, target_kbps: u32, fps: u32, scale_milli: u32) {
        let el = self.since.elapsed();
        if el < Duration::from_secs(5) || self.frames == 0 {
            return;
        }
        let n = self.frames;
        tracing::info!(
            "host: {:.0} fps (target {fps}), {:.1} Mbit/s on the wire incl. FEC (target {:.1}), scale {:.2}, encode {:.1} ms, capture→sent {:.1} ms, {} keyframes, {} skipped for backlog",
            f64::from(n) / el.as_secs_f64(),
            self.bytes as f64 * 8.0 / el.as_secs_f64() / 1e6,
            f64::from(target_kbps) / 1000.0,
            f64::from(scale_milli) / 1000.0,
            self.enc.as_secs_f64() * 1000.0 / f64::from(n),
            self.glass_to_send.as_secs_f64() * 1000.0 / f64::from(n),
            self.keys,
            self.backlogged
        );
        *self = Self::new();
    }
}

//! M4 acceptance under network emulation (§11, §12 M4):
//! - 2% loss, 50 ms RTT: no freeze longer than 300 ms;
//! - 1 Mbit/s bottleneck: the Host steps down gracefully and latency stays
//!   bounded.
//!
//! The real sender (software encoder, noisy test pattern), the real QUIC
//! transport, FEC, reassembly, decoder and feedback run through
//! [`relay_net::netem`]. Only the window is missing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use relay_net::netem::{self, Link};
use relay_net::quic;
use relay_net::tls::Expect;
use relay_net::video::Reassembler;
use relay_proto::v1::{self, control_message::Msg};

use crate::sender::{ControlState, SenderControl, SenderParams};
use crate::{CodecChoice, auth::control, clock};

const W: u32 = 640;
const H: u32 = 360;
const FPS: u32 = 30;

struct Outcome {
    /// (decode instant, glass-to-decode latency)
    decoded: Vec<(Instant, Duration)>,
    /// Decoded frame sizes, in order.
    sizes: Vec<(u32, u32)>,
    recovered: u64,
    final_kbps: u32,
    final_fps: u32,
    final_scale_milli: u32,
    netem: netem::Stats,
    started: Instant,
}

impl Outcome {
    fn after(&self, warmup: Duration) -> impl Iterator<Item = &(Instant, Duration)> {
        let from = self.started + warmup;
        self.decoded.iter().filter(move |(t, _)| *t >= from)
    }

    fn max_gap(&self, warmup: Duration) -> Duration {
        let ts: Vec<Instant> = self.after(warmup).map(|(t, _)| *t).collect();
        ts.windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or(Duration::MAX)
    }

    fn fps(&self, warmup: Duration, total: Duration) -> f64 {
        self.after(warmup).count() as f64 / (total - warmup).as_secs_f64()
    }

    fn latency_p95(&self, warmup: Duration) -> Duration {
        let mut v: Vec<Duration> = self.after(warmup).map(|(_, l)| *l).collect();
        v.sort();
        v.get(v.len() * 95 / 100).copied().unwrap_or(Duration::MAX)
    }
}

/// The scenarios measure timing; running them in parallel on one machine
/// turns CPU contention into apparent network delay. One at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn run(up: Link, down: Link, start_kbps: u32, secs: u64) -> Outcome {
    run_with(up, down, start_kbps, secs, None).await
}

/// `switch`: after this long, capture display 2 instead (multi-monitor).
async fn run_with(
    up: Link,
    down: Link,
    start_kbps: u32,
    secs: u64,
    switch: Option<Duration>,
) -> Outcome {
    let _serial = SERIAL.lock().await;
    let lo: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (host_id, ctl_id) = (crate::identity::ephemeral(), crate::identity::ephemeral());
    let (host, _) = quic::node(lo, &host_id.tls, Some(&[quic::ALPN])).unwrap();
    let host_addr = host.endpoint.local_addr().unwrap();
    let (proxy, netem_stats) = netem::start(lo, host_addr, up, down).await.unwrap();
    let ctl_shared = Arc::new(SenderControl::new(start_kbps, FPS));

    // Host: sender thread + feedback handling, as in a real session.
    let sc = ctl_shared.clone();
    let host_task = tokio::spawn(async move {
        let conn = host.endpoint.accept().await.unwrap().await.unwrap();
        let (_tx, mut rx) = conn.accept_bi().await.unwrap();
        let sender = {
            let (conn, sc) = (conn.clone(), sc.clone());
            std::thread::spawn(move || {
                let mut cap =
                    relay_capture::pattern::PatternCapturer::new(W, H, FPS).with_display(480, 270);
                cap.noise = true;
                let params = SenderParams {
                    codec: CodecChoice::Sw,
                    max_fps: FPS,
                    cpu_source: true,
                };
                let r = crate::sender::run(&conn, Box::new(cap.cpu_frames()), 1, params, &sc, None);
                if let Err(e) = &r {
                    eprintln!("sender failed: {e:#}");
                }
                r
            })
        };
        let mut state = ControlState::new(start_kbps);
        while let Ok(Some(m)) = quic::recv_msg::<v1::ControlMessage>(&mut rx, 1 << 16).await {
            match m.msg {
                Some(Msg::AbrFeedback(fb)) => {
                    state.on_feedback(&fb, &sc);
                    if std::env::var_os("NETEM_DEBUG").is_some() {
                        eprintln!(
                            "fb recv={} loss={} delay={}ms → {} kbps {} fps {}",
                            fb.receive_kbps,
                            fb.loss_permille,
                            fb.delay_trend_us / 1000,
                            sc.bitrate_kbps.load(Ordering::Relaxed),
                            sc.fps.load(Ordering::Relaxed),
                            sc.scale_milli.load(Ordering::Relaxed)
                        );
                    }
                }
                Some(Msg::IdrRequest(_)) => state.on_idr_request(&sc),
                _ => {}
            }
        }
        sc.stop.store(true, Ordering::Relaxed);
        let _ = sender.join();
        host
    });

    // Controller: headless receive → reassemble → decode, 100 ms feedback.
    let (ctl, _) = quic::node(lo, &ctl_id.tls, None).unwrap();
    let (cfg, _) = quic::client_config(
        &ctl_id.tls,
        Expect::Key(host_id.public_key()),
        quic::ALPN,
        Duration::from_secs(2),
    )
    .unwrap();
    let conn = quic::connect(&ctl.endpoint, cfg, proxy).await.unwrap();
    let (mut tx, _rx) = conn.open_bi().await.unwrap();
    let (dec_tx, dec_rx) = std::sync::mpsc::sync_channel::<relay_net::video::CompleteFrame>(8);
    let (idr_tx, mut idr_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let decoder = std::thread::spawn(move || {
        let mut dec = relay_codec::openh264::OpenH264Decoder::new().unwrap();
        let mut out = Vec::new();
        let mut sizes = Vec::new();
        let mut need_key = false;
        while let Ok(f) = dec_rx.recv() {
            if need_key && !f.is_key {
                continue;
            }
            if f.gap_before {
                need_key = true;
                let _ = idr_tx.send(());
                if !f.is_key {
                    continue;
                }
            }
            match relay_codec::Decoder::decode(&mut dec, &f.data, f.capture_ts_us) {
                Ok(Some(d)) => {
                    need_key = false;
                    let lat =
                        Duration::from_micros(clock::now_us().saturating_sub(f.capture_ts_us));
                    out.push((Instant::now(), lat));
                    sizes.push((d.width, d.height));
                }
                Ok(None) => {}
                Err(_) => {
                    need_key = true;
                    let _ = idr_tx.send(());
                }
            }
        }
        (out, sizes)
    });

    let started = Instant::now();
    let mut reasm = Reassembler::new(Duration::from_millis(500));
    let mut fb = crate::feedback::Feedback::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let switch_at = switch.map(|d| tokio::time::Instant::now() + d);
    loop {
        if let Some(at) = switch_at
            && tokio::time::Instant::now() >= at
            && ctl_shared.display_id.load(Ordering::Relaxed) != 2
        {
            ctl_shared.display_id.store(2, Ordering::Relaxed);
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            dg = conn.read_datagram() => {
                let Ok(dg) = dg else { break };
                fb.bytes += dg.len();
                fb.on_datagram(&dg);
                if let Some(f) = reasm.push(dg, Instant::now())
                    && dec_tx.try_send(f).is_err()
                {
                    let _ = quic::send_msg(&mut tx, &control(Msg::IdrRequest(v1::IdrRequest::default()))).await;
                }
            }
            Some(()) = idr_rx.recv() => {
                let _ = quic::send_msg(&mut tx, &control(Msg::IdrRequest(v1::IdrRequest::default()))).await;
            }
            _ = tick.tick() => {
                reasm.expire(Instant::now());
                let report = fb.report(&reasm.stats());
                let _ = quic::send_msg(&mut tx, &control(Msg::AbrFeedback(report))).await;
            }
        }
    }
    let recovered = reasm.stats().frames_recovered;
    drop(dec_tx);
    let (decoded, sizes) = decoder.join().unwrap();
    let _ = tx.finish();
    conn.close(0u32.into(), b"done");
    let _ = tokio::time::timeout(Duration::from_secs(3), host_task).await;
    Outcome {
        decoded,
        sizes,
        recovered,
        final_kbps: ctl_shared.bitrate_kbps.load(Ordering::Relaxed),
        final_fps: ctl_shared.fps.load(Ordering::Relaxed),
        final_scale_milli: ctl_shared.scale_milli.load(Ordering::Relaxed),
        netem: netem_stats,
        started,
    }
}

fn report(name: &str, o: &Outcome, warmup: Duration, total: Duration) {
    let down = *o.netem.down.lock().unwrap();
    eprintln!(
        "[{name}] decoded {:.1} fps, max freeze {:?}, latency p95 {:?}, FEC-recovered {} frames, \
         ABR → {} kbps / {} fps / scale {:.2}, netem down: {} fwd, {} lost, {} queue-dropped",
        o.fps(warmup, total),
        o.max_gap(warmup),
        o.latency_p95(warmup),
        o.recovered,
        o.final_kbps,
        o.final_fps,
        f64::from(o.final_scale_milli) / 1000.0,
        down.forwarded,
        down.lost,
        down.queue_dropped,
    );
}

/// §12 M4: "no freezes > 300 ms at 2% loss".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_percent_loss_no_long_freezes() {
    let link = Link {
        delay: Duration::from_millis(25), // 50 ms RTT
        jitter: Duration::from_millis(5),
        loss: 0.02,
        rate_kbps: Some(20_000),
        queue: Duration::from_millis(100),
    };
    let total = Duration::from_secs(8);
    let o = run(link, link, 2_000, total.as_secs()).await;
    let warmup = Duration::from_secs(2);
    report("2% loss", &o, warmup, total);
    let d = *o.netem.down.lock().unwrap();
    assert!(
        d.lost * 100 >= d.forwarded,
        "loss was applied: {} of {}",
        d.lost,
        d.forwarded
    );
    assert!(o.recovered > 0, "FEC recovered frames");
    assert!(o.fps(warmup, total) > 20.0, "frames keep flowing");
    assert!(
        o.max_gap(warmup) <= Duration::from_millis(300),
        "freeze {:?}",
        o.max_gap(warmup)
    );
}

/// §12 M4: "graceful downscale at 1 Mbps".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_mbit_bottleneck_downscales_gracefully() {
    let down = Link {
        delay: Duration::from_millis(10),
        rate_kbps: Some(1_000),
        queue: Duration::from_millis(200),
        ..Default::default()
    };
    let up = Link {
        delay: Duration::from_millis(10),
        ..Default::default()
    };
    // Start far above the link so ABR has to react.
    let total = Duration::from_secs(12);
    let o = run(up, down, 4_000, total.as_secs()).await;
    let settled = Duration::from_secs(7);
    report("1 Mbit/s", &o, settled, total);
    assert!(
        o.final_kbps <= 1_000,
        "bitrate fits the link: {} kbps",
        o.final_kbps
    );
    assert!(
        o.final_scale_milli < 1000 || o.final_fps < FPS,
        "stepped down fps or resolution"
    );
    assert!(
        o.fps(settled, total) >= 12.0,
        "still moving: {:.1} fps",
        o.fps(settled, total)
    );
    assert!(
        o.latency_p95(settled) <= Duration::from_millis(400),
        "latency p95 {:?}",
        o.latency_p95(settled)
    );
}

/// FR-21: switching monitors re-targets capture and encode mid-session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_switch_changes_the_stream() {
    let o = run_with(
        Link::default(),
        Link::default(),
        3_000,
        4,
        Some(Duration::from_secs(2)),
    )
    .await;
    let first = o.sizes.first().copied();
    let last = o.sizes.last().copied();
    eprintln!(
        "[monitor switch] {:?} → {:?}, {} frames",
        first,
        last,
        o.sizes.len()
    );
    assert_eq!(first, Some((W, H)), "starts on display 1");
    assert_eq!(last, Some((480, 270)), "ends on display 2");
    let after: Vec<_> = o.sizes.iter().skip_while(|s| **s == (W, H)).collect();
    assert!(
        after.iter().all(|s| **s == (480, 270)),
        "no frames from display 1 after the switch"
    );
    assert!(after.len() > 20, "display 2 keeps streaming");
}

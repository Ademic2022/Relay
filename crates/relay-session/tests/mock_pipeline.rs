//! End-to-end Host → Controller video path with mock backends:
//! capture → encode → slice → (lossy "network") → reassemble → decode,
//! with capture/encode/send linked by newest-wins channels.

use std::time::{Duration, Instant};

use relay_capture::{ScreenCapturer, mock::MockCapturer};
use relay_codec::mock::{MockDecoder, MockEncoder};
use relay_codec::{Codec, Decoder, Encoder, EncoderConfig};
use relay_net::video::{DEFAULT_MAX_DATAGRAM, FrameMeta, Reassembler, slice_frame};
use relay_session::idr::IdrLimiter;
use relay_session::latest;

#[test]
fn frames_flow_and_loss_triggers_rate_limited_idr() {
    let mut cap = MockCapturer::single_1080p();
    cap.select(1).unwrap();
    let mut enc = MockEncoder::new(EncoderConfig::low_latency(
        Codec::H264,
        1920,
        1080,
        60,
        3_000,
    ));
    let mut dec = MockDecoder;
    let mut reasm = Reassembler::new(Duration::from_millis(50));
    let mut idr = IdrLimiter::default();
    let (tx, rx) = latest::channel();

    let t0 = Instant::now();
    let mut decoded = 0;
    let mut idr_requests = 0;
    let mut force_key = false;
    for n in 0u32..120 {
        let now = t0 + Duration::from_millis(u64::from(n) * 16);
        let frame = cap.next_frame(Duration::ZERO).unwrap().unwrap();
        let e = enc.encode(&frame, u64::from(n), force_key).unwrap();
        force_key = false;
        tx.send(e);
        let e = rx.try_recv().unwrap();

        let meta = FrameMeta {
            stream_id: 0,
            frame_no: n,
            is_key: e.is_key,
            capture_ts_us: e.capture_ts_us,
        };
        for (i, dg) in slice_frame(meta, &e.data, DEFAULT_MAX_DATAGRAM)
            .unwrap()
            .into_iter()
            .enumerate()
        {
            // Lose one slice of every 10th frame.
            if n % 10 == 5 && i == 0 {
                continue;
            }
            if let Some(f) = reasm.push(dg, now) {
                if f.gap_before && idr.request(now) {
                    idr_requests += 1;
                    force_key = true;
                }
                assert!(dec.decode(&f.data, f.capture_ts_us).unwrap().is_some());
                decoded += 1;
            }
        }
        reasm.expire(now);
    }
    assert_eq!(decoded, 120 - 12);
    // 12 losses over ~1.9 s; the limiter allows at most one per 500 ms.
    assert!(
        (1..=4).contains(&idr_requests),
        "idr_requests={idr_requests}"
    );
}

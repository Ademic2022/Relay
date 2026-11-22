//! Video frame slicing into QUIC datagrams with forward error correction,
//! and newest-wins reassembly.
//!
//! Each encoded frame of `n` data shards is followed by `r` Reed-Solomon
//! recovery shards: any `n` of the `n + r` rebuild the frame, so most
//! packet loss never becomes a lost frame and never needs a keyframe
//! (§6 rule 6, M4). `r` is chosen per frame by the sender from measured
//! loss (see [`recovery_for`]).
//!
//! Wire header (24 bytes, little-endian), followed by the payload:
//!
//! | off | size | field                                         |
//! |-----|------|-----------------------------------------------|
//! | 0   | 1    | version (=2)                                  |
//! | 1   | 1    | flags: bit0 keyframe, bit1 recovery shard     |
//! | 2   | 1    | stream_id                                     |
//! | 3   | 1    | recovery_count `r`                            |
//! | 4   | 4    | frame_no                                      |
//! | 8   | 2    | shard index (data: 0..n, recovery: 0..r)      |
//! | 10  | 2    | data shard count `n`                          |
//! | 12  | 8    | capture_ts_us                                 |
//! | 20  | 4    | frame length in bytes                         |
//!
//! All shards of a frame have the same payload size `P` except the last
//! data shard, which is sent unpadded. A fixed header instead of protobuf
//! keeps per-datagram parsing branch-free and allocation-free.
//!
//! Reassembly follows §6 rule 3: no jitter buffer. A frame is handed out
//! the moment it can be rebuilt, and every older partial frame is dropped.

use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};

pub const HEADER_LEN: usize = 24;
pub const VERSION: u8 = 2;
/// Datagram size that survives typical paths without IP fragmentation.
pub const DEFAULT_MAX_DATAGRAM: usize = 1200;
/// Upper bound on data shards per frame (≈ 4.6 MB at 1176 B payload).
pub const MAX_SLICES: u16 = 4096;
/// Upper bound on recovery shards per frame.
pub const MAX_RECOVERY: u8 = 255;
/// Partial frames kept at once. Anything older is already stale.
const MAX_INFLIGHT: usize = 4;

const FLAG_KEY: u8 = 0b01;
const FLAG_RECOVERY: u8 = 0b10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceHeader {
    pub stream_id: u8,
    pub frame_no: u32,
    pub index: u16,
    pub data_count: u16,
    pub recovery_count: u8,
    pub is_recovery: bool,
    pub is_key: bool,
    pub capture_ts_us: u64,
    pub frame_len: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SliceError {
    #[error("datagram shorter than header")]
    Short,
    #[error("unsupported slice version {0}")]
    Version(u8),
    #[error("invalid slice index/count")]
    BadIndex,
    #[error("frame too large to slice")]
    TooLarge,
    #[error("FEC: {0}")]
    Fec(String),
}

impl SliceHeader {
    pub fn write(&self, out: &mut BytesMut) {
        let mut flags = 0;
        if self.is_key {
            flags |= FLAG_KEY;
        }
        if self.is_recovery {
            flags |= FLAG_RECOVERY;
        }
        out.extend_from_slice(&[VERSION, flags, self.stream_id, self.recovery_count]);
        out.extend_from_slice(&self.frame_no.to_le_bytes());
        out.extend_from_slice(&self.index.to_le_bytes());
        out.extend_from_slice(&self.data_count.to_le_bytes());
        out.extend_from_slice(&self.capture_ts_us.to_le_bytes());
        out.extend_from_slice(&self.frame_len.to_le_bytes());
    }

    pub fn parse(buf: &[u8]) -> Result<Self, SliceError> {
        let h: &[u8; HEADER_LEN] = buf
            .get(..HEADER_LEN)
            .and_then(|s| s.try_into().ok())
            .ok_or(SliceError::Short)?;
        if h[0] != VERSION {
            return Err(SliceError::Version(h[0]));
        }
        let le16 = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
        let le32 = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().expect("4"));
        let hdr = SliceHeader {
            is_key: h[1] & FLAG_KEY != 0,
            is_recovery: h[1] & FLAG_RECOVERY != 0,
            stream_id: h[2],
            recovery_count: h[3],
            frame_no: le32(4),
            index: le16(8),
            data_count: le16(10),
            capture_ts_us: u64::from_le_bytes(h[12..20].try_into().expect("8")),
            frame_len: le32(20),
        };
        let limit = if hdr.is_recovery {
            u16::from(hdr.recovery_count)
        } else {
            hdr.data_count
        };
        if hdr.data_count == 0 || hdr.data_count > MAX_SLICES || hdr.index >= limit {
            return Err(SliceError::BadIndex);
        }
        Ok(hdr)
    }
}

/// Metadata for one encoded frame being sent.
#[derive(Debug, Clone, Copy)]
pub struct FrameMeta {
    pub stream_id: u8,
    pub frame_no: u32,
    pub is_key: bool,
    pub capture_ts_us: u64,
}

/// Recovery shards for `n` data shards at measured `loss` (0..1).
///
/// Sized so that losing more than `r` of `n + r` shards is rare at that
/// loss rate (~1e-5 per frame at 2%), plus a floor so isolated losses on a
/// clean link are absorbed too. Keyframes get extra protection: losing one
/// costs the most.
pub fn recovery_for(n: usize, loss: f32, is_key: bool) -> usize {
    ((n as f32 * recovery_ratio(loss, is_key)).ceil() as usize).clamp(1, usize::from(MAX_RECOVERY))
}

/// Recovery shards per data shard at `loss`. Capped at 0.5 (0.75 for
/// keyframes): beyond ~5% loss the cause is congestion, and more parity
/// would only add to it. Senders budget for it, see [`fec_overhead`].
pub fn recovery_ratio(loss: f32, is_key: bool) -> f32 {
    (0.1 + 8.0 * loss.clamp(0.0, 0.05)) * if is_key { 1.5 } else { 1.0 }
}

/// Fraction of the wire rate the encoder may use, leaving room for FEC.
pub fn fec_overhead(loss: f32) -> f32 {
    1.0 / (1.0 + recovery_ratio(loss, false))
}

/// [`slice_frame`] with recovery sized by [`recovery_for`] at `loss`.
pub fn slice_for_loss(
    meta: FrameMeta,
    data: &[u8],
    max_datagram: usize,
    loss: f32,
) -> Result<Vec<Bytes>, SliceError> {
    let payload = (max_datagram.saturating_sub(HEADER_LEN) & !1).max(2);
    let n = data.len().div_ceil(payload).max(1);
    slice_frame(meta, data, max_datagram, recovery_for(n, loss, meta.is_key))
}

/// Split an encoded frame into datagrams of at most `max_datagram` bytes:
/// `n` data shards, then `recovery` Reed-Solomon recovery shards.
pub fn slice_frame(
    meta: FrameMeta,
    data: &[u8],
    max_datagram: usize,
    recovery: usize,
) -> Result<Vec<Bytes>, SliceError> {
    // Shard size: even, as Reed-Solomon requires.
    let payload = max_datagram
        .checked_sub(HEADER_LEN)
        .map(|p| p & !1)
        .filter(|&p| p > 0)
        .ok_or(SliceError::TooLarge)?;
    let n = data.len().div_ceil(payload).max(1);
    let n16 = u16::try_from(n)
        .ok()
        .filter(|&c| c <= MAX_SLICES)
        .ok_or(SliceError::TooLarge)?;
    let r = recovery.min(usize::from(MAX_RECOVERY));
    let frame_len = u32::try_from(data.len()).map_err(|_| SliceError::TooLarge)?;
    let header = |index: u16, is_recovery: bool| SliceHeader {
        stream_id: meta.stream_id,
        frame_no: meta.frame_no,
        index,
        data_count: n16,
        recovery_count: r as u8,
        is_recovery,
        is_key: meta.is_key,
        capture_ts_us: meta.capture_ts_us,
        frame_len,
    };

    let mut out = Vec::with_capacity(n + r);
    let mut chunks = data.chunks(payload);
    for idx in 0..n16 {
        let chunk = chunks.next().unwrap_or(&[]);
        let mut buf = BytesMut::with_capacity(HEADER_LEN + chunk.len());
        header(idx, false).write(&mut buf);
        buf.extend_from_slice(chunk);
        out.push(buf.freeze());
    }
    if r > 0 {
        // Pad the last shard for coding only; it is sent unpadded.
        let shards: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                let mut v = data
                    .get(i * payload..((i + 1) * payload).min(data.len()))
                    .unwrap_or(&[])
                    .to_vec();
                v.resize(payload, 0);
                v
            })
            .collect();
        let parity =
            reed_solomon_simd::encode(n, r, &shards).map_err(|e| SliceError::Fec(e.to_string()))?;
        for (i, p) in parity.iter().enumerate() {
            let mut buf = BytesMut::with_capacity(HEADER_LEN + p.len());
            header(i as u16, true).write(&mut buf);
            buf.extend_from_slice(p);
            out.push(buf.freeze());
        }
    }
    Ok(out)
}

/// A fully reassembled frame ready for the decoder.
#[derive(Debug, Clone)]
pub struct CompleteFrame {
    pub frame_no: u32,
    pub is_key: bool,
    pub capture_ts_us: u64,
    pub data: Bytes,
    /// Frames were lost between the previous delivered frame and this one,
    /// and this one is not a keyframe: the reference chain may be broken.
    pub gap_before: bool,
    /// Rebuilt from recovery shards.
    pub recovered: bool,
}

#[derive(Debug)]
struct Partial {
    frame_no: u32,
    is_key: bool,
    capture_ts_us: u64,
    frame_len: u32,
    first_seen: Instant,
    data: Vec<Option<Bytes>>,
    recovery: Vec<Option<Bytes>>,
    data_rx: usize,
    recovery_rx: usize,
}

/// Counters reported in ABR feedback and the stats overlay.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReassemblyStats {
    /// Shards announced by every frame seen (n + r) and shards that
    /// arrived; their ratio is packet loss, which FEC hides from frame
    /// counts and the sender needs to size recovery.
    pub shards_expected: u64,
    pub shards_received: u64,
    pub frames_delivered: u64,
    pub frames_dropped: u64,
    pub frames_recovered: u64,
    pub slices_ignored: u64,
}

/// Reassembles one video stream. Not thread-safe; one per receive task.
#[derive(Debug)]
pub struct Reassembler {
    deadline: Duration,
    inflight: Vec<Partial>,
    last_delivered: Option<u32>,
    stats: ReassemblyStats,
}

/// Wrap-aware "a is newer than b" for 32-bit frame counters.
fn newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < (1 << 31)
}

impl Reassembler {
    /// `deadline`: how long a partial frame may wait for missing slices.
    pub fn new(deadline: Duration) -> Self {
        Self {
            deadline,
            inflight: Vec::with_capacity(MAX_INFLIGHT),
            last_delivered: None,
            stats: ReassemblyStats::default(),
        }
    }

    pub fn stats(&self) -> ReassemblyStats {
        self.stats
    }

    /// Feed one datagram. Returns a frame if this slice completed one.
    pub fn push(&mut self, datagram: Bytes, now: Instant) -> Option<CompleteFrame> {
        let Ok(hdr) = SliceHeader::parse(&datagram) else {
            self.stats.slices_ignored += 1;
            return None;
        };
        self.stats.shards_received += 1;
        if self.last_delivered.is_some_and(|l| !newer(hdr.frame_no, l)) {
            self.stats.slices_ignored += 1; // stale or already delivered
            return None;
        }

        let pos = match self
            .inflight
            .iter()
            .position(|p| p.frame_no == hdr.frame_no)
        {
            Some(i) => i,
            None => {
                if self.inflight.len() == MAX_INFLIGHT {
                    self.drop_oldest();
                }
                self.stats.shards_expected +=
                    u64::from(hdr.data_count) + u64::from(hdr.recovery_count);
                self.inflight.push(Partial {
                    frame_no: hdr.frame_no,
                    is_key: hdr.is_key,
                    capture_ts_us: hdr.capture_ts_us,
                    frame_len: hdr.frame_len,
                    first_seen: now,
                    data: vec![None; hdr.data_count as usize],
                    recovery: vec![None; hdr.recovery_count as usize],
                    data_rx: 0,
                    recovery_rx: 0,
                });
                self.inflight.len() - 1
            }
        };
        let p = &mut self.inflight[pos];
        if p.data.len() != hdr.data_count as usize
            || p.recovery.len() != hdr.recovery_count as usize
            || p.frame_len != hdr.frame_len
        {
            self.stats.slices_ignored += 1; // inconsistent header
            return None;
        }
        let (slots, counter) = if hdr.is_recovery {
            (&mut p.recovery, &mut p.recovery_rx)
        } else {
            (&mut p.data, &mut p.data_rx)
        };
        let slot = &mut slots[hdr.index as usize];
        if slot.is_some() {
            self.stats.slices_ignored += 1; // duplicate
            return None;
        }
        *slot = Some(datagram.slice(HEADER_LEN..));
        *counter += 1;

        let n = p.data.len();
        let complete = p.data_rx == n;
        if !complete && p.data_rx + p.recovery_rx < n {
            return None;
        }
        let mut p = self.inflight.swap_remove(pos);
        let recovered = !complete;
        if recovered && !rebuild(&mut p) {
            self.stats.frames_dropped += 1;
            return None;
        }

        // Newest complete wins: every older partial frame is now useless.
        let before = self.inflight.len();
        self.inflight.retain(|q| newer(q.frame_no, p.frame_no));
        self.stats.frames_dropped += (before - self.inflight.len()) as u64;

        let gap_before = !p.is_key
            && self
                .last_delivered
                .is_some_and(|l| p.frame_no != l.wrapping_add(1));
        self.last_delivered = Some(p.frame_no);
        self.stats.frames_delivered += 1;
        self.stats.frames_recovered += u64::from(recovered);

        let len = p.frame_len as usize;
        let data = if n == 1 && !recovered {
            let mut b = p.data.into_iter().next().flatten().expect("complete");
            b.truncate(len);
            b
        } else {
            // Copy of the compressed bitstream (not a raw frame) into one
            // contiguous buffer for the decoder.
            let mut buf = BytesMut::with_capacity(len);
            for s in p.data.iter().flatten() {
                let take = s.len().min(len - buf.len());
                buf.extend_from_slice(&s[..take]);
            }
            buf.freeze()
        };
        Some(CompleteFrame {
            frame_no: p.frame_no,
            is_key: p.is_key,
            capture_ts_us: p.capture_ts_us,
            data,
            gap_before,
            recovered,
        })
    }

    /// Drop partial frames past their deadline. Returns how many.
    pub fn expire(&mut self, now: Instant) -> usize {
        let before = self.inflight.len();
        let deadline = self.deadline;
        self.inflight
            .retain(|p| now.saturating_duration_since(p.first_seen) < deadline);
        let n = before - self.inflight.len();
        self.stats.frames_dropped += n as u64;
        n
    }

    fn drop_oldest(&mut self) {
        if let Some((i, _)) = self.inflight.iter().enumerate().min_by(|(_, a), (_, b)| {
            if newer(a.frame_no, b.frame_no) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }) {
            self.inflight.swap_remove(i);
            self.stats.frames_dropped += 1;
        }
    }
}

/// Fill missing data shards from recovery shards. `false` if impossible.
fn rebuild(p: &mut Partial) -> bool {
    let Some(shard) = p.recovery.iter().flatten().next().map(Bytes::len) else {
        return false;
    };
    let n = p.data.len();
    let padded: Vec<(usize, Vec<u8>)> = p
        .data
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            s.as_ref().map(|b| {
                let mut v = b.to_vec();
                v.resize(shard, 0);
                (i, v)
            })
        })
        .collect();
    let rec: Vec<(usize, &Bytes)> = p
        .recovery
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().map(|b| (i, b)))
        .collect();
    if rec.iter().any(|(_, b)| b.len() != shard) {
        return false;
    }
    match reed_solomon_simd::decode(n, p.recovery.len(), padded, rec) {
        Ok(restored) => {
            for (i, v) in restored {
                if let Some(slot) = p.data.get_mut(i) {
                    *slot = Some(Bytes::from(v));
                }
            }
            p.data.iter().all(Option::is_some)
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(frame_no: u32, is_key: bool) -> FrameMeta {
        FrameMeta {
            stream_id: 0,
            frame_no,
            is_key,
            capture_ts_us: 1000 + u64::from(frame_no),
        }
    }

    fn frame(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    fn deliver(r: &mut Reassembler, dgs: impl IntoIterator<Item = Bytes>) -> Option<CompleteFrame> {
        let now = Instant::now();
        let mut out = None;
        for d in dgs {
            out = r.push(d, now).or(out);
        }
        out
    }

    #[test]
    fn header_round_trip() {
        let h = SliceHeader {
            stream_id: 3,
            frame_no: 0xDEAD_BEEF,
            index: 2,
            data_count: 9,
            recovery_count: 3,
            is_recovery: true,
            is_key: true,
            capture_ts_us: 123_456_789_012,
            frame_len: 10_000,
        };
        let mut b = BytesMut::new();
        h.write(&mut b);
        assert_eq!(b.len(), HEADER_LEN);
        assert_eq!(SliceHeader::parse(&b).unwrap(), h);
    }

    #[test]
    fn header_rejects_garbage() {
        assert_eq!(SliceHeader::parse(&[2; 5]), Err(SliceError::Short));
        let mut h = SliceHeader {
            stream_id: 0,
            frame_no: 0,
            index: 2,
            data_count: 2,
            recovery_count: 0,
            is_recovery: false,
            is_key: false,
            capture_ts_us: 0,
            frame_len: 0,
        };
        let mut b = BytesMut::new();
        h.write(&mut b);
        assert_eq!(SliceHeader::parse(&b), Err(SliceError::BadIndex));
        h.is_recovery = true; // recovery index 2 with r = 0
        let mut b = BytesMut::new();
        h.write(&mut b);
        assert_eq!(SliceHeader::parse(&b), Err(SliceError::BadIndex));
        b[0] = 9;
        assert_eq!(SliceHeader::parse(&b), Err(SliceError::Version(9)));
    }

    #[test]
    fn slices_fit_datagram() {
        let data = frame(10_000, 0);
        let s = slice_frame(meta(1, true), &data, DEFAULT_MAX_DATAGRAM, 2).unwrap();
        let payload = (DEFAULT_MAX_DATAGRAM - HEADER_LEN) & !1;
        assert_eq!(s.len(), 10_000usize.div_ceil(payload) + 2);
        assert!(s.iter().all(|d| d.len() <= DEFAULT_MAX_DATAGRAM));
    }

    #[test]
    fn reassembles_out_of_order() {
        let data = frame(5000, 1);
        let mut slices = slice_frame(meta(1, true), &data, 600, 0).unwrap();
        slices.reverse();
        let f =
            deliver(&mut Reassembler::new(Duration::from_millis(50)), slices).expect("complete");
        assert_eq!(&f.data[..], &data[..]);
        assert!(f.is_key && !f.gap_before && !f.recovered);
    }

    #[test]
    fn recovers_lost_shards() {
        for (len, r, lose) in [
            (10_000, 3, vec![0, 4, 8]),
            (5000, 2, vec![4]),
            (300, 1, vec![0]),
            (4097, 2, vec![3, 1]),
        ] {
            let data = frame(len, 7);
            let slices = slice_frame(meta(5, false), &data, 1200, r).unwrap();
            let kept = slices
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !lose.contains(i))
                .map(|(_, d)| d);
            let f = deliver(&mut Reassembler::new(Duration::from_millis(50)), kept)
                .unwrap_or_else(|| panic!("len {len} lose {lose:?}"));
            assert_eq!(&f.data[..], &data[..], "len {len}");
            assert!(f.recovered);
        }
    }

    #[test]
    fn too_much_loss_drops_frame() {
        let data = frame(10_000, 7);
        let slices = slice_frame(meta(5, false), &data, 1200, 2).unwrap();
        let kept = slices
            .into_iter()
            .enumerate()
            .filter(|(i, _)| ![0, 1, 2].contains(i))
            .map(|(_, d)| d);
        assert!(deliver(&mut Reassembler::new(Duration::from_millis(50)), kept).is_none());
    }

    #[test]
    fn counts_packet_loss() {
        let data = frame(10_000, 7);
        let slices = slice_frame(meta(5, false), &data, 1200, 3).unwrap();
        let total = slices.len() as u64;
        let mut r = Reassembler::new(Duration::from_millis(50));
        deliver(&mut r, slices.into_iter().skip(1));
        let st = r.stats();
        assert_eq!(st.shards_expected, total);
        assert_eq!(st.shards_received, total - 1);
    }

    #[test]
    fn recovery_sizing() {
        assert_eq!(recovery_for(1, 0.0, false), 1);
        assert_eq!(recovery_for(10, 0.0, false), 1);
        assert_eq!(recovery_for(14, 0.02, false), 4);
        assert!(recovery_for(14, 0.02, true) > 4);
        assert_eq!(recovery_for(4000, 0.1, true), usize::from(MAX_RECOVERY));
        assert!((fec_overhead(0.5) - 1.0 / 1.5).abs() < 1e-6, "ratio capped");
    }

    #[test]
    fn newest_complete_wins() {
        let now = Instant::now();
        let mut r = Reassembler::new(Duration::from_millis(50));
        let f1 = slice_frame(meta(1, false), &frame(3000, 1), 1200, 0).unwrap();
        let f2 = slice_frame(meta(2, false), &frame(100, 2), 1200, 0).unwrap();
        assert!(r.push(f1[0].clone(), now).is_none());
        let got = r.push(f2[0].clone(), now).unwrap();
        assert_eq!(got.frame_no, 2);
        assert!(r.push(f1[1].clone(), now).is_none());
        assert!(r.push(f1[2].clone(), now).is_none());
        assert_eq!(r.stats().frames_dropped, 1);
    }

    #[test]
    fn late_recovery_shards_after_completion_are_ignored() {
        let data = frame(3000, 1);
        let s = slice_frame(meta(1, false), &data, 1200, 2).unwrap();
        let now = Instant::now();
        let mut r = Reassembler::new(Duration::from_millis(50));
        let mut got = 0;
        for d in s {
            got += r.push(d, now).is_some() as u32;
        }
        assert_eq!(got, 1, "frame delivered exactly once");
    }

    #[test]
    fn gap_flag_on_loss() {
        let now = Instant::now();
        let mut r = Reassembler::new(Duration::from_millis(50));
        let push_all = |r: &mut Reassembler, m: FrameMeta| {
            slice_frame(m, &frame(10, 0), 1200, 0)
                .unwrap()
                .into_iter()
                .filter_map(|s| r.push(s, now))
                .last()
                .unwrap()
        };
        assert!(!push_all(&mut r, meta(1, true)).gap_before);
        assert!(!push_all(&mut r, meta(2, false)).gap_before);
        assert!(push_all(&mut r, meta(4, false)).gap_before);
        assert!(
            !push_all(&mut r, meta(6, true)).gap_before,
            "key resets chain"
        );
    }

    #[test]
    fn expires_partials() {
        let t0 = Instant::now();
        let mut r = Reassembler::new(Duration::from_millis(30));
        let f = slice_frame(meta(1, false), &frame(3000, 0), 1200, 0).unwrap();
        r.push(f[0].clone(), t0);
        assert_eq!(r.expire(t0 + Duration::from_millis(10)), 0);
        assert_eq!(r.expire(t0 + Duration::from_millis(31)), 1);
    }

    #[test]
    fn bounded_inflight() {
        let now = Instant::now();
        let mut r = Reassembler::new(Duration::from_secs(1));
        for n in 0..20 {
            let f = slice_frame(meta(n, false), &frame(3000, 0), 1200, 0).unwrap();
            r.push(f[0].clone(), now);
        }
        assert!(r.inflight.len() <= MAX_INFLIGHT);
    }

    #[test]
    fn frame_counter_wraps() {
        assert!(newer(0, u32::MAX));
        assert!(!newer(u32::MAX, 0));
    }
}

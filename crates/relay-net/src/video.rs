//! Video frame slicing into QUIC datagrams, and newest-wins reassembly.
//!
//! Wire header (20 bytes, little-endian), followed by the payload:
//!
//! | off | size | field         |
//! |-----|------|---------------|
//! | 0   | 1    | version (=1)  |
//! | 1   | 1    | flags (bit0 = keyframe) |
//! | 2   | 1    | stream_id     |
//! | 3   | 1    | reserved (0)  |
//! | 4   | 4    | frame_no      |
//! | 8   | 2    | slice_idx     |
//! | 10  | 2    | slice_count   |
//! | 12  | 8    | capture_ts_us |
//!
//! A fixed header instead of protobuf keeps per-datagram parsing branch-free
//! and allocation-free on the hot path.
//!
//! Reassembly follows §6 rule 3: no jitter buffer. A frame is handed out the
//! moment its last slice arrives, and every older partial frame is dropped.

use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};

pub const HEADER_LEN: usize = 20;
pub const VERSION: u8 = 1;
/// Datagram size that survives typical paths without IP fragmentation.
pub const DEFAULT_MAX_DATAGRAM: usize = 1200;
/// Upper bound on slices per frame (≈ 4.7 MB at 1180 B payload).
pub const MAX_SLICES: u16 = 4096;
/// Partial frames kept at once. Anything older is already stale.
const MAX_INFLIGHT: usize = 4;

const FLAG_KEY: u8 = 0b1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceHeader {
    pub stream_id: u8,
    pub frame_no: u32,
    pub slice_idx: u16,
    pub slice_count: u16,
    pub is_key: bool,
    pub capture_ts_us: u64,
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
}

impl SliceHeader {
    pub fn write(&self, out: &mut BytesMut) {
        out.extend_from_slice(&[
            VERSION,
            if self.is_key { FLAG_KEY } else { 0 },
            self.stream_id,
            0,
        ]);
        out.extend_from_slice(&self.frame_no.to_le_bytes());
        out.extend_from_slice(&self.slice_idx.to_le_bytes());
        out.extend_from_slice(&self.slice_count.to_le_bytes());
        out.extend_from_slice(&self.capture_ts_us.to_le_bytes());
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
        let hdr = SliceHeader {
            is_key: h[1] & FLAG_KEY != 0,
            stream_id: h[2],
            frame_no: u32::from_le_bytes(h[4..8].try_into().expect("4")),
            slice_idx: le16(8),
            slice_count: le16(10),
            capture_ts_us: u64::from_le_bytes(h[12..20].try_into().expect("8")),
        };
        if hdr.slice_count == 0 || hdr.slice_count > MAX_SLICES || hdr.slice_idx >= hdr.slice_count
        {
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

/// Split an encoded frame into datagrams of at most `max_datagram` bytes.
pub fn slice_frame(
    meta: FrameMeta,
    data: &[u8],
    max_datagram: usize,
) -> Result<Vec<Bytes>, SliceError> {
    let payload = max_datagram
        .checked_sub(HEADER_LEN)
        .filter(|&p| p > 0)
        .ok_or(SliceError::TooLarge)?;
    let count = data.len().div_ceil(payload).max(1);
    let count = u16::try_from(count)
        .ok()
        .filter(|&c| c <= MAX_SLICES)
        .ok_or(SliceError::TooLarge)?;
    let mut out = Vec::with_capacity(count as usize);
    let mut chunks = data.chunks(payload);
    for idx in 0..count {
        let chunk = chunks.next().unwrap_or(&[]);
        let mut buf = BytesMut::with_capacity(HEADER_LEN + chunk.len());
        SliceHeader {
            stream_id: meta.stream_id,
            frame_no: meta.frame_no,
            slice_idx: idx,
            slice_count: count,
            is_key: meta.is_key,
            capture_ts_us: meta.capture_ts_us,
        }
        .write(&mut buf);
        buf.extend_from_slice(chunk);
        out.push(buf.freeze());
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
}

#[derive(Debug)]
struct Partial {
    frame_no: u32,
    is_key: bool,
    capture_ts_us: u64,
    first_seen: Instant,
    slices: Vec<Option<Bytes>>,
    received: u16,
}

/// Counters reported in ABR feedback and the stats overlay.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReassemblyStats {
    pub frames_delivered: u64,
    pub frames_dropped: u64,
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
        if self.last_delivered.is_some_and(|l| !newer(hdr.frame_no, l)) {
            self.stats.slices_ignored += 1; // stale: a newer frame was shown
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
                self.inflight.push(Partial {
                    frame_no: hdr.frame_no,
                    is_key: hdr.is_key,
                    capture_ts_us: hdr.capture_ts_us,
                    first_seen: now,
                    slices: vec![None; hdr.slice_count as usize],
                    received: 0,
                });
                self.inflight.len() - 1
            }
        };
        let p = &mut self.inflight[pos];
        if p.slices.len() != hdr.slice_count as usize {
            self.stats.slices_ignored += 1; // inconsistent header
            return None;
        }
        let slot = &mut p.slices[hdr.slice_idx as usize];
        if slot.is_some() {
            self.stats.slices_ignored += 1; // duplicate
            return None;
        }
        *slot = Some(datagram.slice(HEADER_LEN..));
        p.received += 1;
        if p.received as usize != p.slices.len() {
            return None;
        }

        let p = self.inflight.swap_remove(pos);
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

        let data = if p.slices.len() == 1 {
            p.slices.into_iter().next().flatten().expect("complete")
        } else {
            // Unavoidable copy of the compressed bitstream (not a raw frame)
            // into one contiguous buffer for the decoder.
            let total = p.slices.iter().flatten().map(Bytes::len).sum();
            let mut buf = BytesMut::with_capacity(total);
            for s in p.slices.iter().flatten() {
                buf.extend_from_slice(s);
            }
            buf.freeze()
        };
        Some(CompleteFrame {
            frame_no: p.frame_no,
            is_key: p.is_key,
            capture_ts_us: p.capture_ts_us,
            data,
            gap_before,
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
        (0..len).map(|i| (i as u8).wrapping_add(seed)).collect()
    }

    #[test]
    fn header_round_trip() {
        let h = SliceHeader {
            stream_id: 3,
            frame_no: 0xDEAD_BEEF,
            slice_idx: 7,
            slice_count: 9,
            is_key: true,
            capture_ts_us: 123_456_789_012,
        };
        let mut b = BytesMut::new();
        h.write(&mut b);
        assert_eq!(b.len(), HEADER_LEN);
        assert_eq!(SliceHeader::parse(&b).unwrap(), h);
    }

    #[test]
    fn header_rejects_garbage() {
        assert_eq!(SliceHeader::parse(&[1; 5]), Err(SliceError::Short));
        let mut b = BytesMut::new();
        SliceHeader {
            stream_id: 0,
            frame_no: 0,
            slice_idx: 2,
            slice_count: 2,
            is_key: false,
            capture_ts_us: 0,
        }
        .write(&mut b);
        assert_eq!(SliceHeader::parse(&b), Err(SliceError::BadIndex));
        b[0] = 9;
        assert_eq!(SliceHeader::parse(&b), Err(SliceError::Version(9)));
    }

    #[test]
    fn slices_fit_datagram() {
        let data = frame(10_000, 0);
        let s = slice_frame(meta(1, true), &data, DEFAULT_MAX_DATAGRAM).unwrap();
        assert_eq!(
            s.len(),
            10_000usize.div_ceil(DEFAULT_MAX_DATAGRAM - HEADER_LEN)
        );
        assert!(s.iter().all(|d| d.len() <= DEFAULT_MAX_DATAGRAM));
    }

    #[test]
    fn reassembles_out_of_order() {
        let data = frame(5000, 1);
        let mut slices = slice_frame(meta(1, true), &data, 600).unwrap();
        slices.reverse();
        let mut r = Reassembler::new(Duration::from_millis(50));
        let now = Instant::now();
        let mut out = None;
        for s in slices {
            out = r.push(s, now).or(out);
        }
        let f = out.expect("complete");
        assert_eq!(&f.data[..], &data[..]);
        assert!(f.is_key && !f.gap_before);
    }

    #[test]
    fn newest_complete_wins() {
        let now = Instant::now();
        let mut r = Reassembler::new(Duration::from_millis(50));
        let f1 = slice_frame(meta(1, false), &frame(3000, 1), 1200).unwrap();
        let f2 = slice_frame(meta(2, false), &frame(100, 2), 1200).unwrap();
        // Frame 1 partially arrives, then frame 2 completes.
        assert!(r.push(f1[0].clone(), now).is_none());
        let got = r.push(f2[0].clone(), now).unwrap();
        assert_eq!(got.frame_no, 2);
        // Late slices of frame 1 are stale and ignored.
        assert!(r.push(f1[1].clone(), now).is_none());
        assert!(r.push(f1[2].clone(), now).is_none());
        assert_eq!(r.stats().frames_dropped, 1);
    }

    #[test]
    fn gap_flag_on_loss() {
        let now = Instant::now();
        let mut r = Reassembler::new(Duration::from_millis(50));
        let push_all = |r: &mut Reassembler, m: FrameMeta| {
            slice_frame(m, &frame(10, 0), 1200)
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
        let f = slice_frame(meta(1, false), &frame(3000, 0), 1200).unwrap();
        r.push(f[0].clone(), t0);
        assert_eq!(r.expire(t0 + Duration::from_millis(10)), 0);
        assert_eq!(r.expire(t0 + Duration::from_millis(31)), 1);
    }

    #[test]
    fn bounded_inflight() {
        let now = Instant::now();
        let mut r = Reassembler::new(Duration::from_secs(1));
        for n in 0..20 {
            let f = slice_frame(meta(n, false), &frame(3000, 0), 1200).unwrap();
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

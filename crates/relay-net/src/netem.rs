//! A small network emulator for tests (§11): a UDP shim between two peers
//! that adds delay, jitter, random loss and a bandwidth cap with a
//! drop-tail queue, independently per direction.
//!
//! ```text
//! client ──► [netem listen addr] ──up──► upstream
//!        ◄──                      ◄─down──
//! ```

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::Instant;

/// Impairments for one direction.
#[derive(Debug, Clone, Copy)]
pub struct Link {
    /// One-way propagation delay.
    pub delay: Duration,
    /// Uniform extra delay in `0..jitter` (may reorder packets).
    pub jitter: Duration,
    /// Probability of dropping each packet.
    pub loss: f64,
    /// Bottleneck rate; `None` = unlimited.
    pub rate_kbps: Option<u32>,
    /// Queue at the bottleneck before packets are dropped.
    pub queue: Duration,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
            loss: 0.0,
            rate_kbps: None,
            queue: Duration::from_millis(200),
        }
    }
}

/// Counters, for asserting on what the emulator did.
#[derive(Debug, Default, Clone, Copy)]
pub struct Counters {
    pub forwarded: u64,
    pub lost: u64,
    pub queue_dropped: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub up: Arc<Mutex<Counters>>,
    pub down: Arc<Mutex<Counters>>,
}

struct Shaper {
    link: Link,
    free_at: Instant,
    rng: u64,
}

impl Shaper {
    fn rand(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }

    /// When to deliver a packet of `len` bytes, or `None` to drop it.
    fn schedule(&mut self, len: usize, now: Instant, c: &mut Counters) -> Option<Instant> {
        if self.rand() < self.link.loss {
            c.lost += 1;
            return None;
        }
        let mut sent = now;
        if let Some(kbps) = self.link.rate_kbps {
            let start = self.free_at.max(now);
            if start.duration_since(now) > self.link.queue {
                c.queue_dropped += 1;
                return None;
            }
            let tx = Duration::from_secs_f64(len as f64 * 8.0 / (f64::from(kbps.max(1)) * 1000.0));
            self.free_at = start + tx;
            sent = self.free_at;
        }
        let jitter = self.link.jitter.mul_f64(self.rand());
        c.forwarded += 1;
        Some(sent + self.link.delay + jitter)
    }
}

/// Start the emulator. Returns the address clients should send to.
pub async fn start(
    listen: SocketAddr,
    upstream: SocketAddr,
    up: Link,
    down: Link,
) -> std::io::Result<(SocketAddr, Stats)> {
    let front = Arc::new(UdpSocket::bind(listen).await?);
    let back = Arc::new(UdpSocket::bind(SocketAddr::new(listen.ip(), 0)).await?);
    let addr = front.local_addr()?;
    let stats = Stats::default();
    let client: Arc<Mutex<Option<SocketAddr>>> = Arc::default();

    // client → upstream
    {
        let (front, back, client, counters) = (
            front.clone(),
            back.clone(),
            client.clone(),
            stats.up.clone(),
        );
        let mut shaper = Shaper {
            link: up,
            free_at: Instant::now(),
            rng: 0x9E37_79B9_7F4A_7C15,
        };
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65_536];
            while let Ok((n, from)) = front.recv_from(&mut buf).await {
                *client.lock().expect("poisoned") = Some(from);
                let at =
                    shaper.schedule(n, Instant::now(), &mut counters.lock().expect("poisoned"));
                if let Some(at) = at {
                    let (back, data) = (back.clone(), buf[..n].to_vec());
                    tokio::spawn(async move {
                        tokio::time::sleep_until(at).await;
                        let _ = back.send_to(&data, upstream).await;
                    });
                }
            }
        });
    }
    // upstream → client
    {
        let (front, back, client, counters) = (front, back, client, stats.down.clone());
        let mut shaper = Shaper {
            link: down,
            free_at: Instant::now(),
            rng: 0xD1B5_4A32_D192_ED03,
        };
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65_536];
            while let Ok((n, _)) = back.recv_from(&mut buf).await {
                let Some(to) = *client.lock().expect("poisoned") else {
                    continue;
                };
                let at =
                    shaper.schedule(n, Instant::now(), &mut counters.lock().expect("poisoned"));
                if let Some(at) = at {
                    let (front, data) = (front.clone(), buf[..n].to_vec());
                    tokio::spawn(async move {
                        tokio::time::sleep_until(at).await;
                        let _ = front.send_to(&data, to).await;
                    });
                }
            }
        });
    }
    Ok((addr, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delays_and_drops() {
        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 2048];
            while let Ok((n, f)) = echo.recv_from(&mut b).await {
                let _ = echo.send_to(&b[..n], f).await;
            }
        });
        let link = Link {
            delay: Duration::from_millis(20),
            loss: 0.5,
            ..Default::default()
        };
        let (addr, stats) = start(
            "127.0.0.1:0".parse().unwrap(),
            echo_addr,
            link,
            Link::default(),
        )
        .await
        .unwrap();
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let t = std::time::Instant::now();
        for _ in 0..200 {
            c.send_to(b"x", addr).await.unwrap();
        }
        let mut got = 0;
        let mut b = [0u8; 16];
        while let Ok(Ok(_)) =
            tokio::time::timeout(Duration::from_millis(200), c.recv_from(&mut b)).await
        {
            got += 1;
        }
        assert!(t.elapsed() >= Duration::from_millis(20));
        assert!((60..140).contains(&got), "~50% loss, got {got}/200");
        assert_eq!(stats.up.lock().unwrap().forwarded, got);
    }

    #[tokio::test]
    async fn rate_cap_limits_throughput() {
        let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sink_addr = sink.local_addr().unwrap();
        let link = Link {
            rate_kbps: Some(1000),
            queue: Duration::from_millis(100),
            ..Default::default()
        };
        let (addr, stats) = start(
            "127.0.0.1:0".parse().unwrap(),
            sink_addr,
            link,
            Link::default(),
        )
        .await
        .unwrap();
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // 1 MB offered at once into a 1 Mbit/s link with a 100 ms queue.
        for _ in 0..1000 {
            c.send_to(&[0u8; 1000], addr).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let s = *stats.up.lock().unwrap();
        // 100 ms of queue at 1 Mbit/s ≈ 12.5 KB ≈ 13 packets.
        assert!(s.forwarded < 30, "forwarded {}", s.forwarded);
        assert!(s.queue_dropped > 900);
    }
}

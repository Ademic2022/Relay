//! A UDP socket shared by QUIC and control packets.
//!
//! NAT traversal needs everything a peer sends (rendezvous, hole-punch
//! probes, relay binding and the QUIC session itself) to leave from one
//! UDP port, so the NAT mapping seen by others is the one QUIC uses.
//! [`DemuxSocket`] wraps quinn's socket: datagrams that cannot be QUIC
//! (see [`crate::ctl`]) are diverted to a channel, and [`RawSender`] sends
//! raw control packets from the same socket.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll, ready};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::mpsc;

pub type ControlRx = mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>;

pub struct DemuxSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    control: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    paths: Paths,
}

/// Address virtualisation used to move a relayed session onto a direct
/// path without QUIC noticing (QUIC cannot migrate the server address).
///
/// For a mapping `virt → real`: with `recv`, datagrams from `real` are
/// handed to QUIC as if they came from `virt`; with `send`, datagrams
/// QUIC addresses to `virt` go to `real` instead.
#[derive(Clone, Default)]
pub struct Paths {
    any: Arc<AtomicBool>,
    map: Arc<RwLock<Vec<Mapping>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mapping {
    virt: SocketAddr,
    real: SocketAddr,
    send: bool,
    recv: bool,
}

impl fmt::Debug for Paths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.map.read().expect("poisoned").iter())
            .finish()
    }
}

impl Paths {
    /// Install or update the mapping for `virt`.
    pub fn set(&self, virt: SocketAddr, real: SocketAddr, send: bool, recv: bool) {
        let mut m = self.map.write().expect("poisoned");
        m.retain(|x| x.virt != virt);
        m.push(Mapping {
            virt,
            real,
            send,
            recv,
        });
        self.any.store(true, Ordering::Release);
    }

    pub fn clear(&self, virt: SocketAddr) {
        let mut m = self.map.write().expect("poisoned");
        m.retain(|x| x.virt != virt);
        self.any.store(!m.is_empty(), Ordering::Release);
    }

    fn outgoing(&self, to: SocketAddr) -> SocketAddr {
        if !self.any.load(Ordering::Acquire) {
            return to;
        }
        self.map
            .read()
            .expect("poisoned")
            .iter()
            .find(|m| m.send && m.virt == to)
            .map_or(to, |m| m.real)
    }

    fn incoming(&self, from: SocketAddr) -> SocketAddr {
        if !self.any.load(Ordering::Acquire) {
            return from;
        }
        self.map
            .read()
            .expect("poisoned")
            .iter()
            .find(|m| m.recv && m.real == from)
            .map_or(from, |m| m.virt)
    }
}

impl fmt::Debug for DemuxSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DemuxSocket")
            .field("inner", &self.inner)
            .finish()
    }
}

impl DemuxSocket {
    pub fn new(inner: Arc<dyn AsyncUdpSocket>) -> (Arc<Self>, ControlRx) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                inner,
                control: tx,
                paths: Paths::default(),
            }),
            rx,
        )
    }

    pub fn paths(&self) -> Paths {
        self.paths.clone()
    }

    pub fn raw_sender(&self) -> RawSender {
        RawSender {
            sock: self.inner.clone(),
        }
    }
}

impl AsyncUdpSocket for DemuxSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let to = self.paths.outgoing(transmit.destination);
        if to == transmit.destination {
            return self.inner.try_send(transmit);
        }
        self.inner.try_send(&Transmit {
            destination: to,
            ecn: transmit.ecn,
            contents: transmit.contents,
            segment_size: transmit.segment_size,
            src_ip: transmit.src_ip,
        })
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let n = ready!(self.inner.poll_recv(cx, bufs, meta))?;
            let mut kept = 0;
            for i in 0..n {
                let mut m = meta[i];
                let single = m.stride == m.len;
                if single && crate::ctl::is_control(&bufs[i][..m.len]) {
                    let _ = self.control.send((m.addr, bufs[i][..m.len].to_vec()));
                    continue;
                }
                m.addr = self.paths.incoming(m.addr);
                meta[i].addr = m.addr;
                if kept != i {
                    // Compact: move datagram i into slot `kept`.
                    let (lo, hi) = bufs.split_at_mut(i);
                    let len = m.len.min(lo[kept].len());
                    lo[kept][..len].copy_from_slice(&hi[0][..len]);
                    meta[kept] = m;
                }
                kept += 1;
            }
            if kept > 0 {
                return Poll::Ready(Ok(kept));
            }
            // Everything was control traffic; poll the socket again.
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// Sends raw (non-QUIC) datagrams from the shared socket.
#[derive(Clone)]
pub struct RawSender {
    sock: Arc<dyn AsyncUdpSocket>,
}

impl fmt::Debug for RawSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RawSender")
    }
}

impl RawSender {
    pub async fn send(&self, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        let mut poller = self.sock.clone().create_io_poller();
        loop {
            let t = Transmit {
                destination: to,
                ecn: None,
                contents: data,
                segment_size: None,
                src_ip: None,
            };
            match self.sock.try_send(&t) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx)).await?;
                }
                r => return r,
            }
        }
    }
}

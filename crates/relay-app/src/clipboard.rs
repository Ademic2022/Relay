//! Clipboard text sync, both directions (FR-17).
//!
//! - One worker thread owns the OS clipboard (`arboard`) and polls it
//!   every [`POLL`]; there is no portable change notification.
//! - Only copies made *after* the session starts are sent: the clipboard's
//!   contents at connect time never leave the machine.
//! - Text received from the peer is remembered, so it is not echoed back.
//! - Each side sends on its own unidirectional stream (tag
//!   [`Channel::Clipboard`]). The Host only starts sending once the
//!   Controller has opened its stream, so a Controller with clipboard sync
//!   off never receives the Host's clipboard.

use std::sync::mpsc;
use std::time::Duration;

use relay_net::channel::Channel;
use relay_net::quic::{self, Connection, RecvStream};
use relay_proto::v1;
use tokio::sync::mpsc as tmpsc;

pub const POLL: Duration = Duration::from_millis(500);
/// Larger texts are not synced.
pub const MAX_TEXT: usize = 1 << 20;

/// A running sync for one session. Dropping it stops the worker.
pub struct ClipboardSync {
    incoming: mpsc::Sender<String>,
    _writer: tokio::task::JoinHandle<()>,
}

/// Something that can read and write the clipboard (OS or test double).
pub trait Board: Send + 'static {
    fn get(&mut self) -> Option<String>;
    fn set(&mut self, text: &str);
}

struct Os(arboard::Clipboard);

impl Board for Os {
    fn get(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }
    fn set(&mut self, text: &str) {
        if let Err(e) = self.0.set_text(text.to_string()) {
            tracing::debug!("clipboard write: {e}");
        }
    }
}

pub fn os_board() -> Option<Box<dyn Board>> {
    match arboard::Clipboard::new() {
        Ok(c) => Some(Box::new(Os(c))),
        Err(e) => {
            tracing::warn!("clipboard unavailable: {e}");
            None
        }
    }
}

impl ClipboardSync {
    /// Start syncing over `conn`: opens our outgoing stream now.
    pub fn start(conn: &Connection, mut board: Box<dyn Board>) -> Self {
        let (in_tx, in_rx) = mpsc::channel::<String>();
        let (out_tx, mut out_rx) = tmpsc::unbounded_channel::<String>();
        std::thread::Builder::new()
            .name("relay-clipboard".into())
            .spawn(move || {
                // Pre-existing contents are not sent (privacy).
                let mut last = board.get();
                loop {
                    match in_rx.recv_timeout(POLL) {
                        Ok(text) => {
                            board.set(&text);
                            last = Some(text);
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let now = board.get();
                    if now != last {
                        if let Some(t) = now
                            .as_ref()
                            .filter(|t| !t.is_empty() && t.len() <= MAX_TEXT)
                            && out_tx.send(t.clone()).is_err()
                        {
                            return;
                        }
                        last = now;
                    }
                }
            })
            .expect("spawn clipboard thread");

        let conn = conn.clone();
        let writer = tokio::spawn(async move {
            let Ok(mut s) = conn.open_uni().await else {
                return;
            };
            let _ = s.set_priority(Channel::Clipboard.priority());
            if s.write_all(&[Channel::Clipboard as u8]).await.is_err() {
                return;
            }
            while let Some(text) = out_rx.recv().await {
                let msg = v1::ClipboardData {
                    data: Some(v1::clipboard_data::Data::Text(text)),
                };
                if quic::send_msg(&mut s, &msg).await.is_err() {
                    break;
                }
            }
        });
        Self {
            incoming: in_tx,
            _writer: writer,
        }
    }

    /// Feed the peer's clipboard stream (after its tag byte) into ours.
    pub fn receive(&self, mut rx: RecvStream) {
        let tx = self.incoming.clone();
        tokio::spawn(async move {
            while let Ok(Some(m)) =
                quic::recv_msg::<v1::ClipboardData>(&mut rx, MAX_TEXT + 64).await
            {
                if let Some(v1::clipboard_data::Data::Text(t)) = m.data
                    && tx.send(t).is_err()
                {
                    break;
                }
            }
        });
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self._writer.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Option<String>>>);

    impl Board for Fake {
        fn get(&mut self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        fn set(&mut self, t: &str) {
            *self.0.lock().unwrap() = Some(t.to_string());
        }
    }

    async fn pair() -> (Connection, Connection, quic::Node, quic::Node) {
        let (a, b) = (crate::identity::ephemeral(), crate::identity::ephemeral());
        let lo = "127.0.0.1:0".parse().unwrap();
        let (host, _) = quic::node(lo, &a.tls, Some(&[quic::ALPN])).unwrap();
        let (ctl, _) = quic::node(lo, &b.tls, None).unwrap();
        let addr = host.endpoint.local_addr().unwrap();
        let (cfg, _) = quic::client_config(
            &b.tls,
            relay_net::tls::Expect::AnyRecord,
            quic::ALPN,
            Duration::from_secs(2),
        )
        .unwrap();
        let (c, s) = tokio::join!(quic::connect(&ctl.endpoint, cfg, addr), async {
            host.endpoint.accept().await.unwrap().await.unwrap()
        });
        (s, c.unwrap(), host, ctl)
    }

    async fn accept_clipboard(conn: &Connection) -> RecvStream {
        let mut rx = conn.accept_uni().await.unwrap();
        let mut tag = [0u8; 1];
        rx.read_exact(&mut tag).await.unwrap();
        assert_eq!(tag[0], Channel::Clipboard as u8);
        rx
    }

    async fn wait_for(b: &Fake, want: &str) -> bool {
        for _ in 0..60 {
            if b.0.lock().unwrap().as_deref() == Some(want) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn syncs_both_ways_without_leaking_old_contents_or_echoing() {
        let (host_conn, ctl_conn, _h, _c) = pair().await;
        let host_board = Fake(Arc::new(Mutex::new(Some("host secret from before".into()))));
        let ctl_board = Fake::default();

        let ctl = ClipboardSync::start(&ctl_conn, Box::new(ctl_board.clone()));
        let host = ClipboardSync::start(&host_conn, Box::new(host_board.clone()));
        host.receive(accept_clipboard(&host_conn).await);
        ctl.receive(accept_clipboard(&ctl_conn).await);

        tokio::time::sleep(POLL * 2).await;
        assert_eq!(
            *ctl_board.0.lock().unwrap(),
            None,
            "pre-session clipboard not sent"
        );

        *ctl_board.0.lock().unwrap() = Some("copied on the controller".into());
        assert!(
            wait_for(&host_board, "copied on the controller").await,
            "controller → host"
        );

        *host_board.0.lock().unwrap() = Some("copied on the host".into());
        assert!(
            wait_for(&ctl_board, "copied on the host").await,
            "host → controller"
        );

        // No ping-pong: both stay put.
        tokio::time::sleep(POLL * 3).await;
        assert_eq!(
            ctl_board.0.lock().unwrap().as_deref(),
            Some("copied on the host")
        );
        assert_eq!(
            host_board.0.lock().unwrap().as_deref(),
            Some("copied on the host")
        );
    }
}

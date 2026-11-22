//! Text chat during a session (FR-24).
//!
//! Each side sends on its own unidirectional stream (tag
//! [`Channel::Chat`]), reliable and below input in priority. Messages are
//! sanitised (no control characters except newline) and capped.

use relay_net::channel::Channel;
use relay_net::quic::{self, Connection, RecvStream};
use relay_proto::v1;
use tokio::sync::mpsc;

pub const MAX_CHARS: usize = 4000;

/// Drop control characters (except newline) and cap the length.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(MAX_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// "14:05" in local time.
pub fn clock(ms: u64) -> String {
    jiff::Timestamp::from_millisecond(ms as i64)
        .map(|t| {
            t.to_zoned_datetime(jiff::tz::TimeZone::system())
                .strftime("%H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| "--:--".into())
}

/// Our side of a chat: send with [`ChatLink::send`].
pub struct ChatLink {
    out: mpsc::UnboundedSender<String>,
    writer: tokio::task::JoinHandle<()>,
}

impl Drop for ChatLink {
    fn drop(&mut self) {
        self.writer.abort();
    }
}

impl ChatLink {
    /// Open our outgoing chat stream.
    pub fn start(conn: &Connection) -> Self {
        let (out, mut rx) = mpsc::unbounded_channel::<String>();
        let conn = conn.clone();
        let writer = tokio::spawn(async move {
            let Ok(mut s) = conn.open_uni().await else {
                return;
            };
            let _ = s.set_priority(Channel::Chat.priority());
            if s.write_all(&[Channel::Chat as u8]).await.is_err() {
                return;
            }
            let mut id = 0u64;
            while let Some(text) = rx.recv().await {
                id += 1;
                let m = v1::ChatMessage {
                    id,
                    sent_at_ms: now_ms(),
                    text,
                };
                if quic::send_msg(&mut s, &m).await.is_err() {
                    break;
                }
            }
        });
        Self { out, writer }
    }

    /// Queue a message; returns the sanitised text (empty = not sent).
    pub fn send(&self, text: &str) -> String {
        let t = sanitize(text);
        if !t.is_empty() {
            let _ = self.out.send(t.clone());
        }
        t
    }
}

/// Read the peer's chat stream (after its tag byte), delivering messages.
pub fn receive(mut rx: RecvStream, deliver: impl Fn(v1::ChatMessage) -> bool + Send + 'static) {
    tokio::spawn(async move {
        while let Ok(Some(mut m)) =
            quic::recv_msg::<v1::ChatMessage>(&mut rx, MAX_CHARS * 4 + 64).await
        {
            m.text = sanitize(&m.text);
            if !m.text.is_empty() && !deliver(m) {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes() {
        assert_eq!(sanitize("  hi\u{1b}[31m there\u{7}\n "), "hi[31m there");
        assert_eq!(sanitize(&"x".repeat(5000)).len(), MAX_CHARS);
        assert_eq!(clock(0).len(), 5);
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn messages_flow_both_ways() {
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
        let (c, s) = (c.unwrap(), s);

        let (to_host, from_ctl) = (ChatLink::start(&c), ChatLink::start(&s));
        let (htx, mut hrx) = mpsc::unbounded_channel();
        let (ctx, mut crx) = mpsc::unbounded_channel();
        for (conn, tx) in [(s.clone(), htx), (c.clone(), ctx)] {
            tokio::spawn(async move {
                let mut rx = conn.accept_uni().await.unwrap();
                let mut tag = [0u8; 1];
                rx.read_exact(&mut tag).await.unwrap();
                assert_eq!(tag[0], Channel::Chat as u8);
                receive(rx, move |m| tx.send(m).is_ok());
            });
        }
        to_host.send("hello host \u{1b}[2J");
        from_ctl.send("hi viewer");
        let at_host = tokio::time::timeout(Duration::from_secs(3), hrx.recv())
            .await
            .unwrap()
            .unwrap();
        let at_ctl = tokio::time::timeout(Duration::from_secs(3), crx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            at_host.text, "hello host [2J",
            "escape sequences neutralised"
        );
        assert_eq!(at_ctl.text, "hi viewer");
        assert!(at_host.sent_at_ms > 0);
        assert_eq!(to_host.send("   "), "", "blank lines are not sent");
    }
}

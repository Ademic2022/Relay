//! Host input path (§6 rule 8): a dedicated real-time thread injects
//! events as they arrive, never queued behind video work.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use relay_input::{DisplayTarget, InputInjector};
use relay_net::channel::Channel;
use relay_net::quic::{self, Connection};
use relay_proto::v1;

use crate::input_wire;

/// Upper bound on one input message.
const MAX_INPUT_MSG: usize = 4096;

enum Msg {
    Event(relay_input::InputEvent, Instant),
}

/// Accept the Controller's streams for this session and route input to an
/// injector thread. Returns when the connection closes.
pub async fn serve(
    conn: Connection,
    targets: tokio::sync::watch::Receiver<DisplayTarget>,
    control_allowed: bool,
    clipboard_allowed: bool,
    peer_name: String,
) {
    let initial = *targets.borrow();
    serve_streams(
        conn,
        move || platform(initial, control_allowed),
        Some(targets),
        clipboard_allowed,
        peer_name,
    )
    .await;
}

/// Injector selection for the real host.
fn platform(target: DisplayTarget, control_allowed: bool) -> Option<Box<dyn InputInjector>> {
    if !control_allowed {
        return None;
    }
    match relay_input::platform_injector() {
        Ok(mut i) => {
            i.set_display(target);
            Some(i)
        }
        Err(relay_input::InputError::PermissionDenied) => {
            // Show the system prompt once per process, not every session.
            static PROMPTED: std::sync::Once = std::sync::Once::new();
            PROMPTED.call_once(|| {
                #[cfg(target_os = "macos")]
                relay_input::macos::accessibility_trusted(true);
            });
            println!(
                "  ! Remote control is off: grant Accessibility access to your terminal\n    (System Settings → Privacy & Security → Accessibility), then restart the host."
            );
            None
        }
        Err(e) => {
            tracing::warn!("remote control unavailable: {e}");
            None
        }
    }
}

#[cfg(test)]
pub async fn serve_with<F>(
    conn: Connection,
    make_injector: F,
    targets: Option<tokio::sync::watch::Receiver<DisplayTarget>>,
) where
    F: FnOnce() -> Option<Box<dyn InputInjector>> + Send + 'static,
{
    serve_streams(conn, make_injector, targets, false, String::new()).await;
}

/// Route the Controller's streams: input to the injector thread,
/// clipboard to a [`crate::clipboard::ClipboardSync`] (started only when
/// the Controller opens its clipboard stream, FR-17).
async fn serve_streams<F>(
    conn: Connection,
    make_injector: F,
    targets: Option<tokio::sync::watch::Receiver<DisplayTarget>>,
    clipboard_allowed: bool,
    peer_name: String,
) where
    F: FnOnce() -> Option<Box<dyn InputInjector>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Msg>();
    let injector = std::thread::Builder::new()
        .name("relay-input".into())
        .spawn(move || inject_loop(rx, make_injector(), targets));
    if let Err(e) = injector {
        tracing::error!("input thread: {e}");
        return;
    }

    let mut clipboard: Option<crate::clipboard::ClipboardSync> = None;
    while let Ok(mut rx) = conn.accept_uni().await {
        let mut tag = [0u8; 1];
        if rx.read_exact(&mut tag).await.is_err() {
            continue;
        }
        match Channel::from_u8(tag[0]) {
            Some(Channel::Input) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    while let Ok(Some(m)) =
                        quic::recv_msg::<v1::InputEvent>(&mut rx, MAX_INPUT_MSG).await
                    {
                        let now = Instant::now();
                        if let Some(ev) = input_wire::from_proto(&m)
                            && tx.send(Msg::Event(ev, now)).is_err()
                        {
                            break;
                        }
                    }
                });
            }
            Some(Channel::Clipboard) if clipboard_allowed => {
                if clipboard.is_none()
                    && let Some(board) = crate::clipboard::os_board()
                {
                    clipboard = Some(crate::clipboard::ClipboardSync::start(&conn, board));
                }
                if let Some(c) = &clipboard {
                    c.receive(rx);
                }
            }
            Some(Channel::Chat) => {
                // FR-24, host side: the terminal until the Tauri UI.
                let who = peer_name.clone();
                crate::chat::receive(rx, move |m| {
                    println!(
                        "  💬 {who} {}: {}",
                        crate::chat::clock(m.sent_at_ms),
                        m.text
                    );
                    crate::ui::emit(
                        "chat",
                        serde_json::json!({ "from_me": false, "name": who, "text": m.text }),
                    );
                    true
                });
            }
            other => tracing::debug!("ignoring stream {other:?}"),
        }
    }
    // Dropping `tx` ends the injector, which releases held keys/buttons.
}

fn inject_loop(
    rx: mpsc::Receiver<Msg>,
    mut inj: Option<Box<dyn InputInjector>>,
    mut targets: Option<tokio::sync::watch::Receiver<DisplayTarget>>,
) {
    crate::rt::set_realtime();
    let mut stats = (0u32, Duration::ZERO, Duration::ZERO, Instant::now());
    while let Ok(Msg::Event(ev, received)) = rx.recv() {
        let Some(i) = inj.as_mut() else { continue };
        // The Controller switched monitors: map pointer moves onto it.
        if let Some(t) = targets.as_mut()
            && t.has_changed().unwrap_or(false)
        {
            i.set_display(*t.borrow_and_update());
        }
        let _s = tracing::trace_span!("inject").entered();
        if let Err(e) = i.inject(&ev) {
            // FR-40: never log what was typed or clicked, only that it failed.
            tracing::debug!("input injection failed: {e}");
        }
        let d = received.elapsed();
        stats.0 += 1;
        stats.1 += d;
        stats.2 = stats.2.max(d);
        if stats.3.elapsed() >= Duration::from_secs(5) {
            tracing::info!(
                "input: {} events, receive→injected avg {:.2} ms, max {:.2} ms",
                stats.0,
                stats.1.as_secs_f64() * 1000.0 / f64::from(stats.0),
                stats.2.as_secs_f64() * 1000.0
            );
            stats = (0, Duration::ZERO, Duration::ZERO, Instant::now());
        }
    }
    if let Some(i) = inj.as_mut() {
        let _ = i.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_input::{InputError, InputEvent, Modifiers, MouseButton};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recorder {
        events: Arc<Mutex<Vec<InputEvent>>>,
        released: Arc<Mutex<bool>>,
    }

    impl InputInjector for Recorder {
        fn set_display(&mut self, _t: DisplayTarget) {}
        fn inject(&mut self, e: &InputEvent) -> Result<(), InputError> {
            self.events.lock().unwrap().push(e.clone());
            Ok(())
        }
        fn release_all(&mut self) -> Result<(), InputError> {
            *self.released.lock().unwrap() = true;
            Ok(())
        }
    }

    #[tokio::test]
    async fn events_arrive_in_order_and_release_on_disconnect() {
        let (hid, cid) = (crate::identity::ephemeral(), crate::identity::ephemeral());
        let (sep, _) = quic::node(
            "127.0.0.1:0".parse().unwrap(),
            &hid.tls,
            Some(&[quic::ALPN]),
        )
        .unwrap();
        let addr = sep.endpoint.local_addr().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let released = Arc::new(Mutex::new(false));
        let (e2, r2) = (events.clone(), released.clone());
        let host = tokio::spawn(async move {
            let conn = sep.endpoint.accept().await.unwrap().await.unwrap();
            serve_with(
                conn,
                move || {
                    Some(Box::new(Recorder {
                        events: e2,
                        released: r2,
                    }) as Box<dyn InputInjector>)
                },
                None,
            )
            .await;
        });

        let (ep, _) = quic::node("127.0.0.1:0".parse().unwrap(), &cid.tls, None).unwrap();
        let (cfg, _) = quic::client_config(
            &cid.tls,
            relay_net::tls::Expect::Key(hid.public_key()),
            quic::ALPN,
            Duration::from_secs(2),
        )
        .unwrap();
        let conn = quic::connect(&ep.endpoint, cfg, addr).await.unwrap();
        let mut s = conn.open_uni().await.unwrap();
        s.write_all(&[Channel::Input as u8]).await.unwrap();
        let sent = vec![
            InputEvent::MouseMove { x: 0.5, y: 0.5 },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
            },
            InputEvent::MouseMove { x: 0.6, y: 0.5 },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: false,
            },
            InputEvent::Key {
                usage: 0x04,
                down: true,
                modifiers: Modifiers::default(),
            },
        ];
        for e in &sent {
            quic::send_msg(&mut s, &input_wire::to_proto(e, 0))
                .await
                .unwrap();
        }
        // Wait until everything is injected.
        for _ in 0..200 {
            if events.lock().unwrap().len() == sent.len() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(*events.lock().unwrap(), sent);
        conn.close(0u32.into(), b"bye");
        tokio::time::timeout(Duration::from_secs(5), host)
            .await
            .unwrap()
            .unwrap();
        for _ in 0..200 {
            if *released.lock().unwrap() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            *released.lock().unwrap(),
            "held keys released on disconnect"
        );
    }
}

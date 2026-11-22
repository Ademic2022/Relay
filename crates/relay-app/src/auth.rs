//! Password authentication on the control stream (FR-3, §8.2 step 5).
//!
//! `Hello` (Controller) → `Auth{1, KE1}` → `Auth{2, KE2}` →
//! `Auth{3, KE3}` → `AuthOutcome`. OPAQUE is bound to both verified device
//! keys and the TLS exporter of this QUIC connection.

use anyhow::{Context, bail};
use relay_crypto::pake::{ControllerLogin, HostLogin, HostPassword, PakeError, Parties};
use relay_net::quic::{self, Connection, RecvStream, SendStream};
use relay_proto::v1::{self, control_message::Msg};
use relay_proto::{MAX_MESSAGE_LEN, PROTOCOL_VERSION};

const MAX_AUTH_MSG: usize = 4096;
/// How long the Host waits for a TOTP code to be typed (FR-36).
pub const CODE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Asks the person at the Controller for a TOTP code (FR-36). Runs on a
/// blocking thread: it may read the terminal or the app's pipe.
#[derive(Clone)]
pub struct AskCode(pub std::sync::Arc<dyn Fn() -> anyhow::Result<String> + Send + Sync>);

impl std::fmt::Debug for AskCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AskCode")
    }
}

/// The host refused us (wrong password, declined, locked out, not on the
/// allow-list). Retrying with the same password cannot help, so the
/// viewer's reconnect loop stops on this (and does not feed the host's
/// brute-force counter).
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

pub fn control(msg: Msg) -> v1::ControlMessage {
    v1::ControlMessage { msg: Some(msg) }
}

fn auth_msg(step: u32, payload: Vec<u8>) -> v1::ControlMessage {
    auth_msg_kind(step, payload, v1::PasswordKind::OneTime)
}

fn auth_msg_kind(step: u32, payload: Vec<u8>, kind: v1::PasswordKind) -> v1::ControlMessage {
    control(Msg::Auth(v1::AuthMessage {
        step,
        payload: payload.into(),
        password_kind: kind as i32,
    }))
}

/// One-time passwords are 8 characters; permanent ones 12+ (FR-2, FR-31).
pub fn kind_of(password: &str) -> v1::PasswordKind {
    if password.chars().count() >= relay_crypto::password::PERMANENT_MIN_LEN {
        v1::PasswordKind::Permanent
    } else {
        v1::PasswordKind::OneTime
    }
}

pub fn local_os() -> v1::OsKind {
    if cfg!(target_os = "macos") {
        v1::OsKind::Macos
    } else {
        v1::OsKind::Windows
    }
}

/// What the Controller logs in with.
#[derive(Debug, Clone, Copy)]
pub struct Login<'a> {
    pub password: &'a str,
    /// Asked if the Host wants a two-factor code (FR-36).
    pub ask_code: Option<&'a AskCode>,
    /// Non-empty: a wake request, not a session (FR-37).
    pub wake_macs: &'a [crate::wol::Mac],
}

impl<'a> Login<'a> {
    pub fn password(password: &'a str) -> Self {
        Self {
            password,
            ask_code: None,
            wake_macs: &[],
        }
    }
}

/// Controller side. Returns the session key on success.
pub async fn controller(
    conn: &Connection,
    tx: &mut SendStream,
    rx: &mut RecvStream,
    me: &crate::identity::Me,
    device_name: &str,
    login: Login<'_>,
) -> anyhow::Result<[u8; 32]> {
    let Login {
        password,
        ask_code,
        wake_macs,
    } = login;
    let host_key = relay_net::tls::peer_public_key(conn).context("host presented no device key")?;
    let exporter = quic::exporter(conn)?;
    let my_key = me.public_key();
    quic::send_msg(
        tx,
        &control(Msg::Hello(v1::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_pubkey: my_key.to_vec().into(),
            device_id: me.key.device_id(0).as_u32(),
            device_name: device_name.to_string(),
            os: local_os() as i32,
            capabilities: Some(v1::Capabilities {
                decoders: vec![v1::Codec::H264 as i32],
                ..Default::default()
            }),
            wake_macs: wake_macs.iter().map(|m| m.to_vec().into()).collect(),
        })),
    )
    .await?;
    let (pake, ke1) = ControllerLogin::start(password)?;
    quic::send_msg(tx, &auth_msg_kind(1, ke1, kind_of(password))).await?;

    let ke2 = match quic::recv_msg::<v1::ControlMessage>(rx, MAX_MESSAGE_LEN).await? {
        Some(v1::ControlMessage {
            msg: Some(Msg::Auth(a)),
        }) if a.step == 2 => a.payload,
        Some(v1::ControlMessage {
            msg: Some(Msg::AuthOutcome(o)),
        }) => return Err(outcome_error(&o)),
        _ => bail!("host closed the connection during authentication"),
    };
    let parties = Parties {
        controller_pubkey: &my_key,
        host_pubkey: &host_key,
    };
    let (ke3, key) = match pake.finish(password, &ke2, parties, &exporter) {
        Ok(v) => v,
        Err(PakeError::BadPassword) => {
            // Tell the host so it counts the failure (FR-35).
            let _ = quic::send_msg(tx, &auth_msg(3, Vec::new())).await;
            return Err(Refused("wrong password".into()).into());
        }
        Err(e) => bail!("authentication failed: {e}"),
    };
    quic::send_msg(tx, &auth_msg(3, ke3)).await?;
    let mut reply = quic::recv_msg::<v1::ControlMessage>(rx, MAX_MESSAGE_LEN).await?;
    // FR-36: the Host wants a TOTP code as well.
    if let Some(v1::ControlMessage {
        msg: Some(Msg::Auth(a)),
    }) = &reply
        && a.step == 4
    {
        let Some(ask) = ask_code.cloned() else {
            return Err(Refused("this computer also needs a two-factor code".into()).into());
        };
        let code = tokio::task::spawn_blocking(move || (ask.0)()).await??;
        let code: String = code.chars().filter(char::is_ascii_digit).collect();
        quic::send_msg(tx, &auth_msg(5, code.into_bytes())).await?;
        reply = quic::recv_msg::<v1::ControlMessage>(rx, MAX_MESSAGE_LEN).await?;
    }
    match reply {
        Some(v1::ControlMessage {
            msg: Some(Msg::AuthOutcome(o)),
        }) if o.result == v1::AuthResult::Ok as i32 => Ok(key),
        Some(v1::ControlMessage {
            msg: Some(Msg::AuthOutcome(o)),
        }) => Err(outcome_error(&o)),
        _ => bail!("host closed the connection during authentication"),
    }
}

fn outcome_error(o: &v1::AuthOutcome) -> anyhow::Error {
    let msg = match v1::AuthResult::try_from(o.result).unwrap_or(v1::AuthResult::Unspecified) {
        v1::AuthResult::BadPassword => "wrong password".to_string(),
        v1::AuthResult::LockedOut => format!(
            "too many attempts; try again in {} s",
            o.retry_after_ms.div_ceil(1000)
        ),
        v1::AuthResult::Declined => "the host declined the connection".into(),
        v1::AuthResult::NotWhitelisted => "this device is not allowed to connect".into(),
        v1::AuthResult::BadCode => "wrong two-factor code".into(),
        other => format!("host refused the connection: {other:?}"),
    };
    Refused(msg).into()
}

/// What the host learned about the controller.
#[derive(Debug, Clone)]
pub struct Peer {
    pub key: [u8; 32],
    pub id: Option<relay_crypto::DeviceId>,
    pub name: String,
    /// A wake-only request (FR-37): acted on only after authentication.
    pub wake_macs: Vec<crate::wol::Mac>,
}

/// Host side, step 1: read `Hello` and check it matches the TLS identity.
pub async fn host_hello(conn: &Connection, rx: &mut RecvStream) -> anyhow::Result<Peer> {
    let tls_key =
        relay_net::tls::peer_public_key(conn).context("controller presented no device key")?;
    let hello = match quic::recv_msg::<v1::ControlMessage>(rx, MAX_MESSAGE_LEN).await? {
        Some(v1::ControlMessage {
            msg: Some(Msg::Hello(h)),
        }) => h,
        _ => bail!("expected Hello"),
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        bail!("protocol version {} unsupported", hello.protocol_version);
    }
    if hello.device_pubkey.as_ref() != tls_key {
        bail!("Hello key does not match the TLS identity");
    }
    // Only show an ID we can verify belongs to this key.
    let id = relay_crypto::DeviceId::from_u32(hello.device_id)
        .filter(|id| (0..8).any(|n| relay_crypto::DeviceId::derive(&tls_key, n) == *id));
    let name: String = hello
        .device_name
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    Ok(Peer {
        key: tls_key,
        id,
        name,
        wake_macs: crate::wol::from_wire(&hello.wake_macs),
    })
}

/// Result of the password exchange on the host.
pub enum HostAuth {
    Ok(v1::PasswordKind),
    BadPassword,
    /// No password of the kind the Controller used is set on this Host.
    NotAccepted(v1::PasswordKind),
}

/// Host side, step 2: run OPAQUE against the verifier `pick` returns for
/// the kind of password the Controller says it is using.
pub async fn host_login<'a>(
    conn: &Connection,
    tx: &mut SendStream,
    rx: &mut RecvStream,
    me: &crate::identity::Me,
    peer: &Peer,
    pick: impl FnOnce(v1::PasswordKind) -> Option<&'a HostPassword>,
) -> anyhow::Result<HostAuth> {
    let exporter = quic::exporter(conn)?;
    let my_key = me.public_key();
    let parties = Parties {
        controller_pubkey: &peer.key,
        host_pubkey: &my_key,
    };
    let (ke1, kind) = match quic::recv_msg::<v1::ControlMessage>(rx, MAX_AUTH_MSG).await? {
        Some(v1::ControlMessage {
            msg: Some(Msg::Auth(a)),
        }) if a.step == 1 => {
            let kind =
                v1::PasswordKind::try_from(a.password_kind).unwrap_or(v1::PasswordKind::OneTime);
            (a.payload, kind)
        }
        _ => bail!("expected KE1"),
    };
    let Some(password) = pick(kind) else {
        return Ok(HostAuth::NotAccepted(kind));
    };
    let (login, ke2): (HostLogin, Vec<u8>) = password.start_login(&ke1, parties, &exporter)?;
    quic::send_msg(tx, &auth_msg(2, ke2)).await?;
    let ke3 = match quic::recv_msg::<v1::ControlMessage>(rx, MAX_AUTH_MSG).await? {
        Some(v1::ControlMessage {
            msg: Some(Msg::Auth(a)),
        }) if a.step == 3 => a.payload,
        _ => bail!("expected KE3"),
    };
    Ok(match login.finish(&ke3, parties, &exporter) {
        Ok(_) => HostAuth::Ok(kind),
        Err(_) => HostAuth::BadPassword,
    })
}

/// Host side, step 3 (FR-36): ask for the TOTP code and return what the
/// Controller typed. Only called after the password passed.
pub async fn host_code(tx: &mut SendStream, rx: &mut RecvStream) -> anyhow::Result<String> {
    quic::send_msg(tx, &auth_msg(4, Vec::new())).await?;
    let msg = tokio::time::timeout(CODE_WAIT, quic::recv_msg::<v1::ControlMessage>(rx, 64))
        .await
        .context("no two-factor code in time")??;
    match msg {
        Some(v1::ControlMessage {
            msg: Some(Msg::Auth(a)),
        }) if a.step == 5 => Ok(String::from_utf8_lossy(&a.payload).into_owned()),
        _ => bail!("expected a two-factor code"),
    }
}

pub async fn send_outcome(
    tx: &mut SendStream,
    result: v1::AuthResult,
    retry_after_ms: u32,
) -> anyhow::Result<()> {
    quic::send_msg(
        tx,
        &control(Msg::AuthOutcome(v1::AuthOutcome {
            result: result as i32,
            retry_after_ms,
        })),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_crypto::pake::PasswordKind;
    use relay_crypto::totp::Secret;
    use std::sync::Arc;
    use std::time::Duration;

    const PW: &str = "correct-horse-battery-staple";

    /// Host that wants the permanent password and then a TOTP code; sends
    /// `Ok` only when both pass.
    async fn host(secret: Secret) -> (SocketAddr, [u8; 32]) {
        let me = Arc::new(crate::identity::ephemeral());
        let (node, _) =
            quic::node("127.0.0.1:0".parse().unwrap(), &me.tls, Some(&[quic::ALPN])).unwrap();
        let addr = node.endpoint.local_addr().unwrap();
        let key = me.public_key();
        tokio::spawn(async move {
            let verifier = HostPassword::new(PW, PasswordKind::Permanent).unwrap();
            let mut last = None;
            while let Some(i) = node.endpoint.accept().await {
                let Ok(conn) = i.await else { continue };
                let (mut tx, mut rx) = conn.accept_bi().await.unwrap();
                let peer = host_hello(&conn, &mut rx).await.unwrap();
                let ok = matches!(
                    host_login(&conn, &mut tx, &mut rx, &me, &peer, |_| Some(&verifier)).await,
                    Ok(HostAuth::Ok(_))
                );
                let result = if !ok {
                    v1::AuthResult::BadPassword
                } else {
                    match host_code(&mut tx, &mut rx).await {
                        Ok(c) => match secret.verify(&c, crate::unattended::unix_secs(), last) {
                            Some(step) => {
                                last = Some(step);
                                v1::AuthResult::Ok
                            }
                            None => v1::AuthResult::BadCode,
                        },
                        Err(_) => continue,
                    }
                };
                let _ = send_outcome(&mut tx, result, 0).await;
                let _ = tx.finish();
                let _ = conn.closed().await;
            }
        });
        (addr, key)
    }

    async fn login(
        addr: SocketAddr,
        host_key: [u8; 32],
        ask: Option<AskCode>,
    ) -> anyhow::Result<[u8; 32]> {
        let me = crate::identity::ephemeral();
        let (node, _) = quic::node("127.0.0.1:0".parse().unwrap(), &me.tls, None)?;
        let (cfg, _) = quic::client_config(
            &me.tls,
            relay_net::tls::Expect::Key(host_key),
            quic::ALPN,
            Duration::from_secs(2),
        )?;
        let conn = quic::connect(&node.endpoint, cfg, addr).await?;
        let (mut tx, mut rx) = conn.open_bi().await?;
        let login = Login {
            ask_code: ask.as_ref(),
            ..Login::password(PW)
        };
        let r = controller(&conn, &mut tx, &mut rx, &me, "test", login).await;
        conn.close(0u32.into(), b"done");
        r
    }

    fn answer(code: String) -> Option<AskCode> {
        Some(AskCode(Arc::new(move || Ok(code.clone()))))
    }

    use std::net::SocketAddr;

    #[tokio::test]
    async fn totp_second_factor() {
        let secret = Secret::generate().unwrap();
        let (addr, key) = host(secret.clone()).await;
        let now = crate::unattended::unix_secs();
        let good = format!("{:06}", secret.code(now / 30));
        // A code no step in the window produces.
        let window: Vec<u32> = (now / 30 - 1..=now / 30 + 1)
            .map(|t| secret.code(t))
            .collect();
        let wrong = (0..).find(|c| !window.contains(c)).unwrap();
        let refused = |r: anyhow::Result<[u8; 32]>| {
            r.unwrap_err()
                .downcast::<Refused>()
                .map(|e| e.0)
                .unwrap_or_else(|e| format!("not a refusal: {e:#}"))
        };

        assert_eq!(
            refused(login(addr, key, None).await),
            "this computer also needs a two-factor code"
        );
        assert_eq!(
            refused(login(addr, key, answer(format!("{wrong:06}"))).await),
            "wrong two-factor code"
        );
        login(addr, key, answer(good.clone()))
            .await
            .expect("password + code accepted");
        assert_eq!(
            refused(login(addr, key, answer(good)).await),
            "wrong two-factor code",
            "a code works only once"
        );
    }
}

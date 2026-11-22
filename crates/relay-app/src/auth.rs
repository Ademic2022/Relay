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

pub fn control(msg: Msg) -> v1::ControlMessage {
    v1::ControlMessage { msg: Some(msg) }
}

fn auth_msg(step: u32, payload: Vec<u8>) -> v1::ControlMessage {
    control(Msg::Auth(v1::AuthMessage {
        step,
        payload: payload.into(),
    }))
}

pub fn local_os() -> v1::OsKind {
    if cfg!(target_os = "macos") {
        v1::OsKind::Macos
    } else {
        v1::OsKind::Windows
    }
}

/// Controller side. Returns the session key on success.
pub async fn controller(
    conn: &Connection,
    tx: &mut SendStream,
    rx: &mut RecvStream,
    me: &crate::identity::Me,
    device_name: &str,
    password: &str,
) -> anyhow::Result<[u8; 32]> {
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
        })),
    )
    .await?;
    let (login, ke1) = ControllerLogin::start(password)?;
    quic::send_msg(tx, &auth_msg(1, ke1)).await?;

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
    let (ke3, key) = match login.finish(password, &ke2, parties, &exporter) {
        Ok(v) => v,
        Err(PakeError::BadPassword) => {
            // Tell the host so it counts the failure (FR-35).
            let _ = quic::send_msg(tx, &auth_msg(3, Vec::new())).await;
            bail!("wrong password");
        }
        Err(e) => bail!("authentication failed: {e}"),
    };
    quic::send_msg(tx, &auth_msg(3, ke3)).await?;
    match quic::recv_msg::<v1::ControlMessage>(rx, MAX_MESSAGE_LEN).await? {
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
    match v1::AuthResult::try_from(o.result).unwrap_or(v1::AuthResult::Unspecified) {
        v1::AuthResult::BadPassword => anyhow::anyhow!("wrong password"),
        v1::AuthResult::LockedOut => anyhow::anyhow!(
            "too many attempts; try again in {} s",
            o.retry_after_ms.div_ceil(1000)
        ),
        v1::AuthResult::Declined => anyhow::anyhow!("the host declined the connection"),
        v1::AuthResult::NotWhitelisted => anyhow::anyhow!("this device is not allowed to connect"),
        other => anyhow::anyhow!("host refused the connection: {other:?}"),
    }
}

/// What the host learned about the controller.
#[derive(Debug, Clone)]
pub struct Peer {
    pub key: [u8; 32],
    pub id: Option<relay_crypto::DeviceId>,
    pub name: String,
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
    })
}

/// Result of the password exchange on the host.
pub enum HostAuth {
    Ok,
    BadPassword,
}

/// Host side, step 2: run OPAQUE against `password`.
pub async fn host_login(
    conn: &Connection,
    tx: &mut SendStream,
    rx: &mut RecvStream,
    me: &crate::identity::Me,
    peer: &Peer,
    password: &HostPassword,
) -> anyhow::Result<HostAuth> {
    let exporter = quic::exporter(conn)?;
    let my_key = me.public_key();
    let parties = Parties {
        controller_pubkey: &peer.key,
        host_pubkey: &my_key,
    };
    let ke1 = match quic::recv_msg::<v1::ControlMessage>(rx, MAX_AUTH_MSG).await? {
        Some(v1::ControlMessage {
            msg: Some(Msg::Auth(a)),
        }) if a.step == 1 => a.payload,
        _ => bail!("expected KE1"),
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
        Ok(_) => HostAuth::Ok,
        Err(_) => HostAuth::BadPassword,
    })
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

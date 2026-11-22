//! Wake-on-LAN through another Relay device (FR-37).
//!
//! A sleeping computer cannot answer, and a magic packet only travels
//! inside its own LAN. So:
//!
//! 1. During every session a Host tells the Controller its MAC addresses
//!    (`HostDetails`); the Controller remembers them per device.
//! 2. `relay wake <ID> --via <other device>` connects to another Relay Host
//!    on the same LAN, logs in with *that* Host's permanent password, and
//!    asks it to send the magic packets (`Hello.wake_macs`).
//!
//! The relaying Host only does this for unattended-access logins (the
//! owner's own machines, FR-31), after the password (and TOTP, allow-list)
//! pass, and records it in its audit log. It never starts a session for a
//! wake request.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

pub type Mac = [u8; 6];

/// Most MACs carried per device / per request.
pub const MAX_MACS: usize = 4;

/// The 102-byte magic packet: 6 × 0xFF, then the MAC 16 times.
pub fn magic_packet(mac: &Mac) -> [u8; 102] {
    let mut p = [0xFFu8; 102];
    for i in 0..16 {
        p[6 + i * 6..12 + i * 6].copy_from_slice(mac);
    }
    p
}

/// Broadcast magic packets for `macs` on this LAN (UDP 9 and 7, three
/// times each: they are cheap and UDP may drop one). Returns how many
/// packets went out.
pub fn send(macs: &[Mac]) -> std::io::Result<u32> {
    let s = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?;
    s.set_broadcast(true)?;
    let mut sent = 0;
    for _ in 0..3 {
        for mac in macs {
            let p = magic_packet(mac);
            for port in [9, 7] {
                if s.send_to(&p, (Ipv4Addr::BROADCAST, port)).is_ok() {
                    sent += 1;
                }
            }
        }
    }
    if sent == 0 {
        return Err(std::io::Error::other("could not broadcast on this network"));
    }
    Ok(sent)
}

/// A real, globally assigned MAC (not zero, not multicast, not a
/// locally administered one such as a VM bridge or a randomised address).
fn usable(m: &Mac) -> bool {
    *m != [0; 6] && m[0] & 0x01 == 0 && m[0] & 0x02 == 0
}

/// This computer's MACs, for `HostDetails`.
pub fn local_macs() -> Vec<Mac> {
    let mut out = Vec::new();
    if let Ok(it) = mac_address::MacAddressIterator::new() {
        for m in it {
            let b = m.bytes();
            if usable(&b) && !out.contains(&b) {
                out.push(b);
            }
        }
    }
    out.truncate(MAX_MACS);
    out
}

pub fn format(m: &Mac) -> String {
    m.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn parse(s: &str) -> Option<Mac> {
    let parts: Vec<&str> = s.trim().split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut m = [0u8; 6];
    for (o, p) in m.iter_mut().zip(parts) {
        if p.len() != 2 {
            return None;
        }
        *o = u8::from_str_radix(p, 16).ok()?;
    }
    Some(m)
}

/// Keep at most [`MAX_MACS`] well-formed MACs from the wire.
pub fn from_wire<B: AsRef<[u8]>>(v: &[B]) -> Vec<Mac> {
    v.iter()
        .filter_map(|b| Mac::try_from(b.as_ref()).ok())
        .take(MAX_MACS)
        .collect()
}

/// Controller side: log in to `via` and ask it to wake `macs`. Returns
/// how many packets it sent.
pub async fn wake_via(
    via: &crate::viewer::Target,
    password: &str,
    ask_code: crate::auth::AskCode,
    macs: &[Mac],
) -> anyhow::Result<u32> {
    use anyhow::{Context, bail};
    use relay_net::quic;
    use relay_proto::v1::{self, control_message::Msg};

    let me = crate::identity::load()?;
    let est = crate::viewer::establish(&me, via, false).await?;
    let (mut tx, mut rx) = est.conn.open_bi().await?;
    let name = crate::host::hostname();
    crate::auth::controller(
        &est.conn,
        &mut tx,
        &mut rx,
        &me,
        &name,
        crate::auth::Login {
            password,
            ask_code: Some(&ask_code),
            wake_macs: macs,
        },
    )
    .await
    .map_err(|e| match e.downcast_ref::<crate::auth::Refused>() {
        Some(r) if r.0.contains("declined") => anyhow::anyhow!(
            "{via} only passes on wake requests when unattended access is set up there \
             (relay unattended setup)"
        ),
        _ => e,
    })?;
    let r = quic::recv_msg::<v1::ControlMessage>(&mut rx, relay_proto::MAX_MESSAGE_LEN)
        .await?
        .context("the device closed the connection")?;
    est.conn.close(0u32.into(), b"done");
    match r.msg {
        Some(Msg::WakeResult(w)) if w.error.is_empty() => Ok(w.packets_sent),
        Some(Msg::WakeResult(w)) => bail!("{via} could not send the wake packets: {}", w.error),
        _ => bail!("{via} does not support waking other devices (update Relay there)"),
    }
}

// ------------------------------------------------------------ known MACs

fn store_path() -> Option<std::path::PathBuf> {
    if std::env::var("RELAY_KEYSTORE").as_deref() == Ok("memory") {
        return None;
    }
    Some(crate::pins::config_dir()?.join("relay").join("wake"))
}

fn load_from(text: &str) -> Vec<(String, Vec<Mac>)> {
    text.lines()
        .filter_map(|l| {
            let (name, macs) = l.split_once(' ')?;
            let macs: Vec<Mac> = macs.split(',').filter_map(parse).collect();
            (!macs.is_empty()).then(|| (name.to_string(), macs))
        })
        .collect()
}

/// MACs remembered for `device` (a Device ID or address, as pinned).
pub fn known(device: &str) -> Vec<Mac> {
    let Some(p) = store_path() else {
        return Vec::new();
    };
    let text = std::fs::read_to_string(p).unwrap_or_default();
    load_from(&text)
        .into_iter()
        .find(|(n, _)| n == device)
        .map(|(_, m)| m)
        .unwrap_or_default()
}

/// Remember `macs` for `device` (from its `HostDetails`).
pub fn remember(device: &str, macs: &[Mac]) {
    let Some(p) = store_path() else { return };
    if macs.is_empty() || device.contains(char::is_whitespace) {
        return;
    }
    let mut all = load_from(&std::fs::read_to_string(&p).unwrap_or_default());
    all.retain(|(n, _)| n != device);
    all.push((device.to_string(), macs.to_vec()));
    let text: String = all
        .iter()
        .map(|(n, m)| {
            format!(
                "{n} {}\n",
                m.iter().map(format).collect::<Vec<_>>().join(",")
            )
        })
        .collect();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_and_parsing() {
        let mac = [0x00, 0x11, 0x22, 0xaa, 0xbb, 0xcc];
        let p = magic_packet(&mac);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert!(p[6..].chunks(6).all(|c| c == mac));
        assert_eq!(format(&mac), "00:11:22:aa:bb:cc");
        assert_eq!(parse("00-11-22-AA-BB-CC"), Some(mac));
        assert_eq!(parse("00:11:22:aa:bb"), None);
        assert_eq!(parse("00:11:22:aa:bb:zz"), None);
        assert!(usable(&mac));
        assert!(!usable(&[0x02, 0, 0, 0, 0, 1]), "locally administered");
        assert!(!usable(&[0x01, 0, 0x5e, 0, 0, 1]), "multicast");
        let store = load_from("123456789 00:11:22:aa:bb:cc,00:11:22:aa:bb:cd\nbad line\n");
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].1.len(), 2);
    }

    #[test]
    fn this_machine_has_a_mac() {
        let m = local_macs();
        assert!(m.len() <= MAX_MACS);
        assert!(m.iter().all(usable));
    }

    #[test]
    fn magic_packet_is_received() {
        // A listener on the discard port would need root; check the
        // payload over loopback on a free port instead.
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mac = [0x00, 0x16, 0x3e, 1, 2, 3];
        tx.send_to(&magic_packet(&mac), rx.local_addr().unwrap())
            .unwrap();
        let mut b = [0u8; 200];
        let n = rx.recv(&mut b).unwrap();
        assert_eq!(n, 102);
        assert_eq!(&b[96..102], &mac);
    }
}

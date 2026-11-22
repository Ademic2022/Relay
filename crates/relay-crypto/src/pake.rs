//! OPAQUE password authentication (FR-3, §8.2 step 5).
//!
//! The password never crosses the network, and a transcript gives an
//! observer (rendezvous, relay, network) nothing to guess offline against.
//!
//! - The Host registers the password with itself when it is created
//!   ([`HostPassword::new`]), producing an OPAQUE password file. The
//!   plaintext password is not kept.
//! - Login is three messages: `KE1` Controller → Host, `KE2` Host →
//!   Controller, `KE3` Controller → Host.
//! - The login context binds both device public keys and the QUIC TLS
//!   exporter. (OPAQUE identifiers are sealed at registration, before the
//!   Controller is known, so the context carries them instead.) A man in
//!   the middle who terminates TLS on either side gets a different
//!   exporter, so the login fails.
//! - Argon2id stretches the password on the Controller.

use opaque_ke::argon2::Argon2;
use opaque_ke::ciphersuite::CipherSuite;
use opaque_ke::{
    ClientLogin, ClientLoginFinishParameters, ClientRegistration,
    ClientRegistrationFinishParameters, CredentialFinalization, CredentialRequest,
    CredentialResponse, Identifiers, ServerLogin, ServerLoginParameters, ServerRegistration,
    ServerSetup,
};
use rand_core::OsRng;

struct Suite;

impl CipherSuite for Suite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeyExchange = opaque_ke::TripleDh<opaque_ke::Ristretto255, sha2::Sha512>;
    type Ksf = Argon2<'static>;
}

/// Which password the Host is checking against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordKind {
    OneTime,
    Permanent,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PakeError {
    #[error("wrong password")]
    BadPassword,
    #[error("malformed PAKE message")]
    Malformed,
    #[error("PAKE internal error: {0}")]
    Internal(String),
}

/// Domain-separated context: label, both device keys, TLS exporter.
fn context(parties: Parties<'_>, tls_exporter: &[u8]) -> Vec<u8> {
    let mut c = b"relay v1 opaque".to_vec();
    c.extend_from_slice(parties.controller_pubkey);
    c.extend_from_slice(parties.host_pubkey);
    c.extend_from_slice(tls_exporter);
    c
}

/// Who is talking to whom; bound into the OPAQUE transcript.
#[derive(Debug, Clone, Copy)]
pub struct Parties<'a> {
    pub controller_pubkey: &'a [u8; 32],
    pub host_pubkey: &'a [u8; 32],
}

/// The Host's verifier for one password. Holds no plaintext.
pub struct HostPassword {
    setup: ServerSetup<Suite>,
    file: ServerRegistration<Suite>,
    pub kind: PasswordKind,
}

impl std::fmt::Debug for HostPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostPassword")
            .field("kind", &self.kind)
            .finish()
    }
}

/// OPAQUE's "credential identifier": one per Host password.
const CREDENTIAL_ID: &[u8] = b"relay-host";

impl HostPassword {
    /// Register `password` (runs both registration roles locally).
    pub fn new(password: &str, kind: PasswordKind) -> Result<Self, PakeError> {
        let mut rng = OsRng;
        let setup = ServerSetup::<Suite>::new(&mut rng);
        let start = ClientRegistration::<Suite>::start(&mut rng, password.as_bytes())
            .map_err(|e| PakeError::Internal(e.to_string()))?;
        let resp = ServerRegistration::<Suite>::start(&setup, start.message, CREDENTIAL_ID)
            .map_err(|e| PakeError::Internal(e.to_string()))?;
        let finish = start
            .state
            .finish(
                &mut rng,
                password.as_bytes(),
                resp.message,
                ClientRegistrationFinishParameters::default(),
            )
            .map_err(|e| PakeError::Internal(e.to_string()))?;
        let file = ServerRegistration::finish(finish.message);
        Ok(Self { setup, file, kind })
    }

    /// Handle `KE1`; returns the in-progress login and `KE2`.
    pub fn start_login(
        &self,
        ke1: &[u8],
        parties: Parties<'_>,
        tls_exporter: &[u8],
    ) -> Result<(HostLogin, Vec<u8>), PakeError> {
        let req = CredentialRequest::<Suite>::deserialize(ke1).map_err(|_| PakeError::Malformed)?;
        let ctx = context(parties, tls_exporter);
        let res = ServerLogin::start(
            &mut OsRng,
            &self.setup,
            Some(self.file.clone()),
            req,
            CREDENTIAL_ID,
            ServerLoginParameters {
                context: Some(&ctx),
                identifiers: Identifiers::default(),
            },
        )
        .map_err(|e| PakeError::Internal(e.to_string()))?;
        Ok((
            HostLogin { state: res.state },
            res.message.serialize().to_vec(),
        ))
    }
}

/// Host side of a login between `KE2` and `KE3`.
pub struct HostLogin {
    state: ServerLogin<Suite>,
}

impl std::fmt::Debug for HostLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostLogin")
    }
}

impl HostLogin {
    /// Check `KE3`. `Ok` means the Controller knew the password.
    pub fn finish(
        self,
        ke3: &[u8],
        parties: Parties<'_>,
        tls_exporter: &[u8],
    ) -> Result<[u8; 32], PakeError> {
        let fin =
            CredentialFinalization::<Suite>::deserialize(ke3).map_err(|_| PakeError::Malformed)?;
        let ctx = context(parties, tls_exporter);
        let res = self
            .state
            .finish(
                fin,
                ServerLoginParameters {
                    context: Some(&ctx),
                    identifiers: Identifiers::default(),
                },
            )
            .map_err(|_| PakeError::BadPassword)?;
        Ok(session_key(&res.session_key))
    }
}

/// Controller side of a login.
pub struct ControllerLogin {
    state: ClientLogin<Suite>,
}

impl std::fmt::Debug for ControllerLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControllerLogin")
    }
}

impl ControllerLogin {
    /// Start a login; returns the state and `KE1`.
    pub fn start(password: &str) -> Result<(Self, Vec<u8>), PakeError> {
        let res = ClientLogin::<Suite>::start(&mut OsRng, password.as_bytes())
            .map_err(|e| PakeError::Internal(e.to_string()))?;
        Ok((Self { state: res.state }, res.message.serialize().to_vec()))
    }

    /// Process `KE2`; returns `KE3` and the session key. Fails with
    /// [`PakeError::BadPassword`] for a wrong password or a mismatched
    /// transcript (identities, TLS exporter).
    pub fn finish(
        self,
        password: &str,
        ke2: &[u8],
        parties: Parties<'_>,
        tls_exporter: &[u8],
    ) -> Result<(Vec<u8>, [u8; 32]), PakeError> {
        let resp =
            CredentialResponse::<Suite>::deserialize(ke2).map_err(|_| PakeError::Malformed)?;
        let ctx = context(parties, tls_exporter);
        let res = self
            .state
            .finish(
                &mut OsRng,
                password.as_bytes(),
                resp,
                ClientLoginFinishParameters::new(Some(&ctx), Identifiers::default(), None),
            )
            .map_err(|_| PakeError::BadPassword)?;
        Ok((
            res.message.serialize().to_vec(),
            session_key(&res.session_key),
        ))
    }
}

fn session_key(k: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&blake3::derive_key("relay v1 session key", k)[..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: [u8; 32] = [1; 32];
    const H: [u8; 32] = [2; 32];

    fn parties() -> Parties<'static> {
        Parties {
            controller_pubkey: &C,
            host_pubkey: &H,
        }
    }

    fn run(
        host_pw: &str,
        ctl_pw: &str,
        ctl_exporter: &[u8],
        ctl_parties: Parties<'_>,
    ) -> Result<(), PakeError> {
        let hp = HostPassword::new(host_pw, PasswordKind::OneTime)?;
        let (cl, ke1) = ControllerLogin::start(ctl_pw)?;
        let (hl, ke2) = hp.start_login(&ke1, parties(), b"exporter")?;
        let (ke3, ck) = cl.finish(ctl_pw, &ke2, ctl_parties, ctl_exporter)?;
        let hk = hl.finish(&ke3, parties(), b"exporter")?;
        assert_eq!(ck, hk);
        Ok(())
    }

    #[test]
    fn correct_password_agrees() {
        run("Ab3dEf7h", "Ab3dEf7h", b"exporter", parties()).unwrap();
    }

    #[test]
    fn wrong_password_fails() {
        assert_eq!(
            run("Ab3dEf7h", "Ab3dEf7x", b"exporter", parties()),
            Err(PakeError::BadPassword)
        );
    }

    #[test]
    fn mitm_with_different_tls_session_fails() {
        // A relay terminating TLS would give each side a different exporter.
        assert_eq!(
            run("Ab3dEf7h", "Ab3dEf7h", b"other-tls", parties()),
            Err(PakeError::BadPassword)
        );
    }

    #[test]
    fn wrong_host_identity_fails() {
        let other = [9u8; 32];
        let p = Parties {
            controller_pubkey: &C,
            host_pubkey: &other,
        };
        assert_eq!(
            run("Ab3dEf7h", "Ab3dEf7h", b"exporter", p),
            Err(PakeError::BadPassword)
        );
    }

    #[test]
    fn garbage_messages_rejected() {
        let hp = HostPassword::new("pw", PasswordKind::OneTime).unwrap();
        assert!(matches!(
            hp.start_login(&[0u8; 7], parties(), b"x"),
            Err(PakeError::Malformed)
        ));
    }
}

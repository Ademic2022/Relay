//! TLS 1.3 identities bound to device Ed25519 keys (§8.2 step 4, FR-5).
//!
//! Every endpoint presents a self-signed certificate whose key *is* its
//! device key. Both sides authenticate (mutual TLS): the handshake
//! signature proves possession of the key in the certificate, so the
//! certificate's public key is the peer's verified identity.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A device's TLS identity: certificate + private key.
#[derive(Debug)]
pub struct Identity {
    pub cert: CertificateDer<'static>,
    key_pkcs8: Vec<u8>,
    pub public_key: [u8; 32],
}

impl Clone for Identity {
    fn clone(&self) -> Self {
        Self {
            cert: self.cert.clone(),
            key_pkcs8: self.key_pkcs8.clone(),
            public_key: self.public_key,
        }
    }
}

impl Identity {
    /// Build a self-signed certificate for an Ed25519 key given as PKCS#8
    /// DER plus its raw public key.
    pub fn from_ed25519(pkcs8: &[u8], public_key: [u8; 32]) -> Result<Self, String> {
        let der = PrivatePkcs8KeyDer::from(pkcs8.to_vec());
        let kp = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&der, &rcgen::PKCS_ED25519)
            .map_err(|e| e.to_string())?;
        let params =
            rcgen::CertificateParams::new(vec!["relay".to_string()]).map_err(|e| e.to_string())?;
        let cert = params.self_signed(&kp).map_err(|e| e.to_string())?;
        let cert = cert.der().clone();
        if cert_public_key(&cert) != Some(public_key) {
            return Err("certificate key does not match device key".into());
        }
        Ok(Self {
            cert,
            key_pkcs8: pkcs8.to_vec(),
            public_key,
        })
    }

    /// Fresh random identity (servers, tests).
    pub fn generate() -> Result<Self, String> {
        let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).map_err(|e| e.to_string())?;
        let pk: [u8; 32] = kp
            .public_key_raw()
            .try_into()
            .map_err(|_| "bad key length".to_string())?;
        Self::from_ed25519(&kp.serialize_der(), pk)
    }

    /// PKCS#8 DER of the private key, for persisting a server identity.
    pub fn pkcs8(&self) -> &[u8] {
        &self.key_pkcs8
    }

    pub(crate) fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8.clone()))
    }
}

/// Ed25519 SubjectPublicKeyInfo prefix (RFC 8410).
const ED25519_SPKI: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// The Ed25519 public key in a certificate, if it has one.
pub fn cert_public_key(cert: &CertificateDer<'_>) -> Option<[u8; 32]> {
    let der = cert.as_ref();
    let pos = der
        .windows(ED25519_SPKI.len())
        .position(|w| w == ED25519_SPKI)?;
    der.get(pos + 12..pos + 44)?.try_into().ok()
}

/// The verified peer key of an established connection.
pub fn peer_public_key(conn: &quinn::Connection) -> Option<[u8; 32]> {
    let any = conn.peer_identity()?;
    let certs = any.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    cert_public_key(certs.first()?)
}

/// What the Controller expects of the server's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Must be exactly this key (rendezvous server, host found by ID,
    /// pinned host).
    Key([u8; 32]),
    /// First contact in direct mode: accept and report it (TOFU, FR-5).
    AnyRecord,
}

/// Key observed during the last handshake through this verifier.
#[derive(Debug, Clone, Default)]
pub struct SeenKey(Arc<Mutex<Option<[u8; 32]>>>);

impl SeenKey {
    pub fn get(&self) -> Option<[u8; 32]> {
        *self.0.lock().expect("poisoned")
    }
}

fn check_sig(
    provider: &CryptoProvider,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls13_signature(
        message,
        cert,
        dss,
        &provider.signature_verification_algorithms,
    )
}

#[derive(Debug)]
pub(crate) struct ServerKeyVerifier {
    pub provider: Arc<CryptoProvider>,
    pub expect: Expect,
    pub seen: SeenKey,
}

impl ServerCertVerifier for ServerKeyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let key = cert_public_key(end_entity).ok_or_else(|| {
            rustls::Error::General("peer certificate is not an Ed25519 device key".into())
        })?;
        *self.seen.0.lock().expect("poisoned") = Some(key);
        match self.expect {
            Expect::Key(k) if k != key => Err(rustls::Error::General(
                "peer device key does not match".into(),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        check_sig(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Server side: require a client certificate with an Ed25519 key. Any key
/// is accepted here; authorisation happens above TLS (password, whitelist).
#[derive(Debug)]
pub(crate) struct ClientKeyVerifier {
    pub provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for ClientKeyVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        cert_public_key(end_entity)
            .map(|_| ClientCertVerified::assertion())
            .ok_or_else(|| {
                rustls::Error::General("client certificate is not an Ed25519 device key".into())
            })
    }

    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        check_sig(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

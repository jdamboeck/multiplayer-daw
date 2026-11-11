//! TLS without a certificate authority: the server makes its own certificate, and clients accept
//! exactly the one whose fingerprint came with the join string.

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

/// The name in the server's certificate. Identity comes from the fingerprint, not from this.
pub(crate) const SERVER_NAME: &str = "mdaw";

/// A server's certificate and private key, both DER.
#[derive(Clone)]
pub struct Identity {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
}

impl Identity {
    /// A new self-signed identity.
    pub fn generate() -> Result<Self> {
        let made = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])?;
        Ok(Self {
            cert: made.cert.der().to_vec(),
            key: made.signing_key.serialize_der(),
        })
    }

    /// Reads the identity kept in `dir`, creating it on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
        if cert_path.exists() && key_path.exists() {
            let cert =
                std::fs::read(&cert_path).with_context(|| format!("reading {}", cert_path.display()))?;
            let key = std::fs::read(&key_path).with_context(|| format!("reading {}", key_path.display()))?;
            return Ok(Self { cert, key });
        }
        let identity = Self::generate()?;
        write_private(&key_path, &identity.key).with_context(|| format!("writing {}", key_path.display()))?;
        std::fs::write(&cert_path, &identity.cert)
            .with_context(|| format!("writing {}", cert_path.display()))?;
        Ok(identity)
    }

    /// SHA-256 of the certificate, which clients pin.
    pub fn fingerprint(&self) -> [u8; 32] {
        fingerprint(&self.cert)
    }
}

fn fingerprint(cert: &[u8]) -> [u8; 32] {
    Sha256::digest(cert).into()
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub(crate) fn server_config(identity: &Identity) -> Result<rustls::ServerConfig> {
    let cert = CertificateDer::from(identity.cert.clone());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key.clone()));
    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    config.alpn_protocols = crate::alpn();
    Ok(config)
}

pub(crate) fn client_config(fingerprint: [u8; 32]) -> Result<rustls::ClientConfig> {
    let provider = provider();
    let verifier = Arc::new(Pinned {
        fingerprint,
        provider: provider.clone(),
    });
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = crate::alpn();
    Ok(config)
}

/// Accepts the one certificate with the pinned fingerprint and nothing else.
#[derive(Debug)]
struct Pinned {
    fingerprint: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the server's certificate does not match the join string".into(),
            ))
        }
    }

    // The handshake signature still proves the server holds the certificate's private key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        None
    }
}

/// Writes a file only its owner can read, from the first byte on.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    // A file that was there before keeps its mode on open.
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn only_the_owner_can_read_the_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // A key file that was there with lax rights, and a directory others can read.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("key.der"), b"old").unwrap();
        std::fs::set_permissions(dir.path().join("key.der"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let identity = Identity::load_or_create(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join("key.der"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            Identity::load_or_create(dir.path()).unwrap().fingerprint(),
            identity.fingerprint()
        );
    }
}

//! `QUIC` endpoint setup and `TLS` certificate management for the server.

use std::fs;
use std::net::SocketAddr;

use directories::ProjectDirs;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use stargaze_core::transport::TransportError;
use tracing::{debug, info};

/// Creates a server `QUIC` endpoint with `TLS` using a self-signed certificate.
///
/// Loads an existing certificate from disk or generates a new one.
///
/// # Errors
///
/// Returns [`TransportError::TlsError`] if certificate operations fail,
/// or [`TransportError::ConnectionError`] if the endpoint cannot bind.
pub(crate) fn create_server_endpoint(
    bind_addr: SocketAddr,
) -> Result<quinn::Endpoint, TransportError> {
    let (cert_der, key_der) = load_or_generate_cert()?;

    let cert_chain = vec![cert_der];
    let server_config =
        quinn::ServerConfig::with_single_cert(cert_chain, key_der).map_err(|e| {
            TransportError::TlsError(format!("failed to create server TLS config: {e}"))
        })?;

    let endpoint = quinn::Endpoint::server(server_config, bind_addr).map_err(|e| {
        TransportError::ConnectionError(format!("failed to bind QUIC endpoint: {e}"))
    })?;

    Ok(endpoint)
}

/// Loads an existing `TLS` certificate from disk, or generates a new
/// self-signed one if none exists.
///
/// Certificates are stored in `~/.config/stargaze/cert.der` and `key.der`.
fn load_or_generate_cert()
-> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), TransportError> {
    let config_dir = ProjectDirs::from("", "", "stargaze")
        .ok_or_else(|| TransportError::TlsError("cannot determine config directory".to_string()))?;
    let dir = config_dir.config_dir();

    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");

    // Try loading existing cert.
    if cert_path.exists() && key_path.exists() {
        debug!("Loading TLS certificate from {}", cert_path.display());
        let cert_bytes = fs::read(&cert_path)
            .map_err(|e| TransportError::TlsError(format!("read cert: {e}")))?;
        let key_bytes =
            fs::read(&key_path).map_err(|e| TransportError::TlsError(format!("read key: {e}")))?;

        let cert = CertificateDer::from(cert_bytes);
        let key = PrivateKeyDer::try_from(key_bytes)
            .map_err(|e| TransportError::TlsError(format!("parse key: {e}")))?;

        return Ok((cert, key));
    }

    // Generate new self-signed certificate.
    info!("Generating new self-signed TLS certificate");
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| TransportError::TlsError(format!("key generation: {e}")))?;

    let mut params = rcgen::CertificateParams::new(vec!["stargaze-server".to_string()])
        .map_err(|e| TransportError::TlsError(format!("cert params: {e}")))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "stargaze-server");

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| TransportError::TlsError(format!("self-sign: {e}")))?;

    let cert_der_bytes = cert.der().to_vec();
    let key_der_bytes = key_pair.serialize_der();

    // Save to disk.
    fs::create_dir_all(dir)
        .map_err(|e| TransportError::TlsError(format!("create config dir: {e}")))?;
    fs::write(&cert_path, &cert_der_bytes)
        .map_err(|e| TransportError::TlsError(format!("write cert: {e}")))?;
    fs::write(&key_path, &key_der_bytes)
        .map_err(|e| TransportError::TlsError(format!("write key: {e}")))?;

    info!("Saved TLS certificate to {}", cert_path.display());

    let cert = CertificateDer::from(cert_der_bytes);
    let key = PrivateKeyDer::try_from(key_der_bytes)
        .map_err(|e| TransportError::TlsError(format!("parse generated key: {e}")))?;

    Ok((cert, key))
}

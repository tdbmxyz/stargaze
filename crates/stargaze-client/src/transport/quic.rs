//! `QUIC` connection setup for the client.
//!
//! Connects to the server with TLS certificate verification disabled
//! (LAN MVP — both machines are trusted).

use std::net::SocketAddr;
use std::sync::Arc;

use stargaze_core::transport::{DATAGRAM_SEND_BUFFER_SIZE, STREAMING_INITIAL_MTU, TransportError};
use tracing::debug;

/// A `rustls` certificate verifier that accepts any server certificate.
///
/// This is safe for the LAN MVP where both machines are on a trusted
/// local network. A future improvement would use certificate fingerprint
/// pinning.
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

/// Connects to the server at the given address using `QUIC`.
///
/// Uses TLS with server certificate verification disabled (LAN MVP).
///
/// # Errors
///
/// Returns `TransportError::ConnectionError` if the connection fails.
pub(crate) async fn connect_to_server(
    server_addr: SocketAddr,
) -> Result<quinn::Connection, TransportError> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    let mut client_config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).map_err(|e| {
            TransportError::TlsError(format!("failed to create QUIC client config: {e}"))
        })?,
    ));

    let mut transport = quinn::TransportConfig::default();
    transport.initial_mtu(STREAMING_INITIAL_MTU);
    transport.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER_SIZE);
    client_config.transport_config(Arc::new(transport));

    let mut endpoint =
        quinn::Endpoint::client("0.0.0.0:0".parse().expect("valid addr")).map_err(|e| {
            TransportError::ConnectionError(format!("failed to create client endpoint: {e}"))
        })?;
    endpoint.set_default_client_config(client_config);

    debug!("Connecting to {server_addr}");
    let connection = endpoint
        .connect(server_addr, "stargaze-server")
        .map_err(|e| TransportError::ConnectionError(format!("connect: {e}")))?
        .await
        .map_err(|e| TransportError::ConnectionError(format!("connection failed: {e}")))?;

    Ok(connection)
}

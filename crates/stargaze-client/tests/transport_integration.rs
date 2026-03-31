//! Integration test: server<->client transport over localhost.
//!
//! Verifies that encoded packets sent from the server are correctly
//! fragmented into `QUIC` datagrams, transmitted over localhost, and
//! reassembled by the client's [`FrameAssembler`].

use std::sync::Arc;
use std::time::Duration;

use stargaze_core::transport::{
    DatagramHeader, ReassembledFrame, STREAM_TYPE_VIDEO, deserialize_header, serialize_header,
};
use tokio::time::timeout;

/// Test: send synthetic packets through `QUIC` transport and verify
/// the client receives them byte-for-byte after reassembly.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_transport_localhost_round_trip() {
    // Install the default crypto provider (both ring and aws-lc-rs features are
    // enabled, so rustls can't auto-detect).
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    // Create a self-signed cert for the test server.
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "test-server");
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();

    // Server endpoint.
    let server_config = quinn::ServerConfig::with_single_cert(vec![cert_der], key_der).unwrap();
    let server_endpoint =
        quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();

    // Client endpoint with skip verification.
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerification))
        .with_no_client_auth();
    let client_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap();
    let client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
    let mut client_endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
    client_endpoint.set_default_client_config(client_config);

    // Connect client to server.
    let client_conn_future = client_endpoint.connect(server_addr, "localhost").unwrap();
    let server_accept_future = async {
        let incoming = server_endpoint.accept().await.unwrap();
        incoming.accept().unwrap().await.unwrap()
    };

    let (client_conn, server_conn) = tokio::join!(client_conn_future, server_accept_future);
    let client_conn = client_conn.unwrap();

    // Test data: 3 synthetic frames of various sizes.
    let test_frames: Vec<Vec<u8>> = vec![
        vec![0xAA; 100],   // Small frame (single datagram).
        vec![0xBB; 5000],  // Medium frame (multiple fragments).
        vec![0xCC; 15000], // Large frame (many fragments).
    ];

    // Server sends frames as fragmented datagrams.
    let server_handle = tokio::spawn({
        let test_frames = test_frames.clone();
        async move {
            for (frame_idx, frame_data) in test_frames.iter().enumerate() {
                let max_datagram_size = server_conn.max_datagram_size().unwrap_or(1200);

                let sample_header = DatagramHeader {
                    stream_type: STREAM_TYPE_VIDEO,
                    frame_index: u32::try_from(frame_idx).unwrap(),
                    fragment_index: 0,
                    fragment_count: 1,
                    pts: u64::try_from(frame_idx).unwrap(),
                    is_keyframe: frame_idx == 0,
                };
                let header_size = serialize_header(&sample_header).unwrap().len();
                let max_payload = max_datagram_size - header_size;

                let fragment_count = frame_data.len().div_ceil(max_payload);

                for frag_idx in 0..fragment_count {
                    let start = frag_idx * max_payload;
                    let end = ((frag_idx + 1) * max_payload).min(frame_data.len());

                    let header = DatagramHeader {
                        stream_type: STREAM_TYPE_VIDEO,
                        frame_index: u32::try_from(frame_idx).unwrap(),
                        fragment_index: u16::try_from(frag_idx).unwrap(),
                        fragment_count: u16::try_from(fragment_count).unwrap(),
                        pts: u64::try_from(frame_idx).unwrap(),
                        is_keyframe: frame_idx == 0,
                    };

                    let header_bytes = serialize_header(&header).unwrap();
                    let mut datagram = Vec::with_capacity(header_bytes.len() + (end - start));
                    datagram.extend_from_slice(&header_bytes);
                    datagram.extend_from_slice(&frame_data[start..end]);

                    server_conn
                        .send_datagram(bytes::Bytes::from(datagram))
                        .unwrap();
                }
            }

            // Small delay to ensure all datagrams are flushed.
            tokio::time::sleep(Duration::from_millis(100)).await;
            server_conn.close(quinn::VarInt::from_u32(0), b"done");
        }
    });

    // Client receives and reassembles.
    let mut assembler = stargaze_client::transport::receiver::FrameAssembler::new();
    let mut received_frames: Vec<ReassembledFrame> = Vec::new();

    let _receive_result = timeout(Duration::from_secs(5), async {
        loop {
            match client_conn.read_datagram().await {
                Ok(datagram) => {
                    let (header, payload) = deserialize_header(&datagram).unwrap();
                    let (completed, _) = assembler.process_datagram(&header, payload.to_vec());
                    received_frames.extend(completed);

                    if received_frames.len() == test_frames.len() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
    .await;

    server_handle.await.unwrap();

    // Verify all frames were received correctly.
    assert_eq!(
        received_frames.len(),
        test_frames.len(),
        "Expected {} frames, got {}",
        test_frames.len(),
        received_frames.len()
    );

    for (i, (received, expected)) in received_frames.iter().zip(test_frames.iter()).enumerate() {
        assert_eq!(
            received.data,
            *expected,
            "Frame {i} data mismatch (received {} bytes, expected {} bytes)",
            received.data.len(),
            expected.len()
        );
        assert_eq!(received.pts, u64::try_from(i).unwrap());
        assert_eq!(received.is_keyframe, i == 0);
    }

    // Clean up.
    client_endpoint.close(quinn::VarInt::from_u32(0), b"done");
    server_endpoint.close(quinn::VarInt::from_u32(0), b"done");
}

/// Skip server verification for test client.
#[derive(Debug)]
struct SkipVerification;

impl rustls::client::danger::ServerCertVerifier for SkipVerification {
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
        ]
    }
}

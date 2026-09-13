//! `apid --healthcheck`: whether this machine's apid answers `GET /healthz`
//! over HTTPS, for the boot health gate (`mica-system:mica-health`).
//!
//! The address is apid's own `APID_HTTPS_ADDR` (default `0.0.0.0:443`), with an
//! unspecified host probed on loopback. The certificate chain is not verified:
//! it is apid's own self-signed identity, and the question is whether apid
//! answers rather than who it is -- the same question the `curl -k` probe this
//! replaces asked. The handshake signature is still checked, so something that
//! is not a TLS server holding its key does not pass.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

/// Bound on connecting, and on each read or write after it.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Probe apid once; `Ok` only for a 2xx answer to `GET /healthz`.
pub fn probe() -> anyhow::Result<()> {
    let configured = std::env::var("APID_HTTPS_ADDR").unwrap_or_else(|_| "0.0.0.0:443".to_string());
    let mut address: SocketAddr = configured
        .parse()
        .with_context(|| format!("APID_HTTPS_ADDR `{configured}` is not a socket address"))?;
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            Ipv4Addr::LOCALHOST.into()
        } else {
            Ipv6Addr::LOCALHOST.into()
        });
    }
    let stream = TcpStream::connect_timeout(&address, TIMEOUT)
        .with_context(|| format!("connect to apid at {address}"))?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyChain(provider)))
        .with_no_client_auth();
    let connection =
        rustls::ClientConnection::new(Arc::new(config), ServerName::from(address.ip()))?;
    let mut tls = rustls::StreamOwned::new(connection, stream);
    write!(
        tls,
        "GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .with_context(|| format!("TLS request to apid at {address}"))?;
    let mut status = [0_u8; 12];
    tls.read_exact(&mut status)
        .with_context(|| format!("read apid's answer from {address}"))?;
    anyhow::ensure!(
        status.starts_with(b"HTTP/1.") && status[9] == b'2',
        "apid at {address} answered /healthz with `{}`",
        String::from_utf8_lossy(&status)
    );
    Ok(())
}

/// Accepts any certificate chain; verifies handshake signatures.
#[derive(Debug)]
struct AnyChain(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyChain {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

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
            &self.0.signature_verification_algorithms,
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
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

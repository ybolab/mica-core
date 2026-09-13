//! State-directory management: self-signed TLS certificate and the session
//! cookie signing key, generated on first start and reused afterwards.

use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use anyhow::Context;
use rand::RngCore;

/// PEM-encoded server certificate and private key.
pub struct Certificate {
    /// PEM identity containing the certificate chain.
    pub cert_pem: String,
    /// The same PEM identity containing the private key.
    pub key_pem: String,
}

/// Create `dir` with mode 0700 when missing.
pub fn ensure_state_dir(dir: &Path) -> anyhow::Result<()> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::File::open(dir)?.sync_all()?;
        if let Some(parent) = dir.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}

/// Load the atomic `identity.pem` from `dir`, generating a self-signed pair on
/// first start: CN `mica`, SANs `DNS:mica`, `DNS:localhost`, `IP:127.0.0.1`,
/// with rcgen's default long validity. The complete pair is synced and published
/// with mode 0600 in one rename, so interruption cannot leave mismatched halves.
pub fn load_or_generate_certificate(dir: &Path) -> anyhow::Result<Certificate> {
    let identity_path = dir.join("identity.pem");
    if identity_path.exists() {
        let identity = std::fs::read_to_string(&identity_path)
            .with_context(|| format!("read {}", identity_path.display()))?;
        return Ok(Certificate {
            cert_pem: identity.clone(),
            key_pem: identity,
        });
    }

    let mut params =
        rcgen::CertificateParams::new(vec!["mica".to_string(), "localhost".to_string()])
            .context("build certificate params")?;
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "mica");
    let key_pair = rcgen::KeyPair::generate().context("generate certificate key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("self-sign certificate")?;
    let identity = format!("{}{}", cert.pem(), key_pair.serialize_pem());
    crate::persist::write_atomically(&identity_path, &identity, 0o600)?;
    tracing::info!(identity = %identity_path.display(), "generated self-signed identity");
    Ok(Certificate {
        cert_pem: identity.clone(),
        key_pem: identity,
    })
}

/// Load the 32-byte session signing key from `session.key` in `dir`,
/// generating it (mode 0600) on first start.
pub fn load_or_generate_session_key(dir: &Path) -> anyhow::Result<[u8; 32]> {
    let key_path = dir.join("session.key");
    if key_path.exists() {
        let bytes =
            std::fs::read(&key_path).with_context(|| format!("read {}", key_path.display()))?;
        return bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "{} must hold exactly 32 bytes, got {}",
                key_path.display(),
                bytes.len()
            )
        });
    }
    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    crate::persist::write_atomically(&key_path, key, 0o600)?;
    tracing::info!(key = %key_path.display(), "generated session signing key");
    Ok(key)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn generates_and_reloads_material() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        ensure_state_dir(&state).unwrap();
        assert_eq!(
            std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let generated = load_or_generate_certificate(&state).unwrap();
        assert!(generated.cert_pem.contains("BEGIN CERTIFICATE"));
        let identity = std::fs::read_to_string(state.join("identity.pem")).unwrap();
        assert!(identity.contains("BEGIN CERTIFICATE"));
        assert!(identity.contains("BEGIN PRIVATE KEY"));
        assert!(!state.join("cert.pem").exists());
        assert!(!state.join("key.pem").exists());
        let key_mode = std::fs::metadata(state.join("identity.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(key_mode & 0o777, 0o600);
        let reloaded = load_or_generate_certificate(&state).unwrap();
        assert_eq!(reloaded.cert_pem, generated.cert_pem);
        assert_eq!(reloaded.key_pem, generated.key_pem);

        let key = load_or_generate_session_key(&state).unwrap();
        assert_eq!(load_or_generate_session_key(&state).unwrap(), key);
    }

    #[test]
    fn unpublished_identity_is_replaced_as_one_complete_pair() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".identity.pem.apid-tmp"),
            "partial identity",
        )
        .unwrap();
        let generated = load_or_generate_certificate(dir.path()).unwrap();
        let reloaded = load_or_generate_certificate(dir.path()).unwrap();
        assert_eq!(generated.cert_pem, reloaded.cert_pem);
        assert_eq!(generated.key_pem, reloaded.key_pem);
        assert!(!dir.path().join(".identity.pem.apid-tmp").exists());
    }

    #[tokio::test]
    async fn published_identity_loads_into_the_actual_tls_server() {
        let dir = tempfile::tempdir().unwrap();
        let identity = load_or_generate_certificate(dir.path()).unwrap();
        axum_server::tls_rustls::RustlsConfig::from_pem(
            identity.cert_pem.into_bytes(),
            identity.key_pem.into_bytes(),
        )
        .await
        .unwrap();
    }
}

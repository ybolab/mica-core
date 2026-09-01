//! State-directory management: self-signed TLS certificate and the session
//! cookie signing key, generated on first start and reused afterwards.

use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use anyhow::Context;
use rand::RngCore;

/// PEM-encoded server certificate and private key.
pub struct Certificate {
    /// Certificate chain PEM (`cert.pem`).
    pub cert_pem: String,
    /// Private key PEM (`key.pem`).
    pub key_pem: String,
}

/// Create `dir` with mode 0700 when missing.
pub fn ensure_state_dir(dir: &Path) -> anyhow::Result<()> {
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    Ok(())
}

/// Write `bytes` to `path` with mode 0600, replacing any existing file.
fn write_secret(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {} for writing", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// Load `cert.pem`/`key.pem` from `dir`, generating a self-signed pair on
/// first start: CN `mos`, SANs `DNS:mos`, `DNS:localhost`, `IP:127.0.0.1`,
/// with rcgen's default long validity. The key is written with mode 0600.
pub fn load_or_generate_certificate(dir: &Path) -> anyhow::Result<Certificate> {
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    if cert_path.exists() && key_path.exists() {
        return Ok(Certificate {
            cert_pem: std::fs::read_to_string(&cert_path)
                .with_context(|| format!("read {}", cert_path.display()))?,
            key_pem: std::fs::read_to_string(&key_path)
                .with_context(|| format!("read {}", key_path.display()))?,
        });
    }

    let mut params =
        rcgen::CertificateParams::new(vec!["mos".to_string(), "localhost".to_string()])
            .context("build certificate params")?;
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "mos");
    let key_pair = rcgen::KeyPair::generate().context("generate certificate key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("self-sign certificate")?;
    let certificate = Certificate {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    };
    std::fs::write(&cert_path, &certificate.cert_pem)
        .with_context(|| format!("write {}", cert_path.display()))?;
    write_secret(&key_path, certificate.key_pem.as_bytes())?;
    tracing::info!(cert = %cert_path.display(), "generated self-signed certificate");
    Ok(certificate)
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
    write_secret(&key_path, &key)?;
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
        let key_mode = std::fs::metadata(state.join("key.pem"))
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
}

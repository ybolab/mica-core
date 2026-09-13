//! Environment-driven daemon configuration.

use std::path::PathBuf;

/// Which message bus to reach `micad` on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusKind {
    /// The system bus (production default).
    System,
    /// The session bus (tests and development).
    Session,
}

/// Runtime configuration, read once at startup.
#[derive(Debug, Clone)]
pub struct Config {
    /// HTTPS listen address (`APID_HTTPS_ADDR`).
    pub https_addr: String,
    /// Redirect-only HTTP listen address (`APID_HTTP_ADDR`).
    pub http_addr: String,
    /// Directory holding the certificate and key material (`APID_STATE_DIR`).
    pub state_dir: PathBuf,
    /// Bus to reach `micad` on (`APID_BUS`).
    pub bus: BusKind,
}

impl Config {
    /// Read configuration from the environment, applying defaults.
    ///
    /// # Errors
    ///
    /// Fails when `APID_BUS` is set to anything but `system` or `session`.
    pub fn from_env() -> anyhow::Result<Self> {
        let https_addr =
            std::env::var("APID_HTTPS_ADDR").unwrap_or_else(|_| "0.0.0.0:443".to_string());
        let http_addr =
            std::env::var("APID_HTTP_ADDR").unwrap_or_else(|_| "0.0.0.0:80".to_string());
        let state_dir = PathBuf::from(
            std::env::var("APID_STATE_DIR").unwrap_or_else(|_| "/var/lib/mica/apid".to_string()),
        );
        let bus = match std::env::var("APID_BUS").as_deref() {
            Err(_) | Ok("system") => BusKind::System,
            Ok("session") => BusKind::Session,
            Ok(other) => anyhow::bail!("APID_BUS must be `system` or `session`, got `{other}`"),
        };
        Ok(Self {
            https_addr,
            http_addr,
            state_dir,
            bus,
        })
    }
}

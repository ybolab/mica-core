//! Error type shared across the settings crate.

use std::io;

/// Errors returned by settings operations.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The dot-path does not resolve to an existing node.
    #[error("settings path not found: `{0}`")]
    NotFound(String),
    /// The dot-path exists but rejects writes.
    #[error("settings path is read-only: `{0}`")]
    ReadOnly(String),
    /// The value (or path shape) does not fit the typed settings tree.
    #[error("invalid settings value at `{path}`: {message}")]
    Validation {
        /// Dot-path of the offending write.
        path: String,
        /// Human-readable reason the value was rejected.
        message: String,
    },
    /// Underlying filesystem failure.
    #[error("settings io error: {0}")]
    Io(#[from] io::Error),
    /// The document could not be parsed or serialized.
    #[error("settings parse error: {0}")]
    Parse(String),
    /// A schema migration failed or is unavailable.
    #[error("settings migration error: {0}")]
    Migration(String),
}

//! Error type shared across the settings crate.

use std::io;

/// Errors returned by settings operations.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The dot-path does not resolve to an existing node.
    #[error("settings path not found: `{0}`")]
    NotFound(String),
    /// The dot-path exists but rejects writes.
    ///
    /// **Nothing in this crate produces it today.** Its only producer was
    /// `Settings::set` refusing a write to `schema_version`, and PLAN-070
    /// §5.2.3 removed that key from the tree — the version is per document
    /// now. The variant is kept because it is a published bus contract:
    /// `micad::bus` maps it to `com.mica.micad1.Error.ReadOnly` and apid maps
    /// that name to a 409. Retiring the name is a change to that contract and
    /// belongs with whoever owns it, not here; adding a read-only key to the
    /// schema makes it live again.
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
    /// The document is not at this build's exact schema version.
    #[error("settings schema version error: {0}")]
    SchemaVersion(String),
    /// The medium carrying the configuration namespace is not mounted.
    ///
    /// Deliberately not an [`SettingsError::Io`] `NotFound`: an absent
    /// document is a default and an absent NAMESPACE is a device that cannot
    /// read its configuration, which must refuse rather than render one
    /// nobody chose (PLAN-070 §5.2.6). The message names the mount because
    /// that is the fact an operator at the serial console needs.
    #[error(
        "{directory} is not there, so this device has no configuration to render: `{mount}` is \
         not mounted. Refusing to start on schema defaults — a device that cannot read its \
         configuration must not render a different one"
    )]
    Unavailable {
        /// The configuration namespace that is missing.
        directory: String,
        /// The mount its absence implicates.
        mount: String,
    },
}

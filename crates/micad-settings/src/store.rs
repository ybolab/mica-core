//! Persistence for the settings tree: the `/mos/config/` documents on DATA
//! and the remainder on STATE.
//!
//! One store, several documents (PLAN-070 §5.2). [`crate::documents`] holds
//! the storage shape and the reasons for it; this module is the reader and the
//! writer, and it is the only place that converts between the documents and
//! the one addressed [`Settings`] tree.

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::documents::{
    CONTAINER_DOCUMENT, CONTAINER_SCHEMA_VERSION, DEFAULT_CONFIG_DIR, DOCUMENT_MODE, DocumentSet,
    MQTT_DOCUMENT, MQTT_SCHEMA_VERSION, NETWORK_DOCUMENT, NETWORK_SCHEMA_VERSION, SSH_DOCUMENT,
    SSH_SCHEMA_VERSION, STATE_SCHEMA_VERSION, SYSTEM_DOCUMENT, SYSTEM_SCHEMA_VERSION,
    StateDocument, TIME_DOCUMENT, TIME_SCHEMA_VERSION, WIFI_DOCUMENT, WIFI_SCHEMA_VERSION,
    document_subtrees,
};
use crate::error::SettingsError;
use crate::model::Settings;

/// Default on-disk location of the STATE document.
pub const DEFAULT_PATH: &str = "/var/lib/mica/settings.toml";

/// The name the STATE document is reported under.
const STATE_DOCUMENT: &str = "settings.toml";

/// One `/mos/config/` document that exists and did **not** become
/// configuration (PLAN-070 §5.2.7, F6g).
///
/// **This is "refuses its subsystem", and it is not "refuses to start".** The
/// two are different rules and the tree carries both: an absent `/mos/config/`
/// **namespace** is the DATA medium being gone and refuses the daemon
/// ([`Store::ensure_config_medium`], §5.2.6), while a single document that does
/// not parse refuses exactly the capabilities *it* gates and leaves its
/// neighbours alone. The pour is why the second one has to exist: an integrator
/// hand-writes these files onto a device that is not running, and a typo in
/// `wifi.json` that took the network reconciler down with it would put the
/// device off the air for a mistake in an unrelated subsystem.
///
/// **The subtree is refused, never defaulted.** [`Store::load_with_refusals`]
/// still returns a total [`Settings`] — the dot-path API has no shape for a
/// hole — so the refused document's subtree sits at its schema default in the
/// addressed tree. What makes that not a silent revert is that the refusal
/// travels with it: the caller skips the reconcilers [`Self::subtrees`] names,
/// so the default is never *applied*, and [`Store::save_preserving`] leaves the
/// document on disk untouched, so the operator's bytes are not overwritten by
/// the default either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentRefusal {
    /// The document this refusal is about, by file name.
    pub document: String,
    /// The file, in full. Carried as well as [`Self::message`] because a
    /// caller records the refusal per document and needs the identity, not
    /// only the sentence.
    pub path: PathBuf,
    /// The refusal an operator reads, and the **only form that may be
    /// served**. Names the file — which the F6g gate requires, because the
    /// whole point of a poured document failing is that the person who poured
    /// it can tell which one — and says what class of failure it was, and
    /// nothing else.
    ///
    /// It quotes none of the document, and that is the F6g gate's third clause
    /// applied to the failing case as well as the adopted one. A parser's
    /// sentence echoes what it choked on: `invalid type: string "…", expected
    /// u8` prints the value, so a poured `wifi.json` whose site key landed in
    /// the wrong field would publish that key through a refusal — into
    /// `configuration.refused`, which `GET /api/v1/state/` serves, past a
    /// redactor that keys on field names and has no reason to look at one
    /// called `message`.
    pub message: String,
    /// The parser's own sentence. **Journal only.**
    ///
    /// This is the half that can quote the document, and the split is what
    /// makes serving the safe half the default: a caller reaching for
    /// `message` gets the one that may leave the device, and a caller that
    /// wants the parser's words has to name a field whose documentation says
    /// where they may go.
    pub detail: String,
    /// The addressed-tree subtrees this document carries
    /// ([`crate::documents::DOCUMENT_SUBTREES`]) — the exact set of
    /// capabilities the refusal covers.
    pub subtrees: &'static [&'static str],
}

/// Loaded settings and explicit per-document refusals.
#[derive(Debug, Clone)]
pub struct LoadedStore {
    /// The one addressed tree, total as always.
    pub settings: Settings,
    /// The `/mos/config/` documents that exist and did not load.
    pub refusals: Vec<DocumentRefusal>,
}

/// The two on-disk formats: JSON for `/mos/config/`, TOML for the STATE
/// remainder.
///
/// `/mos/config/` is JSON because these are machine-written documents and JSON
/// is what a machine writes without a round-trip formatting problem (§5.2.7).
/// The STATE document is not in that namespace and keeps the format it already
/// had.
#[derive(Debug, Clone, Copy)]
enum Format {
    Toml,
    Json,
}

impl Format {
    /// Parse into a format-neutral tree before enforcing the exact schema.
    fn parse(self, text: &str) -> Result<Value, String> {
        match self {
            Self::Toml => toml::from_str(text).map_err(|err: toml::de::Error| err.to_string()),
            Self::Json => {
                serde_json::from_str(text).map_err(|err: serde_json::Error| err.to_string())
            }
        }
    }

    fn render<T: Serialize>(self, value: &T) -> Result<String, String> {
        match self {
            Self::Toml => toml::to_string(value).map_err(|err| err.to_string()),
            Self::Json => serde_json::to_string_pretty(value)
                .map(|mut text| {
                    text.push('\n');
                    text
                })
                .map_err(|err| err.to_string()),
        }
    }
}

/// The `schema_version` a parsed document declares.
///
/// A document with no version, or one whose version is not an integer, does
/// not parse — and a document that does not parse is refused rather than
/// treated as absent (§5.2.7).
fn declared_version(doc: &Value, document: &str) -> Result<u32, SettingsError> {
    let Some(object) = doc.as_object() else {
        return Err(SettingsError::Parse(format!(
            "{document} is not a document: the top level is not a table"
        )));
    };
    match object.get("schema_version") {
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|version| u32::try_from(version).ok())
            .ok_or_else(|| {
                SettingsError::Parse(format!("{document}: schema_version {number} out of range"))
            }),
        Some(other) => Err(SettingsError::Parse(format!(
            "{document}: schema_version must be an integer, got {other}"
        ))),
        None => Err(SettingsError::Parse(format!(
            "{document}: no schema_version"
        ))),
    }
}

/// Public classification without the parser's potentially sensitive values.
fn refusal_class(err: &SettingsError) -> &'static str {
    match err {
        SettingsError::SchemaVersion(_) => "has an unsupported schema version",
        SettingsError::Io(_) => "could not be read",
        _ => "did not parse as this build's schema",
    }
}

/// Read one document, or its schema default when the file is absent.
///
/// **Absence is a default; a parse error is not** (§5.2.7). A document that
/// exists and does not parse refuses the load rather than silently reverting
/// to a schema default, because treating a parse error as absence configures a
/// device the way nobody chose.
fn read_document<T: DeserializeOwned + Default>(
    path: &Path,
    document: &str,
    format: Format,
    version: u32,
) -> Result<T, SettingsError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
        Err(err) => return Err(err.into()),
    };
    let doc = format
        .parse(&text)
        .map_err(|message| SettingsError::Parse(format!("{document}: {message}")))?;
    let from = declared_version(&doc, document)?;
    if from != version {
        return Err(SettingsError::SchemaVersion(format!(
            "{document} declares schema version {from}; this build requires {version}"
        )));
    }
    serde_json::from_value(doc).map_err(|err| SettingsError::Parse(format!("{document}: {err}")))
}

/// Replace `path` with `text` atomically at [`DOCUMENT_MODE`]: a temporary
/// sibling, the mode set **before** the rename, the bytes fsynced, the rename,
/// and the directory fsynced after it.
///
/// **The one write discipline of the `/mos/config/` namespace** (PLAN-070
/// §5.2), and it is `pub(crate)` for that reason: the settings documents above
/// and [`crate::configuration::save_updates`] are two writers of one
/// namespace, and a second implementation of this sequence would be a second
/// answer to "what does a reader see when the power fails here". A reader sees
/// the old document or the new one.
pub(crate) fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(text.as_bytes())?;
    temp.flush()?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(DOCUMENT_MODE))?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|err| err.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// The settings store: the `/mos/config/` namespace plus the STATE document.
#[derive(Debug, Clone)]
pub struct Store {
    /// The STATE document, holding what the device mints or observes.
    path: PathBuf,
    /// The `/mos/config/` namespace, holding what an integrator sets.
    config_dir: PathBuf,
    /// `/mos/config/` documents this store will not overwrite while they are
    /// still on disk ([`Store::preserving`]).
    preserve: Vec<String>,
}

impl Store {
    /// Store backed by the STATE document at `path` and the configuration
    /// namespace at `config_dir`.
    pub fn new(path: impl Into<PathBuf>, config_dir: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            config_dir: config_dir.into(),
            preserve: Vec::new(),
        }
    }

    /// The same store, refusing to overwrite `documents` while they are still
    /// on disk (PLAN-070 §5.2.7, F6g — **the pour**).
    ///
    /// **This exists because a refused document is one `save` away from being
    /// lost.** A refused subtree sits at its schema default in the addressed
    /// tree, because the tree is total; `save` writes every document out of
    /// that tree; so any later write — a hostname change, or first-boot
    /// provisioning minting a device identity, which happens on the very boot
    /// that finds the pour — would replace the integrator's file with the
    /// default it never chose. That is the silent revert §5.2.7 forbids,
    /// arriving one save after the load rather than during it.
    ///
    /// **On the store rather than as an argument to `save`**, because `save`
    /// has five callers on the boot path (`provisioning`, `reset`, `recovery`,
    /// the provisioning document and the bus) and a rule that has to be
    /// remembered at five call sites is a rule with four places to forget it.
    /// `main.rs` narrows the store once, immediately after the load, and every
    /// later holder inherits it.
    ///
    /// **"While they are still on disk" is the whole condition.** A tier-1
    /// reset empties `/mos/config/` and then saves; the refused file is gone by
    /// then, and writing the fresh default there is exactly what the reset
    /// asked for. Preserving a name whose file no longer exists would leave the
    /// namespace one document short of what a reset is supposed to produce.
    #[must_use]
    pub fn preserving(&self, documents: &[&str]) -> Self {
        Self {
            path: self.path.clone(),
            config_dir: self.config_dir.clone(),
            preserve: documents.iter().map(|name| (*name).to_string()).collect(),
        }
    }

    /// Store backed by [`DEFAULT_PATH`] and [`DEFAULT_CONFIG_DIR`].
    #[must_use]
    pub fn default_path() -> Self {
        Self::new(DEFAULT_PATH, DEFAULT_CONFIG_DIR)
    }

    /// The configuration namespace this store reads and writes.
    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Refuse rather than fall back to defaults when the DATA medium carrying
    /// `/mos/config/` is not there (§5.2.6).
    ///
    /// **This is the fail-closed rule applied to the medium instead of to the
    /// bytes.** A device whose DATA pool does not mount has no configuration,
    /// and a device that cannot read its configuration must not render a
    /// different one: it would come up on schema defaults — DHCP on every
    /// interface, sshd off — and be unreachable by anyone relying on the
    /// static address they configured, while looking fine. The recovery route
    /// is `docs/design/recovery.md`'s: the serial console and the recovery
    /// tiers, not a silently degraded network.
    ///
    /// micad's unit carries `RequiresMountsFor=/mos` so this is ordinarily
    /// unreachable; the check is here because the unit is not the only way
    /// micad starts, and because the refusal has to name the mount.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Unavailable`], naming the directory and the
    /// mount it belongs to.
    pub fn ensure_config_medium(&self) -> Result<(), SettingsError> {
        if self.config_dir.is_dir() {
            return Ok(());
        }
        let mount = self
            .config_dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(self.config_dir.as_path());
        Err(SettingsError::Unavailable {
            directory: self.config_dir.display().to_string(),
            mount: mount.display().to_string(),
        })
    }

    /// Load the current schema without conversion. Missing documents use their
    /// defaults; unreadable, malformed or differently versioned files fail.
    ///
    /// # Errors
    /// Returns the storage, parse or schema-version failure for the document.
    pub fn load(&self) -> Result<Settings, SettingsError> {
        self.read_store(None).map(|loaded| loaded.settings)
    }

    /// Load every document, refusing per document instead of per daemon
    /// (PLAN-070 §5.2.7, F6g — **the pour**).
    ///
    /// This is [`Store::load`] with one rule changed, and it is the
    /// rule the pour exists for. An integrator hand-writes these documents onto
    /// a device that is not running; the next boot validates what it finds
    /// exactly as it validates micad's own output, and a document that does not
    /// survive that is a [`DocumentRefusal`] rather than an aborted load. Under
    /// `load` a single mistyped `wifi.json` stops the daemon, which
    /// takes the network reconciler down with it and puts the device off the
    /// air for a mistake in an unrelated subsystem.
    ///
    /// **What is NOT downgraded**, because they are different rules:
    ///
    /// - an absent `/mos/config/` **namespace** is still a hard refusal
    ///   ([`Store::ensure_config_medium`], §5.2.6 / F6f): a device that cannot
    ///   reach its configuration must not render a different one;
    /// - the **STATE** document is still a hard refusal. It is not in this
    ///   namespace and cannot be poured, and it carries the device identity and
    ///   the administrator credential — degrading it to a per-document refusal
    ///   would let `provisioning::ensure_provisioned` mint a fresh identity and
    ///   credential over a store whose real ones merely failed to parse, which
    ///   is a worse outcome than not starting.
    ///
    /// **Nothing falls back.** The refused document's subtree is at its schema
    /// default in the returned tree because the tree is total, and every
    /// consumer of that default is closed off by the refusal travelling beside
    /// it: see [`DocumentRefusal`].
    ///
    /// # Errors
    ///
    /// As [`Store::load`], less the per-document failures this
    /// collects instead: [`SettingsError::Unavailable`] for the medium, and
    /// [`SettingsError::Io`] / [`SettingsError::Parse`] /
    /// [`SettingsError::SchemaVersion`] for the STATE document alone.
    pub fn load_with_refusals(&self) -> Result<LoadedStore, SettingsError> {
        let mut refusals = Vec::new();
        self.read_store(Some(&mut refusals))
    }

    /// The one loader both entry points run.
    ///
    /// `refusals` is what distinguishes them and nothing else does: `Some`
    /// collects a `/mos/config/` document's failure and carries on with that
    /// document's schema default, `None` propagates it and abandons the load.
    /// One implementation because two would drift, and the direction they would
    /// drift in is the lenient one.
    fn read_store(
        &self,
        mut refusals: Option<&mut Vec<DocumentRefusal>>,
    ) -> Result<LoadedStore, SettingsError> {
        self.ensure_config_medium()?;
        crate::transaction::recover(&self.path, &self.config_dir)?;
        let documents = DocumentSet {
            system: self.read_config(
                SYSTEM_DOCUMENT,
                SYSTEM_SCHEMA_VERSION,
                refusals.as_deref_mut(),
            )?,
            network: self.read_config(
                NETWORK_DOCUMENT,
                NETWORK_SCHEMA_VERSION,
                refusals.as_deref_mut(),
            )?,
            wifi: self.read_config(WIFI_DOCUMENT, WIFI_SCHEMA_VERSION, refusals.as_deref_mut())?,
            ssh: self.read_config(SSH_DOCUMENT, SSH_SCHEMA_VERSION, refusals.as_deref_mut())?,
            mqtt: self.read_config(MQTT_DOCUMENT, MQTT_SCHEMA_VERSION, refusals.as_deref_mut())?,
            time: self.read_config(TIME_DOCUMENT, TIME_SCHEMA_VERSION, refusals.as_deref_mut())?,
            container: self.read_config(
                CONTAINER_DOCUMENT,
                CONTAINER_SCHEMA_VERSION,
                refusals.as_deref_mut(),
            )?,
            // The STATE remainder, and NOT through the refusal path: see
            // [`Store::load_with_refusals`] for why it keeps the hard failure.
            state: read_document::<StateDocument>(
                &self.path,
                STATE_DOCUMENT,
                Format::Toml,
                STATE_SCHEMA_VERSION,
            )?,
        };
        Ok(LoadedStore {
            settings: documents.compose(),
            refusals: refusals.map(std::mem::take).unwrap_or_default(),
        })
    }

    fn read_config<T: DeserializeOwned + Default>(
        &self,
        document: &str,
        version: u32,
        refusals: Option<&mut Vec<DocumentRefusal>>,
    ) -> Result<T, SettingsError> {
        let path = self.config_dir.join(document);
        let read = read_document(&path, document, Format::Json, version);
        match (read, refusals) {
            (Ok(value), _) => Ok(value),
            (Err(err), None) => Err(err),
            (Err(err), Some(refusals)) => {
                refusals.push(DocumentRefusal {
                    document: document.to_string(),
                    message: format!(
                        "{} {}, so every capability it configures is refused rather than \
                         rendered from a schema default",
                        path.display(),
                        refusal_class(&err)
                    ),
                    detail: err.to_string(),
                    path,
                    subtrees: document_subtrees(document),
                });
                Ok(T::default())
            }
        }
    }

    /// Persist changed documents with a durable undo journal on STATE.
    ///
    /// A failed or interrupted save restores the previous documents before the
    /// store can load or save again. The single writer removes the journal only
    /// after every replacement is durable. Unchanged and refused files retain
    /// their bytes and inodes.
    ///
    /// # Errors
    ///
    /// Returns an error for an unavailable medium, serialization failure, or I/O
    /// failure. A rollback that cannot finish leaves its journal for recovery.
    pub fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
        self.ensure_config_medium()?;
        crate::transaction::recover(&self.path, &self.config_dir)?;
        let documents = DocumentSet::of(settings);
        let mut writes = Vec::new();
        for (name, rendered) in [
            (SYSTEM_DOCUMENT, Format::Json.render(&documents.system)),
            (NETWORK_DOCUMENT, Format::Json.render(&documents.network)),
            (WIFI_DOCUMENT, Format::Json.render(&documents.wifi)),
            (SSH_DOCUMENT, Format::Json.render(&documents.ssh)),
            (MQTT_DOCUMENT, Format::Json.render(&documents.mqtt)),
            (TIME_DOCUMENT, Format::Json.render(&documents.time)),
            (
                CONTAINER_DOCUMENT,
                Format::Json.render(&documents.container),
            ),
            (STATE_DOCUMENT, Format::Toml.render(&documents.state)),
        ] {
            if self.preserve.iter().any(|document| document == name)
                && self.config_dir.join(name).exists()
            {
                continue;
            }
            let text =
                rendered.map_err(|message| SettingsError::Parse(format!("{name}: {message}")))?;
            writes.push((name.to_string(), text));
        }
        crate::transaction::save(&self.path, &self.config_dir, &writes)?;
        Ok(())
    }
}

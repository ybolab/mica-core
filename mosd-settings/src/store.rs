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
pub const DEFAULT_PATH: &str = "/var/lib/mos/settings.toml";

/// The name the STATE document is reported under.
const STATE_DOCUMENT: &str = "settings.toml";

/// What the tolerant newer-schema load did to ONE document, for the caller to
/// log loudly.
///
/// Produced only when that document's on-disk `schema_version` was greater
/// than the version this build writes — i.e. on the A/B rollback path. See
/// [`Store::load_with_report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackReport {
    /// The document this report is about, by file name.
    pub document: String,
    /// The newer schema version the document carried.
    pub from: u32,
    /// Keys stripped to make the document parse, in the order they were
    /// dropped. Recursive: a same-named key elsewhere in the document is
    /// dropped by the same pass.
    pub dropped_keys: Vec<String>,
    /// True when stripping was not enough — the newer schema reshaped an
    /// existing key — and the load fell back to this document's schema
    /// default, abandoning everything it stored.
    pub defaulted: bool,
}

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

/// A load of the whole store: the addressed tree, what the tolerant
/// newer-schema path did, and which documents were refused.
///
/// Three outcomes rather than two because they are three different facts and
/// collapsing any pair loses the one that matters. A rollback report is a
/// document that WAS adopted, with keys dropped; a refusal is a document that
/// was not adopted at all.
#[derive(Debug, Clone)]
pub struct LoadedStore {
    /// The one addressed tree, total as always.
    pub settings: Settings,
    /// What the A/B rollback path did, per document.
    pub rollback: Vec<RollbackReport>,
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
    /// Parse into the format-neutral tree the version check and the tolerant
    /// load both work on.
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

/// The field name out of a serde `deny_unknown_fields` rejection, if that is
/// what `message` is.
///
/// Serde spells it `` unknown field `name`, expected ... `` and both toml and
/// serde_json carry the message through; the toml workspace pin (`=0.9`-line)
/// keeps the spelling stable.
fn unknown_field_name(message: &str) -> Option<String> {
    let rest = message.split("unknown field `").nth(1)?;
    let (name, _) = rest.split_once('`')?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Remove every key named `key` anywhere in `value`, recursively (arrays
/// included). Returns whether anything was removed.
fn strip_key(value: &mut Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            let mut removed = map.remove(key).is_some();
            for (_, child) in map.iter_mut() {
                removed |= strip_key(child, key);
            }
            removed
        }
        Value::Array(items) => {
            let mut removed = false;
            for item in items {
                removed |= strip_key(item, key);
            }
            removed
        }
        _ => false,
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

/// The tolerant path for a document newer than this build writes.
///
/// **Infallible by design.** This is the A/B rollback path: the other slot ran
/// a newer mosd, wrote its documents, and this slot was rolled back to.
/// Refusing such a document makes mosd exit, and under `Restart=on-failure`
/// the rolled-back-to slot is then a crash loop — which also fails that slot's
/// health gate, so a rollback whose whole point is reaching a working slot
/// produces a device with no confirmable slot at all. Down-migrations cannot
/// help by construction: this binary cannot carry the migration a future
/// schema will need.
///
/// What "tolerantly" means: keys this schema does not know are dropped, one at
/// a time and recursively, until the document parses. The next [`Store::save`]
/// persists the stripped document at this schema version. If stripping is not
/// enough — a future schema **reshaped** an existing key — the last resort is
/// this document's schema default, reported rather than returned as an error.
/// Schema authors owe the mitigation: prefer additive bumps; a reshaping bump
/// forfeits that document's settings on rollback and must say so.
///
/// **The blast radius is one document**, which is part of what the
/// per-document version buys: a reshaped `wifi.json` costs the Wi-Fi settings
/// and leaves the network, the ssh policy and the management credential alone.
fn load_newer<T: DeserializeOwned + Default>(
    mut doc: Value,
    from: u32,
    version: u32,
    document: &str,
) -> (T, RollbackReport) {
    // The version stamp itself is the first "key this schema does not
    // recognise the value of": rewrite it to ours so the parse below is over a
    // document claiming the schema it is being read as.
    if let Some(object) = doc.as_object_mut() {
        object.insert("schema_version".to_string(), Value::from(version));
    }
    let mut dropped = Vec::new();
    // Bounded: each pass must strip at least one key or the loop ends. The
    // bound itself is defensive; a document has finitely many keys.
    for _ in 0..64 {
        match serde_json::from_value::<T>(doc.clone()) {
            Ok(parsed) => {
                return (
                    parsed,
                    RollbackReport {
                        document: document.to_string(),
                        from,
                        dropped_keys: dropped,
                        defaulted: false,
                    },
                );
            }
            Err(err) => {
                let Some(key) = unknown_field_name(&err.to_string()) else {
                    break; // reshaped, not additive: fall through
                };
                if !strip_key(&mut doc, &key) {
                    break; // named key not found: cannot make progress
                }
                dropped.push(key);
            }
        }
    }
    (
        T::default(),
        RollbackReport {
            document: document.to_string(),
            from,
            dropped_keys: dropped,
            defaulted: true,
        },
    )
}

/// How a document failed, in words this build chose rather than words the
/// document supplied.
///
/// Three classes, because three are what a reader can act on — fix the bytes,
/// fix the version, fix the permissions — and because a fourth would have to
/// come from the parser, which is the half that must not be served
/// ([`DocumentRefusal::message`]).
fn refusal_class(err: &SettingsError) -> &'static str {
    match err {
        SettingsError::Migration(_) => "is at a schema version this build has no migration for",
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
    reports: &mut Vec<RollbackReport>,
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
    if from > version {
        let (parsed, report) = load_newer(doc, from, version, document);
        reports.push(report);
        return Ok(parsed);
    }
    if from < version {
        // Reachable only once a document has bumped past v1 and this build is
        // older than the file it found. There is no migration registry to walk
        // — the `V0→V12` chain was deleted with the document it migrated — so
        // the honest answer is a refusal that names the document.
        return Err(SettingsError::Migration(format!(
            "{document} is at schema version {from} and this build reads version {version}; \
             no migration is registered"
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
    /// mosd's unit carries `RequiresMountsFor=/mos` so this is ordinarily
    /// unreachable; the check is here because the unit is not the only way
    /// mosd starts, and because the refusal has to name the mount.
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

    /// Load settings from disk, discarding the rollback reports.
    ///
    /// See [`Store::load_with_report`] for the full contract; this wrapper
    /// exists so callers that cannot log (tests, one-shot tools) keep the
    /// short call. mosd itself calls [`Store::load_with_report`] and logs.
    ///
    /// # Errors
    ///
    /// As [`Store::load_with_report`].
    pub fn load(&self) -> Result<Settings, SettingsError> {
        self.load_with_report().map(|(settings, _)| settings)
    }

    /// Load every document and compose the one addressed tree.
    ///
    /// A missing document yields its schema default without creating the file;
    /// a missing `/mos/config/` **directory** does not, because that is the
    /// medium being gone rather than a document never having been written
    /// ([`Store::ensure_config_medium`]).
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Unavailable`] when the configuration medium is
    /// not mounted, [`SettingsError::Io`] on read failures,
    /// [`SettingsError::Parse`] on a document that does not parse, and
    /// [`SettingsError::Migration`] for a document older than this build
    /// reads. A document NEWER than this build writes never errors — see
    /// [`load_newer`].
    pub fn load_with_report(&self) -> Result<(Settings, Vec<RollbackReport>), SettingsError> {
        let loaded = self.read_store(None)?;
        Ok((loaded.settings, loaded.rollback))
    }

    /// Load every document, refusing per document instead of per daemon
    /// (PLAN-070 §5.2.7, F6g — **the pour**).
    ///
    /// This is [`Store::load_with_report`] with one rule changed, and it is the
    /// rule the pour exists for. An integrator hand-writes these documents onto
    /// a device that is not running; the next boot validates what it finds
    /// exactly as it validates mosd's own output, and a document that does not
    /// survive that is a [`DocumentRefusal`] rather than an aborted load. Under
    /// `load_with_report` a single mistyped `wifi.json` stops the daemon, which
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
    /// As [`Store::load_with_report`], less the per-document failures this
    /// collects instead: [`SettingsError::Unavailable`] for the medium, and
    /// [`SettingsError::Io`] / [`SettingsError::Parse`] /
    /// [`SettingsError::Migration`] for the STATE document alone.
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
        let mut reports = Vec::new();
        let documents = DocumentSet {
            system: self.read_config(
                SYSTEM_DOCUMENT,
                SYSTEM_SCHEMA_VERSION,
                &mut reports,
                refusals.as_deref_mut(),
            )?,
            network: self.read_config(
                NETWORK_DOCUMENT,
                NETWORK_SCHEMA_VERSION,
                &mut reports,
                refusals.as_deref_mut(),
            )?,
            wifi: self.read_config(
                WIFI_DOCUMENT,
                WIFI_SCHEMA_VERSION,
                &mut reports,
                refusals.as_deref_mut(),
            )?,
            ssh: self.read_config(
                SSH_DOCUMENT,
                SSH_SCHEMA_VERSION,
                &mut reports,
                refusals.as_deref_mut(),
            )?,
            mqtt: self.read_config(
                MQTT_DOCUMENT,
                MQTT_SCHEMA_VERSION,
                &mut reports,
                refusals.as_deref_mut(),
            )?,
            time: self.read_config(
                TIME_DOCUMENT,
                TIME_SCHEMA_VERSION,
                &mut reports,
                refusals.as_deref_mut(),
            )?,
            container: self.read_config(
                CONTAINER_DOCUMENT,
                CONTAINER_SCHEMA_VERSION,
                &mut reports,
                refusals.as_deref_mut(),
            )?,
            // The STATE remainder, and NOT through the refusal path: see
            // [`Store::load_with_refusals`] for why it keeps the hard failure.
            state: read_document::<StateDocument>(
                &self.path,
                STATE_DOCUMENT,
                Format::Toml,
                STATE_SCHEMA_VERSION,
                &mut reports,
            )?,
        };
        Ok(LoadedStore {
            settings: documents.compose(),
            rollback: reports,
            refusals: refusals.map(std::mem::take).unwrap_or_default(),
        })
    }

    fn read_config<T: DeserializeOwned + Default>(
        &self,
        document: &str,
        version: u32,
        reports: &mut Vec<RollbackReport>,
        refusals: Option<&mut Vec<DocumentRefusal>>,
    ) -> Result<T, SettingsError> {
        let path = self.config_dir.join(document);
        let read = read_document(&path, document, Format::Json, version, reports);
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

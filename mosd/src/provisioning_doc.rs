//! The provisioning DOCUMENT: a versioned, validated, idempotent file that
//! configures a device with no network at all.
//!
//! `docs/design/provisioning.md` section 4 lists five local configuration
//! channels. This module is the machinery behind the first two — a file on the
//! BOOT medium, and a file on removable media — and `docs/design/access.md`
//! section 7 is the preference order they come from. Layer 1
//! ([`crate::provisioning`]) makes an unboxed device *work*; this makes it
//! *ours*, without a cable, a DHCP server or an operator with a browser.
//!
//! # The no-network property, held the same way Layer 1 holds it
//!
//! [`crate::provisioning`]'s module doc explains why seeding must not touch a
//! network, and the same argument applies here with more force: this is the
//! channel that exists *because* there is no network. So this module
//! references no networking API at all — its entire import list is
//! `std::collections`, `std::fmt`, `std::fs`, `std::path`, `anyhow`, `hex`,
//! `ring::digest`, `toml`, `mosd_settings` and [`crate::identity`]. No socket,
//! no resolver, no DHCP lease, no MAC lookup, no wait on a network unit. That
//! is a claim about the source, mechanically checkable by reading it; it is
//! not a proof that the process issues no network syscall, and no unit test
//! can establish that.
//!
//! # Atomicity, and why a power loss cannot half-configure a device
//!
//! Every apply commits through exactly ONE [`Store::save`], for
//! [`crate::provisioning`]'s reason and by the same construction: the document
//! is parsed and TOTALLY validated first, then written into a private clone of
//! the settings tree, and only after the save returns is the caller's tree
//! replaced. `Store::save` writes a temporary file, fsyncs it, renames it over
//! the target and fsyncs the directory, so the rename is the commit point and
//! it is atomic on every filesystem mos uses. A power loss therefore leaves
//! either the old settings file or the new one, never a blend of the two, and
//! a failure at any step before the rename leaves STATE and the caller
//! untouched — the device stays exactly as configured as it was, and the next
//! boot offers it the same document again.
//!
//! Validation being TOTAL is the other half of that guarantee. A document with
//! one bad field applies NOTHING: no field is written into the settings tree
//! until every field has passed, so "half-configured" is not a state this code
//! can produce even in RAM.
//!
//! # Idempotence
//!
//! The version and a digest of the CANONICAL document are recorded in
//! `provisioning.document` (settings schema v10). An offered document whose
//! digest matches is [`Outcome::Unchanged`] and nothing is applied and nothing
//! is written — so a device left with the stick in its socket does not rewrite
//! STATE on every boot. The digest is over a canonical rendering of the
//! *parsed* document, so a comment, a reordered key or different indentation
//! in the source file is the same document.
//!
//! Applying a document never regenerates a credential
//! [`crate::identity`] minted, and never moves
//! `provisioning.seededGeneration`: neither is a field this document can
//! carry.
//!
//! # Secrets
//!
//! Two document fields are secret-bearing: `admin.password` and
//! `wifi.networks[].psk`. No value of either reaches a log line, a status
//! field, the import record or an error message, and that is a property of
//! construction rather than of care:
//!
//! - a rejection is a [`Rejection`], which carries a KEY PATH and a fixed
//!   reason, never a value. The reasons for the two secret-bearing keys come
//!   from [`mosd_settings::validate_wifi_psk`], which deliberately names
//!   neither the value nor its length, and from [`too_short_password`], which
//!   is written here to the same rule;
//! - the parser's own message is never carried. `toml` reports a type error by
//!   quoting the offending literal, and the offending literal may be the
//!   administrator password, so a shape failure is reported as the key path
//!   plus [`SHAPE_REASON`] and the parser's text is dropped on the floor;
//! - the import record ([`mosd_settings::ProvisioningImport`]) holds a source,
//!   an outcome, a reason and a clock reading. It holds no value the document
//!   carried.
//!
//! `provisioning_doc_reports_carry_no_secret_from_the_document` drives a
//! document containing a sentinel through the whole path and asserts the
//! sentinel appears in nothing emitted.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mosd_settings::{
    AuthorizedKey, IfaceSettings, MIN_ADMIN_PASSWORD_LEN, ProvisioningDocumentSettings,
    ProvisioningImport, Settings, Store, TimeSettings, WifiClientSettings, is_wpa_quotable,
    parse_authorized_key, validate_authorized_keys, validate_device_id, validate_ntp_servers,
    validate_timezone_name, validate_wifi_psk,
};

use crate::identity;

/// The document schema version this build applies.
///
/// The DOCUMENT's own version, in its own first field, independent of
/// `mosd_settings::SCHEMA_VERSION`: a document format revision does not
/// reshape the settings tree, and a settings bump does not invalidate a
/// document an operator already wrote onto a card.
pub const DOCUMENT_VERSION: u32 = 1;

/// The one file name a document may have, at the root of a source.
pub const DOCUMENT_FILE_NAME: &str = "mos-provisioning.toml";

/// Where the transport unit stages the sources it found, read-only.
///
/// `/run` and not `/mnt`: the staging tree is per-boot, it must never survive
/// into a state anything else reads, and `/run` is the tmpfs systemd
/// guarantees exists before any unit that could mount into it.
pub const DEFAULT_STAGING_ROOT: &str = "/run/mos/provisioning";

/// The largest document this daemon will read.
///
/// The medium is operator-supplied and is read at boot by a daemon whose
/// failure stops the device from being manageable, so the size is bounded
/// before the bytes are: a stick carrying a 4 GiB file named
/// `mos-provisioning.toml` must be a legible refusal and not an
/// out-of-memory kill during early boot. 64 KiB is two orders of magnitude
/// past any real document — the largest one this schema can express is a
/// device id, a password, 32 authorized keys and a handful of networks.
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024;

/// What a shape failure reports, in place of the parser's own message.
///
/// `toml`'s type errors quote the offending literal (`invalid type: integer
/// 5, expected a string`), and the offending literal may be the administrator
/// password. The key path says where to look; a TOML linter run against the
/// file before it goes on the medium says what is wrong with it.
pub const SHAPE_REASON: &str = "this key does not have the shape the document schema requires (the parser's own message is \
     not repeated here: it would quote the offending value, and a value in this document may be \
     a secret)";

/// Which transport offered a document.
///
/// Two, in preference order, matching `docs/design/access.md` section 7's
/// first two paths. The BOOT medium wins: physical possession of it already
/// implies full control of the device, so a document found there is the most
/// authoritative one available, and a stick left in a socket cannot displace
/// the file the person who flashed the card wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The BOOT partition, as the transport unit staged it.
    Boot,
    /// An attached removable device, as the transport unit staged it.
    Media,
}

/// The sources consulted, in the order they are consulted.
pub const SOURCES: [Source; 2] = [Source::Boot, Source::Media];

impl Source {
    /// The name this source is recorded and reported under.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Media => "media",
        }
    }

    /// The directory under the staging root this source is staged at.
    ///
    /// The same string as [`Source::as_str`], and deliberately a separate
    /// method: the recorded name is a wire value the status route serves and
    /// the directory name is a contract with the transport unit, and the day
    /// one has to change the other must not follow it silently.
    #[must_use]
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Media => "media",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a document was refused.
///
/// **A key path and a reason, never a value.** Both halves are load-bearing:
/// the key path is what makes a refusal actionable to whoever wrote the file,
/// and the absence of the value is what makes it safe to log, record and
/// serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Dotted path of the offending key, or empty when the document as a whole
    /// is the problem (unreadable, not TOML).
    pub key: String,
    /// What is wrong with it.
    pub reason: String,
}

impl Rejection {
    fn at(key: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            reason: reason.into(),
        }
    }

    fn whole(reason: impl Into<String>) -> Self {
        Self {
            key: String::new(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.key.is_empty() {
            formatter.write_str(&self.reason)
        } else {
            write!(formatter, "`{}`: {}", self.key, self.reason)
        }
    }
}

/// What [`import`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Neither source carried a document. Nothing was read, written or
    /// recorded — a device that boots without a document must not lose the
    /// record of the one it applied last time.
    NoDocument,
    /// A document was applied and committed.
    Applied {
        /// Which source it came from.
        source: Source,
        /// Its `version`.
        version: u32,
        /// Its canonical digest.
        digest: String,
    },
    /// The offered document is the one already applied; nothing was written.
    Unchanged {
        /// Which source offered it.
        source: Source,
        /// Its canonical digest, which is the recorded one.
        digest: String,
    },
    /// The document was refused. Nothing it names was applied; the only thing
    /// written is the import record itself.
    Rejected {
        /// Which source offered it.
        source: Source,
        /// Why.
        rejection: Rejection,
    },
}

/// The refusal a short administrator password earns.
///
/// Its own function so the sentence exists once, and so the rule that it names
/// neither the value nor its length is stated where it is enforced: this
/// string reaches a log, the import record and an HTTP client.
fn too_short_password() -> Rejection {
    Rejection::at(
        "admin.password",
        format!(
            "an administrator bootstrap password is at least {MIN_ADMIN_PASSWORD_LEN} characters"
        ),
    )
}

/// The provisioning document, as parsed.
///
/// Every section maps onto an EXISTING settings path, and three of them are
/// the settings types themselves — [`IfaceSettings`], [`WifiClientSettings`]
/// and [`TimeSettings`] — so the document cannot express a network, a WiFi
/// station or a time configuration the settings tree cannot hold, and there is
/// no second grammar to keep in step with the first.
///
/// **There is deliberately no certificate section.** PLAN-046 lists
/// certificates among the things a provisioning document should carry, and
/// there is no settings path to carry them onto: the only certificate on the
/// device is apid's self-signed TLS pair, which is a file pair on STATE
/// (`apid`'s `tls.rs`), not a setting. Inventing a settings key for it here
/// would be inventing a setting, so a document carrying one is REFUSED naming
/// the key. Same for a hostname: the device names itself from its identity
/// ([`crate::provisioning`]), and there is no hostname predicate in
/// `mosd_settings` to reuse, so the document injects the identity and lets
/// Layer 1 derive the name.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ProvisioningDocument {
    /// The document schema version; must equal [`DOCUMENT_VERSION`].
    pub version: u32,
    /// Device identity the factory injects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentitySection>,
    /// Administrator bootstrap credential material.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin: Option<AdminSection>,
    /// Per-interface network configuration, written to `network`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<BTreeMap<String, IfaceSettings>>,
    /// WiFi station configuration, written to `wifi.client`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi: Option<WifiClientSettings>,
    /// NTP servers and the presentation timezone, written to `time`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<TimeSettings>,
}

/// `[identity]`: what the factory injects, and what it must not.
///
/// The identifier only. A device identity is drawn from the system CSPRNG on
/// the device at first boot ([`crate::identity`]) unless a factory supplies
/// one, and this section is that supply. Nothing here is GENERATED: a document
/// that omits it leaves Layer 1 to mint one, and a document that carries it
/// hands Layer 1 an identity to keep. The two per-device secrets are not here
/// and never will be — `docs/design/provisioning.md` section 3.1 requires them
/// to be drawn on the device.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct IdentitySection {
    /// `provisioning.deviceId`: 32 lowercase hex characters.
    #[serde(rename = "deviceId", skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

/// `[admin]`: how the first administrator gets in.
///
/// **Secret-bearing.** `password` is the plaintext the operator will type; it
/// is hashed with Argon2id ([`identity::hash_password`]) into
/// `access.webAdmin.password_hash` and the plaintext is dropped. It is never
/// stored, logged or echoed.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AdminSection {
    /// The administrator bootstrap password, plaintext. SECRET.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Authorized-key lines, in `authorized_keys` syntax, written to
    /// `access.ssh.authorizedKeys`. Declarative: the listed set REPLACES the
    /// stored one, which on the unclaimed device this document applies to is
    /// empty.
    #[serde(rename = "authorizedKeys", skip_serializing_if = "Option::is_none")]
    pub authorized_keys: Option<Vec<String>>,
}

/// Top-level keys a document may carry.
const DOCUMENT_KEYS: [&str; 6] = ["version", "identity", "admin", "network", "wifi", "time"];

/// Keys `[identity]` may carry.
const IDENTITY_KEYS: [&str; 1] = ["deviceId"];

/// Keys `[admin]` may carry.
const ADMIN_KEYS: [&str; 2] = ["password", "authorizedKeys"];

/// Read, validate and apply a provisioning document, if one is offered.
///
/// Consults [`SOURCES`] in order under `staging_root` and takes the FIRST that
/// carries a document; the other is not read. On success `settings` is
/// replaced with the applied tree, so the caller's first reconcile already
/// sees it.
///
/// The whole document is validated before any of it is applied, and the apply
/// commits through exactly one [`Store::save`] — see the module doc for why
/// that is what makes a power loss mid-apply harmless.
///
/// A refusal is [`Outcome::Rejected`] and not an error: a bad file on a stick
/// must leave a working, unclaimed appliance and a legible record, never a
/// daemon that will not start.
///
/// # Errors
///
/// Only when [`Store::save`] fails — the settings file could not be written.
/// STATE and `settings` are left exactly as they were.
pub fn import(store: &Store, settings: &mut Settings, staging_root: &Path) -> Result<Outcome> {
    let Some((source, path)) = find_document(staging_root) else {
        return Ok(Outcome::NoDocument);
    };
    tracing::info!(%source, path = %path.display(), "provisioning document offered");

    let document = match read_document(&path) {
        Ok(document) => document,
        Err(rejection) => return reject(store, settings, source, rejection),
    };
    if let Err(rejection) = validate(&document) {
        return reject(store, settings, source, rejection);
    }
    let digest = digest_of(&document);

    // The short-circuit, BEFORE the claim gate: a device that was claimed by
    // the very document being offered must report `unchanged`, not
    // `already-claimed`. Nothing is written on this path at all, so a stick
    // left in a socket costs no flash write per boot.
    if settings
        .provisioning
        .document
        .as_ref()
        .and_then(|record| record.applied_digest.as_deref())
        == Some(digest.as_str())
    {
        let outcome = Outcome::Unchanged { source, digest };
        record_attempt(store, settings, source, &outcome)?;
        tracing::info!(%source, "provisioning document already applied; nothing to do");
        return Ok(outcome);
    }

    // The claim gate. A document is the FIRST-RUN channel: once an
    // administrator credential exists the device is claimed, and reconfiguring
    // it goes through the authenticated API. Neither transport carries a
    // signature, so without this gate a stick pushed into a fielded device
    // would reconfigure it — including its administrator password.
    if settings.access.web_admin.is_some() {
        return reject(
            store,
            settings,
            source,
            Rejection::whole(
                "this device is already claimed: it has an administrator credential, so \
                 configuration changes go through the authenticated API rather than through a \
                 provisioning document",
            ),
        );
    }

    // Everything from here mutates a private clone. `*settings` is replaced
    // only after the save that commits the tree has returned.
    let mut applied = settings.clone();
    if let Err(rejection) = apply_into(&document, &mut applied) {
        return reject(store, settings, source, rejection);
    }
    let outcome = Outcome::Applied {
        source,
        version: document.version,
        digest: digest.clone(),
    };
    applied.provisioning.document = Some(ProvisioningDocumentSettings {
        applied_version: Some(document.version),
        applied_digest: Some(digest),
        last_import: Some(import_record(source, &outcome)),
    });
    store
        .save(&applied)
        .context("persist the applied provisioning document")?;
    tracing::info!(
        %source,
        version = document.version,
        "provisioning document applied"
    );
    *settings = applied;
    Ok(outcome)
}

/// Record a refusal and return it. Nothing the document named is applied.
fn reject(
    store: &Store,
    settings: &mut Settings,
    source: Source,
    rejection: Rejection,
) -> Result<Outcome> {
    tracing::warn!(
        %source,
        key = rejection.key,
        reason = rejection.reason,
        "provisioning document refused; the device is unchanged"
    );
    let outcome = Outcome::Rejected { source, rejection };
    record_attempt(store, settings, source, &outcome)?;
    Ok(outcome)
}

/// Persist the import record for an attempt that applied nothing.
///
/// The record is the ONLY thing this writes, and it is written only when it
/// would say something different from what is already there — a device booting
/// again and again against the same medium writes STATE once, not once per
/// boot. `at` moves only with the rest of the record, so a stable record is a
/// stable file.
///
/// # Errors
///
/// As [`Store::save`].
fn record_attempt(
    store: &Store,
    settings: &mut Settings,
    source: Source,
    outcome: &Outcome,
) -> Result<()> {
    let record = import_record(source, outcome);
    let existing = settings
        .provisioning
        .document
        .as_ref()
        .and_then(|document| document.last_import.as_ref());
    if let Some(existing) = existing
        && existing.source == record.source
        && existing.outcome == record.outcome
        && existing.reason == record.reason
    {
        return Ok(());
    }
    let mut updated = settings.clone();
    let document = updated
        .provisioning
        .document
        .get_or_insert_with(ProvisioningDocumentSettings::default);
    document.last_import = Some(record);
    store
        .save(&updated)
        .context("persist the provisioning import record")?;
    *settings = updated;
    Ok(())
}

/// The settings record for one attempt.
fn import_record(source: Source, outcome: &Outcome) -> ProvisioningImport {
    let (name, reason) = match outcome {
        Outcome::Applied { .. } => ("applied", None),
        Outcome::Unchanged { .. } => ("unchanged", None),
        Outcome::Rejected { rejection, .. } => ("rejected", Some(rejection.to_string())),
        // Not reachable: `NoDocument` records nothing. Spelled rather than
        // `unreachable!()` so a future caller cannot panic a boot path.
        Outcome::NoDocument => ("none", None),
    };
    ProvisioningImport {
        source: source.as_str().to_string(),
        outcome: name.to_string(),
        reason,
        at: now_seconds(),
    }
}

/// The device clock as seconds since the epoch, saturating at 0.
///
/// A label and never a deadline: the import runs before any time source has
/// been consulted, so this reading is whatever the clock happened to say.
fn now_seconds() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0)
}

/// The first source under `staging_root` carrying a document.
///
/// **This is the whole of "where a document may come from".** The path is
/// built from a fixed staging root, a fixed per-source directory name and a
/// fixed file name; nothing in it comes from the document, from the medium or
/// from an operator, so there is no path for a caller to traverse out of. The
/// entry must be a REGULAR file and not a symbolic link: a link planted on
/// operator media would otherwise name a path on the device, and reading a
/// character device would hang early boot.
fn find_document(staging_root: &Path) -> Option<(Source, PathBuf)> {
    SOURCES.into_iter().find_map(|source| {
        let path = staging_root
            .join(source.dir_name())
            .join(DOCUMENT_FILE_NAME);
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() {
            tracing::warn!(
                %source,
                path = %path.display(),
                "provisioning document path is not a regular file; ignored"
            );
            return None;
        }
        Some((source, path))
    })
}

/// Read and parse the document at `path`.
fn read_document(path: &Path) -> Result<ProvisioningDocument, Rejection> {
    let size = fs::symlink_metadata(path)
        .map_err(|_| Rejection::whole("the document could not be read"))?
        .len();
    if size > MAX_DOCUMENT_BYTES {
        return Err(Rejection::whole(format!(
            "the document is larger than the {MAX_DOCUMENT_BYTES}-byte maximum"
        )));
    }
    let text = fs::read_to_string(path)
        .map_err(|_| Rejection::whole("the document could not be read as UTF-8 text"))?;
    let table: toml::Table = text
        .parse()
        .map_err(|_: toml::de::Error| Rejection::whole("the document is not valid TOML"))?;
    parse_document(&table)
}

/// Turn a parsed TOML table into a document, key by key.
///
/// By hand rather than by `Deserialize`, for the reason [`SHAPE_REASON`]
/// gives: a derived deserializer reports a type error by quoting the offending
/// literal, and this document's literals include the administrator password.
/// Extracting key by key means every message in this file is one written here.
fn parse_document(table: &toml::Table) -> Result<ProvisioningDocument, Rejection> {
    reject_unknown_keys(table, &DOCUMENT_KEYS, "")?;

    let version = match table.get("version") {
        None => {
            return Err(Rejection::at(
                "version",
                "the document must state its schema version in its first field",
            ));
        }
        Some(toml::Value::Integer(version)) => u32::try_from(*version)
            .map_err(|_| Rejection::at("version", "the document version is out of range"))?,
        Some(_) => return Err(Rejection::at("version", SHAPE_REASON)),
    };

    let identity = match table.get("identity") {
        None => None,
        Some(toml::Value::Table(section)) => {
            reject_unknown_keys(section, &IDENTITY_KEYS, "identity")?;
            Some(IdentitySection {
                device_id: optional_string(section, "deviceId", "identity.deviceId")?,
            })
        }
        Some(_) => return Err(Rejection::at("identity", SHAPE_REASON)),
    };

    let admin = match table.get("admin") {
        None => None,
        Some(toml::Value::Table(section)) => {
            reject_unknown_keys(section, &ADMIN_KEYS, "admin")?;
            Some(AdminSection {
                password: optional_string(section, "password", "admin.password")?,
                authorized_keys: optional_string_list(
                    section,
                    "authorizedKeys",
                    "admin.authorizedKeys",
                )?,
            })
        }
        Some(_) => return Err(Rejection::at("admin", SHAPE_REASON)),
    };

    Ok(ProvisioningDocument {
        version,
        identity,
        admin,
        network: typed_section(table, "network")?,
        wifi: typed_section(table, "wifi")?,
        time: typed_section(table, "time")?,
    })
}

/// Refuse any key of `table` that is not in `allowed`, naming it.
///
/// The precise half of the shape rule, and the one an operator hits: a
/// `[certificates]` section, or `deviceID` for `deviceId`, is named exactly
/// rather than reported as a shape failure of the whole document.
fn reject_unknown_keys(
    table: &toml::Table,
    allowed: &[&str],
    prefix: &str,
) -> Result<(), Rejection> {
    for key in table.keys() {
        if allowed.contains(&key.as_str()) {
            continue;
        }
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        return Err(Rejection::at(
            path,
            format!(
                "the document schema has no such key; it may carry {}",
                allowed.join(", ")
            ),
        ));
    }
    Ok(())
}

/// An optional string field.
fn optional_string(
    table: &toml::Table,
    key: &str,
    path: &str,
) -> Result<Option<String>, Rejection> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(Rejection::at(path, SHAPE_REASON)),
    }
}

/// An optional array-of-strings field.
fn optional_string_list(
    table: &toml::Table,
    key: &str,
    path: &str,
) -> Result<Option<Vec<String>>, Rejection> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                toml::Value::String(value) => Ok(value.clone()),
                _ => Err(Rejection::at(path, SHAPE_REASON)),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(Rejection::at(path, SHAPE_REASON)),
    }
}

/// A section deserialized straight into the settings type that owns it.
///
/// The serde message is dropped for [`SHAPE_REASON`]'s reason. What is lost is
/// precision inside these three sections; what is kept is the guarantee that
/// no message this module produces can quote a value.
fn typed_section<T: serde::de::DeserializeOwned>(
    table: &toml::Table,
    key: &str,
) -> Result<Option<T>, Rejection> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .clone()
            .try_into()
            .map(Some)
            .map_err(|_: toml::de::Error| Rejection::at(key, SHAPE_REASON)),
    }
}

/// Validate the WHOLE document. Nothing is applied until this has passed.
///
/// Every predicate is one `mosd_settings` already states, so a document can
/// express exactly what the settings tree accepts and no more — there is no
/// second grammar here that could drift from the first.
fn validate(document: &ProvisioningDocument) -> Result<(), Rejection> {
    if document.version != DOCUMENT_VERSION {
        return Err(Rejection::at(
            "version",
            format!("this build applies provisioning document version {DOCUMENT_VERSION}"),
        ));
    }
    if let Some(identity) = &document.identity
        && let Some(device_id) = &identity.device_id
    {
        validate_device_id(device_id)
            .map_err(|reason| Rejection::at("identity.deviceId", reason))?;
    }
    if let Some(admin) = &document.admin {
        if let Some(password) = &admin.password
            && password.len() < MIN_ADMIN_PASSWORD_LEN
        {
            return Err(too_short_password());
        }
        if let Some(lines) = &admin.authorized_keys {
            validate_authorized_keys(&parse_keys(lines)?)
                .map_err(|err| Rejection::at("admin.authorizedKeys", refusal_sentence(&err)))?;
        }
    }
    if let Some(wifi) = &document.wifi {
        for (index, network) in wifi.networks.iter().enumerate() {
            let path = format!("wifi.networks[{index}]");
            if network.ssid.is_empty() || !is_wpa_quotable(&network.ssid) {
                return Err(Rejection::at(
                    format!("{path}.ssid"),
                    "a network name must be non-empty printable ASCII without a quote or a \
                     backslash, which is what wpa_supplicant configuration can carry",
                ));
            }
            if let Some(psk) = &network.psk {
                validate_wifi_psk(psk)
                    .map_err(|reason| Rejection::at(format!("{path}.psk"), reason))?;
            }
        }
    }
    if let Some(time) = &document.time {
        validate_ntp_servers(&time.ntp.servers)
            .map_err(|reason| Rejection::at("time.ntp.servers", reason))?;
        validate_timezone_name(&time.timezone)
            .map_err(|reason| Rejection::at("time.timezone", reason))?;
    }
    Ok(())
}

/// Parse every authorized-key line, or refuse naming the entry.
fn parse_keys(lines: &[String]) -> Result<Vec<AuthorizedKey>, Rejection> {
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            parse_authorized_key(line).map_err(|err| {
                Rejection::at(
                    format!("admin.authorizedKeys[{index}]"),
                    refusal_sentence(&err),
                )
            })
        })
        .collect()
}

/// The sentence a `SettingsError` refusal carries.
///
/// `parse_authorized_key` and `validate_authorized_keys` both document that
/// they never echo the rejected input, so their message is safe to carry.
fn refusal_sentence(err: &mosd_settings::SettingsError) -> String {
    match err {
        mosd_settings::SettingsError::Validation { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// Write a validated document into `settings`.
///
/// Every write goes through [`Settings::set`], which deserializes the whole
/// candidate tree and runs the tree's own write predicates (the interface-name
/// charset, the `time` subtree's rules). So a document cannot produce a
/// settings tree the API would have refused, and the checks are not restated
/// here.
///
/// `settings` is a private clone of the caller's tree. A failure part-way
/// leaves that clone half-written and the clone is then discarded, so nothing
/// half-written is ever committed or observed.
fn apply_into(document: &ProvisioningDocument, settings: &mut Settings) -> Result<(), Rejection> {
    if let Some(identity) = &document.identity
        && let Some(device_id) = &identity.device_id
    {
        set(settings, "provisioning.deviceId", device_id.as_str().into())?;
    }
    if let Some(admin) = &document.admin {
        if let Some(password) = &admin.password {
            // Argon2id, the format `access.webAdmin.password_hash` holds and
            // apid verifies against. The plaintext is dropped here: it is not
            // stored, and there is deliberately no path that could read it
            // back.
            let hash = identity::hash_password(password).map_err(|_| {
                Rejection::at(
                    "admin.password",
                    "the administrator password could not be hashed",
                )
            })?;
            set(
                settings,
                "access.webAdmin",
                serde_json::json!({ "password_hash": hash }),
            )?;
        }
        if let Some(lines) = &admin.authorized_keys {
            let keys = parse_keys(lines)?;
            let value = serde_json::to_value(&keys).map_err(|_| {
                Rejection::at("admin.authorizedKeys", "the key list could not be encoded")
            })?;
            set(settings, "access.ssh.authorizedKeys", value)?;
        }
    }
    if let Some(network) = &document.network {
        set(settings, "network", encode(network, "network")?)?;
    }
    if let Some(wifi) = &document.wifi {
        set(settings, "wifi.client", encode(wifi, "wifi")?)?;
    }
    if let Some(time) = &document.time {
        set(settings, "time", encode(time, "time")?)?;
    }
    Ok(())
}

/// One dot-path write, refusing with the settings tree's own sentence.
fn set(settings: &mut Settings, path: &str, value: serde_json::Value) -> Result<(), Rejection> {
    settings.set(path, value).map_err(|err| {
        Rejection::at(
            document_key_for(path),
            match err {
                mosd_settings::SettingsError::Validation { message, .. } => message,
                other => other.to_string(),
            },
        )
    })
}

/// The DOCUMENT key a settings dot-path came from.
///
/// A refusal has to name a key the person holding the file can find, and the
/// settings path and the document path are not the same string for three of
/// the six sections.
fn document_key_for(path: &str) -> &str {
    match path {
        "provisioning.deviceId" => "identity.deviceId",
        "access.webAdmin" => "admin.password",
        "access.ssh.authorizedKeys" => "admin.authorizedKeys",
        "wifi.client" => "wifi",
        other => other,
    }
}

/// A section as JSON, for [`Settings::set`].
fn encode<T: serde::Serialize>(value: &T, key: &str) -> Result<serde_json::Value, Rejection> {
    serde_json::to_value(value).map_err(|_| Rejection::at(key, "this section could not be encoded"))
}

/// The canonical digest of a document: SHA-256 over its canonical rendering,
/// lowercase hex.
///
/// Canonical means re-rendered from the PARSED document rather than hashed as
/// it was written, so whitespace, comments and key order in the source file do
/// not change the answer, and one document written twice by two people is one
/// document.
///
/// The digest covers the secret-bearing fields as well, deliberately. Omitting
/// them would make two documents that differ only in the administrator
/// password one document, and the second would then be short-circuited as
/// `unchanged` and never applied — a credential silently not rotated is a
/// worse failure than the one omitting them would avoid. What that costs is
/// named rather than hidden: an authenticated reader of
/// `GET /api/v1/provisioning/status` can confirm a guess at the WHOLE document
/// (every field of it, including the password) by hashing their guess. That
/// reader is an administrator who can already read the settings the document
/// wrote.
#[must_use]
pub fn digest_of(document: &ProvisioningDocument) -> String {
    let canonical = toml::to_string(document).unwrap_or_default();
    hex::encode(ring::digest::digest(
        &ring::digest::SHA256,
        canonical.as_bytes(),
    ))
}

/// The staging root to read, honouring the `MOSD_PROVISIONING_ROOT` test hook.
#[must_use]
pub fn staging_root_from_env() -> PathBuf {
    std::env::var_os("MOSD_PROVISIONING_ROOT")
        .map_or_else(|| PathBuf::from(DEFAULT_STAGING_ROOT), PathBuf::from)
}

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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use mosd_settings::{ProvisioningState, encode_base64_nopad};
    use tempfile::TempDir;

    use super::*;

    /// A password the sentinel test can find anywhere it leaked, and which is
    /// long enough to be accepted.
    const SECRET_PASSWORD: &str = "PW-SENTINEL-8a3f-do-not-log";

    /// The same, for the WiFi pre-shared key. A DIFFERENT string, so a test
    /// that finds one cannot be satisfied by the other.
    const SECRET_PSK: &str = "PSK-SENTINEL-4c7e";

    /// The same sentinel, padded past IEEE 802.11i's longest passphrase. Used
    /// where a REFUSAL about a pre-shared key is wanted: the refusal must name
    /// neither the value nor its length, and this is the string that proves it.
    fn unusable_psk() -> String {
        format!("{SECRET_PSK}{}", "x".repeat(50))
    }

    fn settings_path(dir: &Path) -> PathBuf {
        dir.join("settings.toml")
    }

    fn store_in(dir: &Path) -> Store {
        // The `/mos/config/` namespace has to exist: an absent document is a
        // default, an absent namespace is the DATA medium being gone, and the
        // store refuses that rather than defaulting (PLAN-070 §5.2.6).
        let config = dir.join("config");
        fs::create_dir_all(&config).expect("create the config namespace");
        Store::new(settings_path(dir), config)
    }

    /// Write `body` as the document of `source`, under a fresh staging root.
    fn stage(dir: &Path, source: Source, body: &str) -> PathBuf {
        let root = dir.join("staging");
        let source_dir = root.join(source.dir_name());
        fs::create_dir_all(&source_dir).expect("create staging dir");
        fs::write(source_dir.join(DOCUMENT_FILE_NAME), body).expect("write document");
        root
    }

    /// A structurally valid authorized-key line, built from the crate's own
    /// encoder rather than pasted from anywhere.
    fn key_line() -> String {
        let key_type = "ssh-ed25519";
        let mut bytes = Vec::new();
        let name = key_type.as_bytes();
        bytes.extend_from_slice(
            &u32::try_from(name.len())
                .expect("name length")
                .to_be_bytes(),
        );
        bytes.extend_from_slice(name);
        while bytes.len() < 64 {
            let index = u8::try_from(bytes.len()).expect("index fits");
            bytes.push(index.wrapping_mul(11).wrapping_add(5));
        }
        let mut encoded = encode_base64_nopad(&bytes);
        while !encoded.len().is_multiple_of(4) {
            encoded.push('=');
        }
        format!("{key_type} {encoded}")
    }

    /// A document exercising every section, secrets included.
    fn full_document() -> String {
        format!(
            r#"
# A factory document, with the comments and blank lines a real one carries.
version = 1

[identity]
deviceId = "0123456789abcdef0123456789abcdef"

[admin]
password = "{SECRET_PASSWORD}"
authorizedKeys = ["{key}"]

[network.eth0]
dhcp = true

[wifi]
enabled = true
interface = "wlan0"

[[wifi.networks]]
ssid = "site-ap"
psk = "{SECRET_PSK}"
priority = 10

[time]
timezone = "Europe/Berlin"

[time.ntp]
servers = ["0.pool.ntp.org", "192.0.2.7"]
"#,
            key = key_line()
        )
    }

    /// The smallest document that does anything, and which claims nothing.
    fn time_only_document() -> &'static str {
        r#"
version = 1

[time]
timezone = "Europe/Berlin"
"#
    }

    /// Import `body` from `source` against a fresh STATE, returning the store,
    /// the resulting tree and the outcome.
    fn import_body(dir: &Path, source: Source, body: &str) -> (Store, Settings, Outcome) {
        let store = store_in(dir);
        let root = stage(dir, source, body);
        let mut settings = Settings::default();
        let outcome = import(&store, &mut settings, &root).expect("import");
        (store, settings, outcome)
    }

    // The happy path, end to end: every section lands where it belongs, the
    // record names the document, and the tree really reached STATE.
    #[test]
    fn a_boot_document_is_applied_and_persisted() {
        let dir = TempDir::new().expect("tempdir");
        let (store, settings, outcome) = import_body(dir.path(), Source::Boot, &full_document());

        let Outcome::Applied {
            source,
            version,
            digest,
        } = outcome
        else {
            panic!("expected an applied document, got {outcome:?}");
        };
        assert_eq!(source, Source::Boot);
        assert_eq!(version, DOCUMENT_VERSION);

        assert_eq!(
            settings.provisioning.device_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert!(
            settings
                .access
                .web_admin
                .as_ref()
                .expect("the admin credential is set")
                .password_hash
                .starts_with("$argon2id$"),
            "the bootstrap password must be stored as an Argon2id hash"
        );
        assert_eq!(settings.access.ssh.authorized_keys.len(), 1);
        assert!(settings.network.get("eth0").expect("eth0 configured").dhcp);
        assert!(settings.wifi.client.enabled);
        assert_eq!(settings.wifi.client.networks[0].ssid, "site-ap");
        assert_eq!(settings.time.timezone, "Europe/Berlin");
        assert_eq!(settings.time.ntp.servers.len(), 2);

        let record = settings
            .provisioning
            .document
            .as_ref()
            .expect("a document record");
        assert_eq!(record.applied_version, Some(DOCUMENT_VERSION));
        assert_eq!(record.applied_digest.as_deref(), Some(digest.as_str()));
        let import_record = record.last_import.as_ref().expect("an import record");
        assert_eq!(import_record.source, "boot");
        assert_eq!(import_record.outcome, "applied");
        assert_eq!(import_record.reason, None);

        // The one save really happened, and it holds the same tree.
        assert_eq!(store.load().expect("reload"), settings);

        // Nothing seeding owns was moved: this ran before seeding, so the
        // state is still pending and the generation still zero.
        assert_eq!(settings.provisioning.state, ProvisioningState::Pending);
        assert_eq!(settings.provisioning.seeded_generation, 0);
    }

    // The idempotence criterion, asserted on the bytes: a second import writes
    // NOTHING at all, so a device left with the medium in place does not burn
    // a flash write per boot.
    #[test]
    fn re_applying_an_identical_document_is_a_proven_no_op() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let root = stage(dir.path(), Source::Boot, &full_document());
        let mut settings = Settings::default();

        let first = import(&store, &mut settings, &root).expect("first import");
        assert!(matches!(first, Outcome::Applied { .. }), "{first:?}");
        let first_tree = settings.clone();
        let first_bytes = fs::read(settings_path(dir.path())).expect("read settings file");

        // Reload from STATE, exactly as the next boot would.
        let mut second = store.load().expect("reload");
        assert_eq!(second, first_tree);
        let outcome = import(&store, &mut second, &root).expect("second import");
        let Outcome::Unchanged { source, digest } = outcome else {
            panic!("expected `unchanged`, got {outcome:?}");
        };
        assert_eq!(source, Source::Boot);
        assert_eq!(
            Some(digest.as_str()),
            first_tree
                .provisioning
                .document
                .as_ref()
                .and_then(|record| record.applied_digest.as_deref())
        );

        // NOTHING the document names moved. The one thing a second import does
        // write is the attempt record, so it is excluded here and asserted
        // directly below — the tree is otherwise the tree the first import
        // produced, byte for byte in the settings the document owns.
        let mut second_without_record = second.clone();
        let mut first_without_record = first_tree.clone();
        for tree in [&mut second_without_record, &mut first_without_record] {
            if let Some(document) = tree.provisioning.document.as_mut() {
                document.last_import = None;
            }
        }
        assert_eq!(
            second_without_record, first_without_record,
            "a second import must move nothing the document names"
        );
        let attempt = second
            .provisioning
            .document
            .as_ref()
            .and_then(|record| record.last_import.as_ref())
            .expect("an import record");
        assert_eq!(attempt.outcome, "unchanged");
        assert_eq!(attempt.reason, None);
        assert_ne!(
            fs::read(settings_path(dir.path())).expect("re-read settings file"),
            first_bytes,
            "the second import records that it found nothing to do"
        );

        // And a THIRD import writes nothing at all: a device left with the
        // medium in its socket must not burn a flash write per boot.
        let second_bytes = fs::read(settings_path(dir.path())).expect("read settings file");
        let mut third = store.load().expect("reload");
        assert!(matches!(
            import(&store, &mut third, &root).expect("third import"),
            Outcome::Unchanged { .. }
        ));
        assert_eq!(third, second);
        assert_eq!(
            fs::read(settings_path(dir.path())).expect("re-read settings file"),
            second_bytes,
            "settings.toml must be byte-identical from the second import on"
        );
    }

    // The digest is over the CANONICAL document: the same configuration
    // written differently is the same document. Without this, an operator who
    // reformatted the file on the medium would re-claim the device.
    #[test]
    fn the_digest_ignores_comments_whitespace_and_section_order() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let original = "version = 1\n\n[identity]\ndeviceId = \"0123456789abcdef0123456789abcdef\"\n\n                        [time]\ntimezone = \"Europe/Berlin\"\n";
        let root = stage(dir.path(), Source::Boot, original);
        let mut settings = Settings::default();
        assert!(matches!(
            import(&store, &mut settings, &root).expect("first"),
            Outcome::Applied { .. }
        ));

        // The same document, rewritten by hand: a comment, different spacing,
        // and the two sections in the other order.
        let rewritten = "# rewritten by an operator\nversion=1\n\n[time]\n\ntimezone   =   \
                         \"Europe/Berlin\"\n\n[identity]\ndeviceId=\"0123456789abcdef0123456789abcdef\"\n";
        let root = stage(dir.path(), Source::Boot, rewritten);
        let mut reloaded = store.load().expect("reload");
        let outcome = import(&store, &mut reloaded, &root).expect("second");
        assert!(
            matches!(outcome, Outcome::Unchanged { .. }),
            "a reformatted document is the same document, got {outcome:?}"
        );

        // And a document that differs in a VALUE is a different document.
        let root = stage(
            dir.path(),
            Source::Boot,
            "version = 1\n\n[time]\ntimezone = \"UTC\"\n",
        );
        let mut reloaded = store.load().expect("reload");
        let outcome = import(&store, &mut reloaded, &root).expect("third");
        assert!(
            matches!(outcome, Outcome::Applied { .. }),
            "a changed value must not be short-circuited, got {outcome:?}"
        );
        assert_eq!(reloaded.time.timezone, "UTC");
    }

    /// Every way a document can be wrong, with the key path its refusal must
    /// name. Driven as one table so a new rule is one row.
    fn invalid_documents() -> Vec<(&'static str, String, &'static str)> {
        vec![
            (
                "no version field",
                "[time]\ntimezone = \"UTC\"\n".to_string(),
                "version",
            ),
            (
                "a version this build does not apply",
                "version = 2\n".to_string(),
                "version",
            ),
            (
                "a version that is not an integer",
                "version = \"1\"\n".to_string(),
                "version",
            ),
            (
                "a section the schema does not have",
                "version = 1\n\n[certificates]\nca = \"x\"\n".to_string(),
                "certificates",
            ),
            (
                "a misspelled key inside a section",
                "version = 1\n\n[identity]\ndeviceID = \"0123456789abcdef0123456789abcdef\"\n"
                    .to_string(),
                "identity.deviceID",
            ),
            (
                "a device identifier that is not 32 lowercase hex",
                "version = 1\n\n[identity]\ndeviceId = \"0123456789ABCDEF0123456789abcdef\"\n"
                    .to_string(),
                "identity.deviceId",
            ),
            (
                "a device identifier of the wrong length",
                "version = 1\n\n[identity]\ndeviceId = \"abc\"\n".to_string(),
                "identity.deviceId",
            ),
            (
                "an administrator password below the floor",
                "version = 1\n\n[admin]\npassword = \"short\"\n".to_string(),
                "admin.password",
            ),
            (
                "an authorized key that is not one",
                "version = 1\n\n[admin]\nauthorizedKeys = [\"command=/bin/sh ssh-ed25519 AAAA\"]\n"
                    .to_string(),
                "admin.authorizedKeys[0]",
            ),
            (
                "an interface name that is not one",
                "version = 1\n\n[network.\"eth 0\"]\ndhcp = true\n".to_string(),
                "network",
            ),
            (
                "a network entry of the wrong shape",
                "version = 1\n\n[network.eth0]\ndhcp = \"yes\"\n".to_string(),
                "network",
            ),
            (
                "a WiFi network with no name",
                "version = 1\n\n[[wifi.networks]]\nssid = \"\"\n".to_string(),
                "wifi.networks[0].ssid",
            ),
            (
                "a pre-shared key no supplicant could use",
                format!(
                    "version = 1\n\n[[wifi.networks]]\nssid = \"s\"\npsk = \"{}\"\n",
                    unusable_psk()
                ),
                "wifi.networks[0].psk",
            ),
            (
                "an NTP server that could smuggle a second assignment",
                "version = 1\n\n[time.ntp]\nservers = [\"pool one\"]\n".to_string(),
                "time.ntp.servers",
            ),
            (
                "a timezone that is not an IANA zone name",
                "version = 1\n\n[time]\ntimezone = \"../etc/passwd\"\n".to_string(),
                "time.timezone",
            ),
        ]
    }

    // The acceptance criterion, stated twice over: one bad field applies
    // NOTHING, and the refusal names the offending KEY PATH.
    //
    // "Applies nothing" is asserted against the tree the import started from,
    // with the import record excluded — the record is the one thing a refusal
    // does write, because a refusal nobody can see is not a legible failure.
    #[test]
    fn a_document_with_one_bad_field_applies_nothing_and_names_the_key() {
        for (label, body, key) in invalid_documents() {
            let dir = TempDir::new().expect("tempdir");
            let store = store_in(dir.path());
            let root = stage(dir.path(), Source::Boot, &body);
            let mut settings = Settings::default();

            let outcome = import(&store, &mut settings, &root).expect("import");
            let Outcome::Rejected { source, rejection } = outcome else {
                panic!("{label}: expected a rejection, got {outcome:?}");
            };
            assert_eq!(source, Source::Boot);
            assert_eq!(
                rejection.key, key,
                "{label}: wrong key path in {rejection:?}"
            );
            assert!(!rejection.reason.is_empty(), "{label}: no reason given");

            let mut without_record = settings.clone();
            without_record.provisioning.document = None;
            assert_eq!(
                without_record,
                Settings::default(),
                "{label}: a refused document moved something"
            );

            let record = settings
                .provisioning
                .document
                .as_ref()
                .expect("a record of the refusal");
            assert_eq!(record.applied_version, None);
            assert_eq!(record.applied_digest, None);
            let attempt = record.last_import.as_ref().expect("an import record");
            assert_eq!(attempt.outcome, "rejected");
            let reason = attempt.reason.as_deref().expect("a reason");
            assert!(
                reason.contains(key),
                "{label}: the recorded reason must name the key, got {reason:?}"
            );

            // And the device is still a working, UNCLAIMED appliance: nothing
            // about a refused document may take it out of setup mode.
            assert!(settings.access.web_admin.is_none(), "{label}");
            assert_eq!(store.load().expect("reload"), settings, "{label}");
        }
    }

    // The secret-safety acceptance criterion. A document carrying two known
    // sentinels is driven through the WHOLE path — apply, record, re-import,
    // refuse — and neither sentinel may appear in anything emitted.
    //
    // The settings tree is deliberately NOT in the search space for the PSK:
    // `wifi.client.networks[].psk` stores it by design and apid's redactor is
    // what keeps it off the wire. What is asserted here is everything this
    // module itself produces, plus the whole `provisioning` subtree, which is
    // what the status route serves.
    #[test]
    fn no_secret_from_the_document_reaches_a_report_or_the_record() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let root = stage(dir.path(), Source::Boot, &full_document());
        let mut settings = Settings::default();

        let applied = import(&store, &mut settings, &root).expect("import");
        let mut emitted = vec![format!("{applied:?}")];

        // The subtree the status route reads and `GetSettings("provisioning")`
        // serves, in the form it is served in.
        emitted
            .push(serde_json::to_string(&settings.provisioning).expect("the subtree serializes"));
        emitted.push(toml::to_string(&settings.provisioning).expect("the subtree renders"));

        // The re-import, and the refusal a claimed device gives a DIFFERENT
        // document carrying the same secrets.
        let mut reloaded = store.load().expect("reload");
        emitted.push(format!(
            "{:?}",
            import(&store, &mut reloaded, &root).expect("re-import")
        ));
        let altered = full_document().replace("priority = 10", "priority = 11");
        let root = stage(dir.path(), Source::Media, &altered);
        // The boot document is still staged, so remove it: this is about what
        // the media path reports.
        fs::remove_file(root.join(Source::Boot.dir_name()).join(DOCUMENT_FILE_NAME))
            .expect("remove the boot document");
        let mut reloaded = store.load().expect("reload");
        let refused = import(&store, &mut reloaded, &root).expect("refused import");
        assert!(
            matches!(refused, Outcome::Rejected { .. }),
            "a claimed device must refuse a new document, got {refused:?}"
        );
        emitted.push(format!("{refused:?}"));
        emitted.push(serde_json::to_string(&reloaded.provisioning).expect("serializes"));

        // Every rejection this module can raise about a secret-bearing key.
        for body in [
            format!(
                "version = 1\n\n[admin]\npassword = \"{}\"\n",
                &SECRET_PASSWORD[..4]
            ),
            format!(
                "version = 1\n\n[[wifi.networks]]\nssid = \"s\"\npsk = \"{}\"\n",
                unusable_psk()
            ),
            format!("version = 1\n\n[admin]\npassword = {SECRET_PASSWORD:?}\nbogus = 1\n"),
        ] {
            let dir = TempDir::new().expect("tempdir");
            let store = store_in(dir.path());
            let root = stage(dir.path(), Source::Media, &body);
            let mut fresh = Settings::default();
            let outcome = import(&store, &mut fresh, &root).expect("import");
            emitted.push(format!("{outcome:?}"));
            if let Outcome::Rejected { rejection, .. } = &outcome {
                emitted.push(rejection.to_string());
            }
            emitted.push(serde_json::to_string(&fresh.provisioning).expect("serializes"));
        }

        for text in &emitted {
            for sentinel in [SECRET_PASSWORD, SECRET_PSK, &SECRET_PASSWORD[..4]] {
                assert!(
                    !text.contains(sentinel),
                    "a document secret reached an emitted string: {sentinel:?} in {text:?}"
                );
            }
        }
        // The search space is populated: a scan over empty strings would pass
        // forever.
        assert!(emitted.len() >= 10, "{emitted:?}");
        assert!(
            emitted.iter().any(|text| text.contains("rejected")),
            "no refusal was emitted, so the refusal half proves nothing: {emitted:?}"
        );
    }

    // The stored hash is not the password, which is the other half of the
    // claim above: the plaintext is dropped, not moved.
    #[test]
    fn the_bootstrap_password_is_stored_only_as_a_hash() {
        let dir = TempDir::new().expect("tempdir");
        let (_, settings, _) = import_body(dir.path(), Source::Boot, &full_document());
        let rendered = toml::to_string(&settings).expect("the tree renders");
        assert!(
            !rendered.contains(SECRET_PASSWORD),
            "the plaintext password reached the settings file"
        );
        assert!(rendered.contains("$argon2id$"));
    }

    // The preference order: the BOOT medium wins, and the other source is not
    // consulted at all.
    #[test]
    fn the_boot_medium_wins_over_removable_media() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let root = stage(
            dir.path(),
            Source::Boot,
            "version = 1\n\n[time]\ntimezone = \"Europe/Berlin\"\n",
        );
        stage(
            dir.path(),
            Source::Media,
            "version = 1\n\n[time]\ntimezone = \"Asia/Shanghai\"\n",
        );
        let mut settings = Settings::default();

        let outcome = import(&store, &mut settings, &root).expect("import");
        let Outcome::Applied { source, .. } = outcome else {
            panic!("expected an applied document, got {outcome:?}");
        };
        assert_eq!(source, Source::Boot);
        assert_eq!(settings.time.timezone, "Europe/Berlin");
    }

    // With no boot document the media one is taken, which is the whole of
    // path 2.
    #[test]
    fn a_removable_medium_is_taken_when_the_boot_medium_carries_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let (_, settings, outcome) = import_body(dir.path(), Source::Media, time_only_document());
        let Outcome::Applied { source, .. } = outcome else {
            panic!("expected an applied document, got {outcome:?}");
        };
        assert_eq!(source, Source::Media);
        assert_eq!(settings.time.timezone, "Europe/Berlin");
    }

    // No medium, or a medium with no document: nothing is read, nothing is
    // written, and the record of the document this device DID apply survives.
    #[test]
    fn no_document_writes_nothing_and_forgets_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let root = stage(dir.path(), Source::Boot, time_only_document());
        let mut settings = Settings::default();
        import(&store, &mut settings, &root).expect("import");
        let applied = settings.clone();
        let bytes = fs::read(settings_path(dir.path())).expect("read settings file");

        // The medium is gone on the next boot.
        fs::remove_file(root.join("boot").join(DOCUMENT_FILE_NAME)).expect("remove");
        let mut reloaded = store.load().expect("reload");
        assert_eq!(
            import(&store, &mut reloaded, &root).expect("import"),
            Outcome::NoDocument
        );
        assert_eq!(reloaded, applied);
        assert_eq!(
            fs::read(settings_path(dir.path())).expect("re-read"),
            bytes,
            "a boot with no document must write nothing"
        );

        // And a staging root that was never created at all.
        let mut reloaded = store.load().expect("reload");
        assert_eq!(
            import(&store, &mut reloaded, &dir.path().join("absent")).expect("import"),
            Outcome::NoDocument
        );
        assert_eq!(reloaded, applied);
    }

    // Only a regular file at the one documented path is read. A symbolic link
    // planted on operator media would otherwise name a path on the DEVICE, and
    // a character device would hang early boot on a read that never ends.
    #[test]
    fn only_a_regular_file_at_the_documented_path_is_read() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("staging");
        let boot = root.join("boot");
        fs::create_dir_all(&boot).expect("create staging dir");

        // A document at a name this code does not look for is not found.
        fs::write(boot.join("provisioning.toml"), time_only_document()).expect("write");
        fs::write(root.join(DOCUMENT_FILE_NAME), time_only_document()).expect("write");
        assert_eq!(find_document(&root), None);

        // A directory under the document's name is not a document.
        fs::create_dir(boot.join(DOCUMENT_FILE_NAME)).expect("create dir");
        assert_eq!(find_document(&root), None);
        fs::remove_dir(boot.join(DOCUMENT_FILE_NAME)).expect("remove dir");

        // A symbolic link, even one pointing at a perfectly good document.
        let target = dir.path().join("elsewhere.toml");
        fs::write(&target, time_only_document()).expect("write");
        std::os::unix::fs::symlink(&target, boot.join(DOCUMENT_FILE_NAME)).expect("symlink");
        assert_eq!(find_document(&root), None);
        fs::remove_file(boot.join(DOCUMENT_FILE_NAME)).expect("remove link");

        // The positive control: the same bytes, as a regular file, ARE found.
        fs::write(boot.join(DOCUMENT_FILE_NAME), time_only_document()).expect("write");
        assert_eq!(
            find_document(&root),
            Some((Source::Boot, boot.join(DOCUMENT_FILE_NAME)))
        );
    }

    // A document too large to be one is refused before its bytes are read, so
    // a stick carrying a huge file is a legible refusal and not an
    // out-of-memory kill during early boot.
    #[test]
    fn an_oversized_document_is_refused_without_being_read() {
        let dir = TempDir::new().expect("tempdir");
        let body = "#".repeat(usize::try_from(MAX_DOCUMENT_BYTES).expect("fits") + 1);
        let (_, settings, outcome) = import_body(dir.path(), Source::Media, &body);
        let Outcome::Rejected { rejection, .. } = outcome else {
            panic!("expected a rejection, got {outcome:?}");
        };
        assert_eq!(rejection.key, "");
        assert!(rejection.reason.contains("maximum"), "{rejection:?}");
        assert!(settings.access.web_admin.is_none());
    }

    // A file that is not TOML is a refusal, and the parser's own message —
    // which quotes source text — is not what is reported.
    #[test]
    fn a_file_that_is_not_toml_is_refused_without_quoting_it() {
        let dir = TempDir::new().expect("tempdir");
        let body = format!("this is not TOML {SECRET_PASSWORD}");
        let (_, _, outcome) = import_body(dir.path(), Source::Boot, &body);
        let Outcome::Rejected { rejection, .. } = outcome else {
            panic!("expected a rejection, got {outcome:?}");
        };
        assert_eq!(rejection.key, "");
        assert_eq!(rejection.reason, "the document is not valid TOML");
        assert!(!rejection.to_string().contains(SECRET_PASSWORD));
    }

    // The claim gate, stated on its own: an unsigned medium cannot reconfigure
    // a device that already has an administrator.
    #[test]
    fn a_claimed_device_refuses_a_document_it_has_not_already_applied() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let mut settings = Settings::default();
        settings
            .set(
                "access.webAdmin",
                serde_json::json!({ "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$ZGV2$ZGV2" }),
            )
            .expect("claim the device");
        store.save(&settings).expect("save");
        let claimed = settings.clone();

        let root = stage(dir.path(), Source::Media, time_only_document());
        let outcome = import(&store, &mut settings, &root).expect("import");
        let Outcome::Rejected { rejection, .. } = outcome else {
            panic!("expected a rejection, got {outcome:?}");
        };
        assert!(
            rejection.reason.contains("already claimed"),
            "{rejection:?}"
        );

        // Nothing the document named moved, and the credential is intact.
        assert_eq!(settings.time, claimed.time);
        assert_eq!(settings.access.web_admin, claimed.access.web_admin);
    }

    // A device claimed BY the offered document reports `unchanged`, not
    // `already-claimed`: the short-circuit runs before the gate, or every
    // reboot with the medium still in place would look like an attack.
    #[test]
    fn the_document_that_claimed_the_device_still_reports_unchanged() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let root = stage(dir.path(), Source::Boot, &full_document());
        let mut settings = Settings::default();
        assert!(matches!(
            import(&store, &mut settings, &root).expect("first"),
            Outcome::Applied { .. }
        ));
        assert!(settings.access.web_admin.is_some(), "the device is claimed");

        let mut reloaded = store.load().expect("reload");
        assert!(matches!(
            import(&store, &mut reloaded, &root).expect("second"),
            Outcome::Unchanged { .. }
        ));
    }

    // A failing save leaves STATE and the caller's tree exactly as they were,
    // which is what makes a power loss mid-apply harmless: the commit is one
    // rename and there is nothing before it that reaches STATE.
    #[test]
    fn a_failed_save_leaves_state_and_the_caller_untouched() {
        let dir = TempDir::new().expect("tempdir");
        // The settings file's parent is a regular file, so `create_dir_all`
        // inside `Store::save` fails. Root-safe: a type error on the path, not
        // a permission check.
        let blocker = dir.path().join("blocked");
        fs::write(&blocker, b"not a directory").expect("write blocker");
        let config = dir.path().join("config");
        fs::create_dir_all(&config).expect("create the config namespace");
        let store = Store::new(blocker.join("settings.toml"), config);
        let root = stage(dir.path(), Source::Boot, time_only_document());
        let mut settings = Settings::default();

        let err = import(&store, &mut settings, &root).expect_err("the save must fail");
        assert!(
            format!("{err:#}").contains("persist the applied provisioning document"),
            "unexpected error: {err:#}"
        );
        assert_eq!(settings, Settings::default());
        assert_eq!(
            fs::read(&blocker).expect("re-read blocker"),
            b"not a directory"
        );
        assert!(!blocker.join("settings.toml").exists());
    }

    // The interlock with Layer 1: an identity the factory injected is the
    // identity the device keeps, and the seeded hostname is derived from it.
    // This is why the import runs BEFORE `ensure_provisioned`.
    #[test]
    fn an_injected_identity_is_the_one_first_boot_seeds_from() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let profile = dir.path().join("profile.conf");
        fs::write(&profile, "MOS_PROFILE=prod\n").expect("write profile");
        let root = stage(
            dir.path(),
            Source::Boot,
            "version = 1\n\n[identity]\ndeviceId = \"fedcba9876543210fedcba9876543210\"\n",
        );
        let mut settings = Settings::default();

        import(&store, &mut settings, &root).expect("import");
        crate::provisioning::ensure_provisioned(&store, dir.path(), &profile, &mut settings)
            .expect("seed");

        assert_eq!(
            settings.provisioning.device_id.as_deref(),
            Some("fedcba9876543210fedcba9876543210"),
            "seeding must keep the injected identity, not mint over it"
        );
        assert_eq!(settings.hostname, "mos-fedcba98");
        assert_eq!(settings.provisioning.state, ProvisioningState::Complete);
        assert_eq!(settings.provisioning.seeded_generation, 1);
        // The document record survives seeding's own save.
        assert!(settings.provisioning.document.is_some());
    }

    // A refused document leaves an appliance a person can still set up: no
    // credential, no settings change, and `POST /api/v1/setup` still available
    // because `access.webAdmin` is absent.
    #[test]
    fn a_refused_document_leaves_a_working_unclaimed_appliance() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let profile = dir.path().join("profile.conf");
        fs::write(&profile, "MOS_PROFILE=prod\n").expect("write profile");
        let root = stage(dir.path(), Source::Media, "version = 9\n");
        let mut settings = Settings::default();

        assert!(matches!(
            import(&store, &mut settings, &root).expect("import"),
            Outcome::Rejected { .. }
        ));
        // Seeding still runs and still works, which is what "does not block
        // boot" means for the daemon that follows.
        crate::provisioning::ensure_provisioned(&store, dir.path(), &profile, &mut settings)
            .expect("seed");
        assert_eq!(settings.provisioning.state, ProvisioningState::Complete);
        assert!(
            settings.access.web_admin.is_none(),
            "the device must still be claimable"
        );
        assert!(settings.access.device.password_hash.is_some());
    }

    // The record is written once, not once per boot: a device that keeps
    // meeting the same refusal does not rewrite STATE every time.
    #[test]
    fn a_repeated_refusal_writes_the_record_once() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let root = stage(dir.path(), Source::Boot, "version = 2\n");
        let mut settings = Settings::default();

        import(&store, &mut settings, &root).expect("first");
        let bytes = fs::read(settings_path(dir.path())).expect("read settings file");

        let mut reloaded = store.load().expect("reload");
        import(&store, &mut reloaded, &root).expect("second");
        assert_eq!(
            fs::read(settings_path(dir.path())).expect("re-read"),
            bytes,
            "the same refusal twice must not rewrite the settings file"
        );
    }

    // The document version is the DOCUMENT's, and it is pinned here so a bump
    // is a decision rather than a side effect.
    //
    // This used to assert it was DIFFERENT from the settings schema version,
    // so a reader could not mistake one for the other. That assertion is gone
    // with its subject: PLAN-070 §5.2.3 replaced the one tree-wide version
    // with one per document, each starting at v1, so there is no single number
    // left to differ from -- and every one of them now IS 1. The confusion the
    // old assertion guarded against has to be prevented by the names instead.
    #[test]
    fn the_document_version_is_pinned() {
        assert_eq!(DOCUMENT_VERSION, 1);
    }

    // The two source names are the wire values the status route serves and the
    // directory names the transport unit writes into. Pinned so a rename shows
    // up here rather than as a unit that stages into a directory nothing reads.
    #[test]
    fn the_source_names_are_the_transport_contract() {
        assert_eq!(SOURCES.len(), 2);
        assert_eq!(Source::Boot.as_str(), "boot");
        assert_eq!(Source::Boot.dir_name(), "boot");
        assert_eq!(Source::Media.as_str(), "media");
        assert_eq!(Source::Media.dir_name(), "media");
        assert_eq!(DOCUMENT_FILE_NAME, "mos-provisioning.toml");
        assert_eq!(DEFAULT_STAGING_ROOT, "/run/mos/provisioning");
    }

    // The staging root is the default unless the test hook names another.
    #[test]
    fn the_staging_root_defaults_to_the_documented_location() {
        // SAFETY-adjacent: this test reads and does not write the variable, so
        // it cannot race another test's environment.
        if std::env::var_os("MOSD_PROVISIONING_ROOT").is_none() {
            assert_eq!(staging_root_from_env(), PathBuf::from(DEFAULT_STAGING_ROOT));
        }
    }
}

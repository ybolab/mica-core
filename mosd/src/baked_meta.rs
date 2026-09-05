//! The baked update configuration — layer 1 of PLAN-070 §5.1.
//!
//! `meta/updates/manifest.json` is a build-host document that the image bakes
//! to `/usr/share/mos/meta/updates/manifest.json`, **inside the read-only
//! dm-verity root**: the configuration and the trust anchors are covered by
//! the same signature as the code that reads them. Nothing on the device
//! writes it, so this module reads it once at startup and never again — an
//! edit would need a new image and a reboot into the other slot.
//!
//! What it carries and what that means (PLAN-070 §5.1's table):
//!
//! - `trust.signingKeys` and `trust.signingKeyIds` — the package anchors.
//!   Layer 1 **owns** them: there is no operator key for either, and naming
//!   one in `/mos/config/updates.json` is a load error
//!   ([`crate::update_policy`] enforces it by name).
//! - `update.source`, `update.channel`, `update.policy` and
//!   `update.checkIntervalMinutes` — **defaults**, which the operator
//!   document overrides per key (PLAN-070 §5.3 moved the source URL into
//!   this list; the anchors above did not move).
//! - `http.credentialHosts` — the same-origin credential rule's second half,
//!   and like the source it is read from *here* and never from the override
//!   ([`BakedManifest::credentials_allowed_for`]).
//! - `product` and `fleet` — labels and the fleet switch's baked default;
//!   PLAN-072 owns what the latter turns on.
//!
//! **An unknown key is a build error, not a runtime one.** `rootfs/build.sh`
//! compares the key names in `meta/updates/manifest.json` against
//! `meta.example/updates/manifest.json` and refuses the build on either
//! difference, so a mistyped key never reaches an image. `deny_unknown_fields`
//! here is the second reader of that rule rather than the first, and no field
//! carries a serde default: the document states every value it configures
//! (PLAN-070 §2, "no implicit defaults").
//!
//! **The reader always answers with a document.** A missing or unreadable
//! manifest yields [`BakedManifest::code_defaults`] plus the reason, never an
//! absent layer: every caller has something to resolve against, and the error
//! is what tells it the answer is not this device's.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where the build bakes the manifest (PLAN-070 §3). `/usr/share/mos/`
/// rather than `/etc/`, because nothing on the device may edit it and `/etc`
/// is where an operator reasonably expects an edit to take.
pub const DEFAULT_MANIFEST_PATH: &str = "/usr/share/mos/meta/updates/manifest.json";

/// The document's first key, and the one value of it this reader accepts.
pub const SCHEMA_TAG: &str = "mos/meta/v1";

/// What the device does on its own (PLAN-071 §1). Baked as a default and
/// overridable per PLAN-070 §5.1; the semantics are PLAN-071's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateMode {
    /// The device initiates nothing. Manual check, fetch and install stay
    /// available behind their own gates, and so does offline import.
    Off,
    /// Metadata checks on the interval; never fetches, never installs.
    Check,
    /// Checks, then fetches, then installs inside a maintenance window.
    /// The fetch and install halves are PLAN-071's slice and are not built:
    /// today this behaves as [`UpdateMode::Check`].
    Auto,
}

/// The baked document. Every field is required — see the module note on
/// implicit defaults.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BakedManifest {
    pub schema: String,
    pub product: Product,
    pub update: BakedUpdate,
    pub trust: BakedTrust,
    pub http: BakedHttp,
    pub fleet: BakedFleet,
}

/// Which product an image is. A label: nothing reads it to make a decision
/// (PLAN-070 §2.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Product {
    pub vendor: String,
    pub model: String,
}

/// The four update defaults. Each is a **default**, not a floor and not a
/// fallback: it is consulted when the operator document is silent about the
/// key, and at no other moment (PLAN-070 §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BakedUpdate {
    /// Base URL of the published repository. `null` is a value, not an
    /// omission: no default server (PLAN-070 §7).
    pub source: Option<String>,
    pub channel: String,
    pub policy: UpdateMode,
    pub check_interval_minutes: u64,
}

/// The package anchors. Owned by this layer: no operator document may name
/// either, which is the premise the overridable source URL rests on
/// (PLAN-070 §5.3.2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BakedTrust {
    /// The trusted ed25519 public keys, inline (PLAN-070 §2.1).
    pub signing_keys: Vec<String>,
    /// Their sha256 ids, derived by the build so a ceremony record can be
    /// checked without decoding base64 by hand.
    pub signing_key_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BakedHttp {
    /// Hosts that may receive the configured update credentials in addition
    /// to the baked source's own origin. Empty by default, and adding to it
    /// is a build-time act (PLAN-070 §5.3.2).
    pub credential_hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BakedFleet {
    pub enabled: bool,
    pub url: Option<String>,
}

impl BakedUpdate {
    /// The defaults a device follows when no manifest could be read, and the
    /// layer a [`crate::update_policy::PolicyStore`] with no baked document
    /// resolves against.
    ///
    /// No source, so nothing can be checked or fetched against a server this
    /// device was never told about; the channel and cadence are the values
    /// the code has always shipped.
    pub fn code_defaults() -> Self {
        Self {
            source: None,
            channel: "stable".to_string(),
            policy: UpdateMode::Check,
            check_interval_minutes: 1440,
        }
    }
}

impl BakedManifest {
    /// The document the reader answers with when there is none to read.
    ///
    /// Empty of everything a device could act on: no source, no trusted key,
    /// no credential host, fleet off. It is a shape, not this device's
    /// configuration, and [`LoadedMeta::error`] is what says so.
    pub fn code_defaults() -> Self {
        Self {
            schema: SCHEMA_TAG.to_string(),
            product: Product {
                vendor: String::new(),
                model: String::new(),
            },
            update: BakedUpdate::code_defaults(),
            trust: BakedTrust {
                signing_keys: Vec::new(),
                signing_key_ids: Vec::new(),
            },
            http: BakedHttp {
                credential_hosts: Vec::new(),
            },
            fleet: BakedFleet {
                enabled: false,
                url: None,
            },
        }
    }

    /// Whether a request to `url` may carry the update credentials.
    ///
    /// **Pinned to the baked values on both halves** — the origin of
    /// `update.source` *as baked* and the baked `http.credentialHosts` — and
    /// it takes the target URL as its only argument, so there is no shape of
    /// this call that could re-anchor on an operator override. That is the
    /// one capability PLAN-070 §5.3.2 says this amendment would otherwise
    /// have created: read naively against an overridable source, "same-origin
    /// with `update.source`" hands the update credentials to whatever host
    /// the operator document names. An override moves the request; it never
    /// moves the credential, and a non-same-origin source is fetched
    /// anonymously.
    ///
    /// Unused today, and deliberately written before its first caller: mos
    /// authenticates to no update source yet, and the rule this encodes has
    /// to exist before the first credential does because the failure it
    /// prevents is silent (PLAN-070 §2.1). The `allow` is scoped to this item
    /// and removing it is the first credential's job.
    #[allow(dead_code)]
    pub fn credentials_allowed_for(&self, url: &str) -> bool {
        let Some(target) = Origin::parse(url) else {
            return false;
        };
        if self
            .http
            .credential_hosts
            .iter()
            .any(|host| host.eq_ignore_ascii_case(&target.host))
        {
            return true;
        }
        self.update
            .source
            .as_deref()
            .and_then(Origin::parse)
            .is_some_and(|baked| baked == target)
    }
}

/// Scheme, host and port — the three values same-origin compares.
///
/// Hand-rolled rather than pulled from a URL crate: the comparison needs
/// exactly these three fields of an `http`/`https` URL, and a dependency
/// added for one predicate would be linked into every device image. It
/// refuses anything it does not fully understand (no scheme, an unknown
/// scheme, userinfo, an empty host), because the caller is a credential
/// decision and "did not parse" must read as "not same-origin".
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

impl Origin {
    fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return None,
        };
        // Everything from the first `/`, `?` or `#` on is path, query or
        // fragment and no part of the origin.
        let authority = rest
            .split(['/', '?', '#'])
            .next()
            .filter(|authority| !authority.is_empty())?;
        // A credential decision does not guess at userinfo: a URL carrying
        // `user@host` is refused rather than read as `host`.
        if authority.contains('@') {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            // An IPv6 literal's colons are inside the brackets; a `]` after
            // the last colon means there was no port.
            Some((_, after)) if after.contains(']') => (authority, default_port),
            Some((host, port)) => (host, port.parse().ok()?),
            None => (authority, default_port),
        };
        (!host.is_empty()).then(|| Self {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
        })
    }
}

/// One load of the baked manifest: the document, and the reason it is not
/// this device's when it is not. Both, never neither.
pub struct LoadedMeta {
    pub manifest: BakedManifest,
    /// `Some` when the file is missing, unreadable, or does not parse or
    /// validate. Unreachable on a device by construction — the build refuses
    /// a manifest this reader would reject and the file is inside the verity
    /// root — so it is a reported condition rather than a handled one.
    pub error: Option<String>,
    path: PathBuf,
}

impl LoadedMeta {
    /// The live-state entry: the whole document, the file it came from, and
    /// the error when there is one.
    ///
    /// The whole document is safe to publish for a structural reason rather
    /// than a promise (PLAN-070 §8): the baked set is allowlisted at staging
    /// and checked again against the image, so there is nothing secret in it
    /// to disclose. That argument covers this layer only — `/mos/config/` is
    /// credential material and is read through the redactor.
    pub fn to_json(&self) -> Value {
        json!({
            "file": self.path.display().to_string(),
            "error": self.error,
            "document": self.manifest,
        })
    }
}

/// Read the baked manifest at `path`.
///
/// Always answers with a document: a missing file, an unreadable one, one
/// that does not parse and one that fails validation all yield
/// [`BakedManifest::code_defaults`] plus the reason.
pub fn load(path: &Path) -> LoadedMeta {
    let fallback = |error: String| LoadedMeta {
        manifest: BakedManifest::code_defaults(),
        error: Some(error),
        path: path.to_path_buf(),
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => return fallback(format!("read {}: {err}", path.display())),
    };
    let manifest = match serde_json::from_str::<BakedManifest>(&raw) {
        Ok(manifest) => manifest,
        Err(err) => return fallback(format!("parse {}: {err}", path.display())),
    };
    if let Err(reason) = validate(&manifest) {
        return fallback(format!("{}: {reason}", path.display()));
    }
    LoadedMeta {
        manifest,
        error: None,
        path: path.to_path_buf(),
    }
}

/// Validate what serde cannot: the schema tag this reader was written for,
/// and a channel there is something to select with.
fn validate(manifest: &BakedManifest) -> Result<(), String> {
    if manifest.schema != SCHEMA_TAG {
        return Err(format!(
            "schema is `{}`, and this reader knows `{SCHEMA_TAG}`",
            manifest.schema
        ));
    }
    if manifest.update.channel.trim().is_empty() {
        return Err("update.channel is empty".to_string());
    }
    Ok(())
}

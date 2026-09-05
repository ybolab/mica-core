//! PLAN-071 §6's version suppression: the versions this device will not
//! install automatically again, and the evidence for each.
//!
//! **Why this exists.** Without it `auto` is a reboot loop. A bundle that
//! installs and fails to confirm is answered by the bootloader spending its
//! credits and falling back, and the device then boots the OLDER system —
//! at which point the failed version is once again *strictly newer than the
//! running one*, so the next check selects it, the window opens, and it is
//! installed again. Forever, once per window. Suppression is what breaks
//! that: a version whose slot rolled back is written down, and the
//! automatic path refuses to select it until an operator says otherwise.
//!
//! **Why the record lives on STATE.** PLAN-071 §9.1 settles the same
//! question for the downgrade floor and the answer carries here unchanged:
//! `/mos/config/` holds what an integrator SETS and STATE holds what the
//! device MINTS or OBSERVES about itself, and "this version failed on this
//! device" is squarely the second. The sharper half of the reason is §9.1's
//! second bullet — since PLAN-070 §5.3 the operator document is also where
//! the source URL lives, so the single write that re-points a device at a
//! hostile server would be the same write that could clear the suppression,
//! if the suppression lived there. *A refusal a remote party can lift is not
//! a refusal.* This is a file on STATE, it is not a key in the policy
//! document, and clearing it stays an explicit audited operator action.
//!
//! **Why the evidence and not just the version.** PLAN-071 §6: *"this device
//! refuses 1.5.0"* with no reason is a support case with nothing in it. Each
//! record carries the slot the version was installed into, when the rollback
//! was observed, and the slot's boot status at that moment.
//!
//! **What suppression does NOT bind.** A manual install of a suppressed
//! version is permitted, on §6's and §9.2's shared reasoning: the operator
//! has been told and is choosing, and that authority is already granted by
//! `docs/design/security-model.md` §1. Only the automatic path consults this
//! store, which is why the consultation lives in [`crate::update_auto`] and
//! not in the install route every caller meets.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The store's file name under the STATE update directory.
pub const DEFAULT_FILE_NAME: &str = "suppressed-versions.json";

/// Mode of the store: world-readable, owner-writable. Nothing here is a
/// secret — it is a refusal an operator needs to be able to read — and the
/// STATE directory it sits in is already root-owned.
const FILE_MODE: u32 = 0o644;

/// One suppressed version, with the evidence for why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Suppression {
    /// The bundle version the failed slot carried, as RAUC reported it.
    pub version: String,
    /// The slot it was installed into, e.g. `rootfs.1`.
    pub slot: String,
    /// When the rollback was observed, RFC 3339 UTC.
    pub at: String,
    /// The slot's `boot-status` at that moment — `bad` in the case this
    /// store exists for. Absent when RAUC did not report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_status: Option<String>,
    /// The sentence an operator reads: what happened, in words.
    pub detail: String,
}

impl Suppression {
    /// The record as it is served beside a deferral and answered to the
    /// clearing route.
    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "slot": self.slot,
            "at": self.at,
            "bootStatus": self.boot_status,
            "detail": self.detail,
        })
    }
}

/// The document on disk. A named object rather than a bare array so a later
/// key can be added without every reader having to guess the shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Document {
    #[serde(default)]
    suppressed: Vec<Suppression>,
}

/// One load of the store: the records, or the reason they could not be read.
///
/// Both, never neither, and for [`crate::update_policy::LoadedPolicy`]'s
/// reason inverted: a store that exists and does not parse must NOT read as
/// "nothing is suppressed", because that is precisely the reading that
/// restarts the loop this store exists to break. The automatic path defers
/// on `error`; every manual route is unaffected, because none of them
/// consults this store at all.
pub struct LoadedSuppressions {
    pub records: Vec<Suppression>,
    pub error: Option<String>,
}

impl LoadedSuppressions {
    /// The record for `version`, if it is suppressed.
    pub fn get(&self, version: &str) -> Option<&Suppression> {
        self.records.iter().find(|record| record.version == version)
    }
}

/// The suppressed-version store, read fresh per decision.
///
/// A handful of bytes per decision is cheaper than a watch, and it is the
/// same trade [`crate::update_policy::PolicyStore`] makes: an operator's
/// clearing takes effect on the next tick with no restart and no reload
/// verb.
#[derive(Clone)]
pub struct SuppressionStore {
    /// `None` = no file at all (dry-run daemons): nothing is ever suppressed
    /// and nothing is ever written.
    path: Option<PathBuf>,
}

impl SuppressionStore {
    /// Store backed by `path`; a missing file is an empty store.
    pub fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Store with no file at all — the dry-run/test shape.
    pub fn none() -> Self {
        Self { path: None }
    }

    /// Load the store. A missing file is empty; an unreadable or
    /// unparseable one is empty PLUS the error that makes the automatic
    /// path defer.
    pub fn load(&self) -> LoadedSuppressions {
        let Some(path) = &self.path else {
            return LoadedSuppressions {
                records: Vec::new(),
                error: None,
            };
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return LoadedSuppressions {
                    records: Vec::new(),
                    error: None,
                };
            }
            Err(err) => {
                return LoadedSuppressions {
                    records: Vec::new(),
                    error: Some(format!("read {}: {err}", path.display())),
                };
            }
        };
        match serde_json::from_str::<Document>(&raw) {
            Ok(document) => LoadedSuppressions {
                records: document.suppressed,
                error: None,
            },
            Err(err) => LoadedSuppressions {
                records: Vec::new(),
                error: Some(format!("parse {}: {err}", path.display())),
            },
        }
    }

    /// Suppress `entry`'s version, answering whether it was newly recorded.
    ///
    /// Idempotent, and idempotent in the direction that keeps the FIRST
    /// evidence: the caller is the state refresh, which runs on every
    /// `GetUpdateState` while a rolled-back slot is visible, so a version
    /// already recorded is left exactly as it was. Overwriting would walk
    /// the timestamp forward on every poll and lose the moment the failure
    /// actually happened.
    ///
    /// # Errors
    ///
    /// The store could not be read or the write failed. A store that does
    /// not parse refuses the write rather than replacing it: the automatic
    /// path is already deferring on that error, and truncating an
    /// unreadable file is how the record it holds would be lost.
    pub fn record(&self, entry: &Suppression) -> Result<bool, String> {
        let Some(path) = &self.path else {
            return Ok(false);
        };
        let loaded = self.load();
        if let Some(error) = loaded.error {
            return Err(error);
        }
        if loaded.get(&entry.version).is_some() {
            return Ok(false);
        }
        let mut records = loaded.records;
        records.push(entry.clone());
        self.save(path, records)?;
        Ok(true)
    }

    /// Clear the suppression on `version`, answering the record removed.
    ///
    /// `Ok(None)` when the version was not suppressed — a typo must not read
    /// as a successful clearing, and the caller answers the operator with
    /// the difference.
    ///
    /// # Errors
    ///
    /// The store could not be read or the write failed.
    pub fn clear(&self, version: &str) -> Result<Option<Suppression>, String> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        let loaded = self.load();
        if let Some(error) = loaded.error {
            return Err(error);
        }
        let mut records = loaded.records;
        let Some(index) = records.iter().position(|record| record.version == version) else {
            return Ok(None);
        };
        let removed = records.remove(index);
        self.save(path, records)?;
        Ok(Some(removed))
    }

    /// Write `records` back, atomically where the filesystem allows it.
    fn save(&self, path: &Path, records: Vec<Suppression>) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("create {}: {err}", parent.display()))?;
        }
        let document = Document {
            suppressed: records,
        };
        let body = serde_json::to_string_pretty(&document)
            .map_err(|err| format!("encode the suppression store: {err}"))?;
        crate::fswrite::write_config(path, &format!("{body}\n"), FILE_MODE)
            .map_err(|err| format!("write {}: {err:#}", path.display()))
    }
}

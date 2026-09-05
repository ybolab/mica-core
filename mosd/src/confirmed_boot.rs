//! PLAN-071 §7's confirmed-boot fact: which install mosd has itself observed
//! running, and in what ORDER it observed them.
//!
//! **Why this exists.** The rollback guard's central rule is that a rollback
//! goes backward — the target must be the older of the two installs — and
//! until this record existed the only way to order them was
//! `installed.timestamp`, the wall clock of the machine at the moment RAUC
//! wrote the slot. `docs/design/updates.md` §5.2 records the consequence: a
//! device that installed with a wrong clock can record an order that did not
//! happen, and the guard then either refuses a legitimate rollback or permits
//! one it cannot justify. A manual install has a witness — the human who
//! pressed the button knows when — and PLAN-071 §7's automatic install has
//! none, which is what turns the footnote into a dependency.
//!
//! **What the fact is grounded in, and why that is better.** Not a clock, and
//! not anything the bootloader was told. One entry is written when mosd
//! observes *itself running from a slot*: the system in that slot booted far
//! enough to start the daemon that serves this API. The ordering key is
//! [`ConfirmedBoot::sequence`], a counter this store mints — each newly
//! observed install takes one higher than any record already held — so the
//! order of two installs is decided by mosd's own succession of observations
//! and reads no clock at all. A wrong clock cannot move it, a clock that
//! jumps backward cannot invert it, and network time arriving later cannot
//! rewrite it.
//!
//! **Why first-boot order IS install order.** An install is written into a
//! slot the device is not running from, and it is booted after it is written.
//! So a slot whose install mosd saw running earlier necessarily carries the
//! earlier install: at the moment the later one was written, the earlier one
//! was already the running system. The record therefore answers the same
//! question `installed.timestamp` answered, from an observation instead of
//! from a timestamp — and it answers a second one the timestamp only ever
//! implied, that the target really did boot (`docs/design/recovery.md` §3
//! node 2's precondition), because the entry exists only because mosd ran
//! there.
//!
//! **What it deliberately does NOT do.** It does not confirm a slot. The boot
//! health gate (`rootfs/overlay/usr/lib/mos/mos-health`) owns the
//! PENDING_CONFIRM -> CONFIRMED edge and `rauc status mark-good` with it, for
//! the reasons [`crate::rauc`]'s module docs give; nothing here marks
//! anything, nothing here is read by the bootloader, and an unconfirmed
//! booted slot still leaves the rollback to the bootloader exactly as before.
//! This record changes what mosd KNOWS about ordering, not who acts on it.
//! It is also strictly weaker than the health gate's verdict — mosd running
//! is a necessary condition of that gate passing, not a sufficient one — and
//! that is the same strength the derivation it replaces had, which claimed
//! only that the device was running the target when the other install
//! happened.
//!
//! **Why the record lives on STATE.** PLAN-071 §9.1 settles it for the
//! downgrade floor and [`crate::update_suppress`] carries it unchanged:
//! `/mos/config/` holds what an integrator SETS and STATE holds what the
//! device MINTS or OBSERVES about itself. "mosd ran here" is squarely the
//! second, it is not an operator key, and an operator document able to
//! rewrite the order of this device's own boots would be an order nobody
//! observed.

use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::rauc::{SlotStatus, booted_slot};

/// The store's file name under the STATE update directory.
pub const DEFAULT_FILE_NAME: &str = "confirmed-boots.json";

/// Mode of the store: world-readable, owner-writable, as
/// [`crate::update_suppress`] writes its own for the same reason — nothing
/// here is a secret and an operator diagnosing a refused rollback needs to be
/// able to read it.
const FILE_MODE: u32 = 0o644;

/// One install mosd has observed running, and where that observation falls in
/// mosd's own succession of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmedBoot {
    /// The slot mosd was running from, e.g. `rootfs.0`.
    pub slot: String,
    /// The bundle version that slot carried, as RAUC reported it. Absent for
    /// a slot whose status file names none — a factory-written slot RAUC has
    /// never installed into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_version: Option<String>,
    /// RAUC's `installed.timestamp` for that slot. Recorded as part of the
    /// install's IDENTITY — it is what tells a later boot whether the slot
    /// still holds the install this entry is about — and never as an
    /// ordering key. Nothing in this module compares two of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_timestamp: Option<String>,
    /// The ordering key: one higher than any sequence held when this install
    /// was first observed running. Minted here, so it is independent of every
    /// clock on the device.
    pub sequence: u64,
    /// When the observation was made, RFC 3339 UTC — evidence for a human
    /// reading the file, never an ordering key. It is a reading of the same
    /// clock this record exists to stop depending on, so it is recorded and
    /// not compared.
    pub first_seen_at: String,
}

impl ConfirmedBoot {
    /// Is this entry about the install `slot` holds NOW?
    ///
    /// Identity rather than name: a slot that has been re-installed carries a
    /// different system, and an entry about the system it used to carry must
    /// not be read as an observation of the one it carries now.
    fn is_about(&self, slot: &SlotStatus) -> bool {
        self.slot == slot.name
            && self.bundle_version == slot.bundle_version
            && self.installed_timestamp == slot.installed_timestamp
    }
}

/// One load of the store: every observation it holds, plus the reason it
/// could not be read.
///
/// [`Default`] is the empty record — no observations, no error — which is
/// what a device that has never run this code has, and what dry-run and the
/// unit tests hand the guard.
#[derive(Debug, Clone, Default)]
pub struct ConfirmedBoots {
    records: Vec<ConfirmedBoot>,
    /// Why the store could not be read, for the caller's log line. A store
    /// that does not parse is NOT emptied and NOT overwritten: the guard
    /// falls back to the install-time clock, which is what it used before
    /// this record existed, and the file stays intact for the next boot to
    /// read.
    pub error: Option<String>,
}

impl ConfirmedBoots {
    /// Every observation held, oldest first. The tests' window onto the
    /// record; the guard reads it through [`Self::older_install`] alone.
    #[cfg(test)]
    pub fn records(&self) -> &[ConfirmedBoot] {
        &self.records
    }

    /// The observation about the install `slot` holds now, if there is one.
    fn get(&self, slot: &SlotStatus) -> Option<&ConfirmedBoot> {
        self.records.iter().find(|record| record.is_about(slot))
    }

    /// Is `target`'s install the STRICTLY OLDER of the two, by mosd's own
    /// observations? `None` when this record cannot order them.
    ///
    /// `None` — never a guess — whenever either install is one mosd has not
    /// observed running: a slot written but never booted, a device whose
    /// record predates this code, or a store that did not parse. The caller
    /// falls back to the install-time clock there, which is what it did
    /// before this record existed. Two equal sequences also answer `None`;
    /// this store never mints a duplicate, so equal means the file was
    /// written by something else, and an ordering derived from it would be
    /// derived from nothing.
    pub fn older_install(&self, target: &SlotStatus, booted: &SlotStatus) -> Option<bool> {
        let (target, booted) = (self.get(target)?, self.get(booted)?);
        (target.sequence != booted.sequence).then_some(target.sequence < booted.sequence)
    }

    /// A record built in memory, for the tests that drive the ordering with
    /// no file behind it.
    #[cfg(test)]
    pub fn from_records(records: Vec<ConfirmedBoot>) -> Self {
        Self {
            records,
            error: None,
        }
    }
}

/// The confirmed-boot store on STATE, read fresh per state refresh.
///
/// Read fresh for [`crate::update_suppress::SuppressionStore`]'s reason: a
/// handful of bytes per refresh is cheaper than a watch, and the write
/// happens only when the observation is new — once per install, not once per
/// poll.
#[derive(Clone)]
pub struct ConfirmedBootStore {
    /// `None` = no file at all (dry-run daemons): nothing is ever observed
    /// and nothing is ever written.
    path: Option<PathBuf>,
}

impl ConfirmedBootStore {
    /// Store backed by `path`; a missing file is an empty record.
    pub fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Store with no file at all — the dry-run/test shape.
    pub fn none() -> Self {
        Self { path: None }
    }

    /// Load the record. A missing file is empty; an unreadable or
    /// unparseable one is empty PLUS the error that says so.
    pub fn load(&self) -> ConfirmedBoots {
        let Some(path) = &self.path else {
            return ConfirmedBoots::default();
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return ConfirmedBoots::default();
            }
            Err(err) => {
                return ConfirmedBoots {
                    records: Vec::new(),
                    error: Some(format!("read {}: {err}", path.display())),
                };
            }
        };
        match serde_json::from_str::<Document>(&raw) {
            Ok(document) => ConfirmedBoots {
                records: document.boots,
                error: None,
            },
            Err(err) => ConfirmedBoots {
                records: Vec::new(),
                error: Some(format!("parse {}: {err}", path.display())),
            },
        }
    }

    /// Observe the running system and answer the record as it then stands.
    ///
    /// The observation is "mosd is running from this slot", so the only slot
    /// it can ever be about is the booted one. Idempotent in the direction
    /// that keeps the FIRST sighting: an install already recorded is left
    /// exactly as it was, because the sequence it was given is the whole
    /// ordering fact and re-stamping it on every poll would walk it forward
    /// past installs that really are newer.
    ///
    /// A slot whose install differs from the recorded one REPLACES that
    /// slot's entry: a slot holds one system, and an entry about the one it
    /// used to hold would order a system that is no longer there.
    ///
    /// Never fatal. A write that fails is logged and the guard falls back to
    /// the install-time clock; a daemon that refused to serve the update
    /// state because it could not write a note about its own boot would be
    /// the worse failure.
    pub fn observe(&self, slots: &[SlotStatus]) -> ConfirmedBoots {
        let loaded = self.load();
        let (Some(path), Some(booted)) = (&self.path, booted_slot(slots)) else {
            return loaded;
        };
        if let Some(error) = &loaded.error {
            // Not overwritten: the file may hold observations this daemon can
            // no longer read but a fixed parser still could, and truncating
            // it would destroy the only record of boots nothing else witnessed.
            tracing::warn!(
                error = %error,
                "confirmed-boot record unreadable; not recording this boot"
            );
            return loaded;
        }
        if loaded.get(booted).is_some() {
            return loaded;
        }
        let sequence = loaded
            .records
            .iter()
            .map(|record| record.sequence)
            .max()
            .unwrap_or(0)
            + 1;
        let record = ConfirmedBoot {
            slot: booted.name.clone(),
            bundle_version: booted.bundle_version.clone(),
            installed_timestamp: booted.installed_timestamp.clone(),
            sequence,
            first_seen_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        };
        let mut records = loaded.records;
        records.retain(|held| held.slot != record.slot);
        records.push(record.clone());
        if let Err(error) = save(path, &records) {
            tracing::warn!(error = %error, "confirmed-boot record not written");
            return ConfirmedBoots {
                records,
                error: Some(error),
            };
        }
        tracing::info!(
            slot = %record.slot,
            sequence = record.sequence,
            version = record.bundle_version.as_deref().unwrap_or("(none)"),
            "recorded mosd's own confirmed boot of this install"
        );
        ConfirmedBoots {
            records,
            error: None,
        }
    }
}

/// The document on disk. A named object rather than a bare array, so a later
/// key can be added without every reader having to guess the shape — the
/// suppression store's reasoning, and its file sits beside this one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Document {
    #[serde(default)]
    boots: Vec<ConfirmedBoot>,
}

/// Write `records` back, atomically where the filesystem allows it.
fn save(path: &Path, records: &[ConfirmedBoot]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    let document = Document {
        boots: records.to_vec(),
    };
    let body = serde_json::to_string_pretty(&document)
        .map_err(|err| format!("encode the confirmed-boot record: {err}"))?;
    crate::fswrite::write_config(path, &format!("{body}\n"), FILE_MODE)
        .map_err(|err| format!("write {}: {err:#}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot as RAUC reports it: the two fields that identify the install it
    /// holds, plus the state that says whether we are running from it.
    fn slot(name: &str, state: &str, version: &str, stamp: &str) -> SlotStatus {
        SlotStatus {
            name: name.to_string(),
            state: Some(state.to_string()),
            bundle_version: Some(version.to_string()),
            installed_timestamp: Some(stamp.to_string()),
            ..SlotStatus::default()
        }
    }

    fn store(dir: &tempfile::TempDir) -> ConfirmedBootStore {
        ConfirmedBootStore::at(dir.path().join("update").join(DEFAULT_FILE_NAME))
    }

    #[test]
    fn the_first_observed_boot_takes_the_first_sequence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        let slots = [
            slot("rootfs.0", "booted", "2026.08", "2026-08-30T10:00:00Z"),
            slot("rootfs.1", "inactive", "2026.07", "2026-08-01T10:00:00Z"),
        ];
        let boots = store.observe(&slots);
        assert_eq!(boots.records().len(), 1, "only the booted slot is observed");
        let record = &boots.records()[0];
        assert_eq!(record.slot, "rootfs.0");
        assert_eq!(record.sequence, 1);
        assert_eq!(record.bundle_version.as_deref(), Some("2026.08"));
        // And it survives the process: the next load reads the same fact.
        assert_eq!(store.load().records(), boots.records());
    }

    #[test]
    fn the_same_install_observed_again_keeps_its_first_sequence() {
        // The state refresh runs on every poll. If a repeat sighting
        // re-stamped the record, the sequence would walk forward past
        // installs that really are newer and the ordering would invert.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        let slots = [slot(
            "rootfs.0",
            "booted",
            "2026.08",
            "2026-08-30T10:00:00Z",
        )];
        let first = store.observe(&slots);
        let again = store.observe(&slots);
        assert_eq!(first.records(), again.records());
        assert_eq!(again.records().len(), 1);
        assert_eq!(again.records()[0].sequence, 1);
    }

    #[test]
    fn a_new_install_in_a_slot_replaces_that_slot_s_entry_with_a_higher_sequence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        // Boot A, then boot B (the update), then boot A again after a
        // rollback: three observations, two of which are about A.
        store.observe(&[slot(
            "rootfs.0",
            "booted",
            "2026.07",
            "2026-08-01T10:00:00Z",
        )]);
        store.observe(&[slot(
            "rootfs.1",
            "booted",
            "2026.08",
            "2026-08-30T10:00:00Z",
        )]);
        let boots = store.observe(&[slot(
            "rootfs.0",
            "booted",
            "2026.09",
            "2026-09-05T10:00:00Z",
        )]);
        assert_eq!(boots.records().len(), 2, "one entry per slot, not per boot");
        let sequences: Vec<(&str, u64)> = boots
            .records()
            .iter()
            .map(|record| (record.slot.as_str(), record.sequence))
            .collect();
        assert_eq!(sequences, vec![("rootfs.1", 2), ("rootfs.0", 3)]);
        assert_eq!(
            boots.records()[1].bundle_version.as_deref(),
            Some("2026.09"),
            "the entry is about the install the slot holds now"
        );
    }

    #[test]
    fn the_order_is_the_sequence_and_not_the_install_clock() {
        // The gap this record closes, as an assertion. The install-time
        // clock says rootfs.1 was written a month BEFORE the running system;
        // mosd's own observations say it was booted after. The record orders
        // by what it saw.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        let older_by_the_clock = slot("rootfs.1", "inactive", "2026.08", "2026-07-01T10:00:00Z");
        store.observe(&[slot(
            "rootfs.0",
            "booted",
            "2026.07",
            "2026-08-01T10:00:00Z",
        )]);
        store.observe(&[SlotStatus {
            state: Some("booted".to_string()),
            ..older_by_the_clock.clone()
        }]);
        let booted = slot("rootfs.0", "booted", "2026.07", "2026-08-01T10:00:00Z");
        let boots = store.load();
        assert_eq!(
            boots.older_install(&older_by_the_clock, &booted),
            Some(false),
            "the alternate booted LATER, so it is not the older install"
        );
        assert_eq!(
            boots.older_install(&booted, &older_by_the_clock),
            Some(true),
            "and the relation is the other way round when the roles swap"
        );
    }

    #[test]
    fn an_install_nobody_observed_running_cannot_be_ordered() {
        // A slot written and never booted, and a slot whose install has
        // changed since it was recorded: neither is an observation of the
        // system there NOW, so the record declines to order rather than
        // guessing. The caller falls back to the install-time clock.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        let booted = slot("rootfs.0", "booted", "2026.07", "2026-08-01T10:00:00Z");
        let boots = store.observe(std::slice::from_ref(&booted));
        let never_booted = slot("rootfs.1", "inactive", "2026.08", "2026-08-30T10:00:00Z");
        assert_eq!(boots.older_install(&never_booted, &booted), None);
        let reinstalled = SlotStatus {
            bundle_version: Some("2026.09".to_string()),
            ..booted.clone()
        };
        assert_eq!(boots.older_install(&never_booted, &reinstalled), None);
    }

    #[test]
    fn an_unreadable_record_is_reported_and_left_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        let path = dir.path().join("update").join(DEFAULT_FILE_NAME);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "{ this is not json").expect("write");
        let boots = store.observe(&[slot(
            "rootfs.0",
            "booted",
            "2026.08",
            "2026-08-30T10:00:00Z",
        )]);
        assert!(boots.records().is_empty(), "nothing is claimed to be known");
        assert!(boots.error.is_some(), "and the reason is carried");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{ this is not json",
            "a record this daemon cannot parse is never truncated"
        );
    }

    #[test]
    fn a_store_with_no_file_observes_nothing() {
        // Dry-run: the daemon runs on a build host with no STATE partition.
        let store = ConfirmedBootStore::none();
        let boots = store.observe(&[slot(
            "rootfs.0",
            "booted",
            "2026.08",
            "2026-08-30T10:00:00Z",
        )]);
        assert!(boots.records().is_empty());
        assert!(boots.error.is_none());
    }
}

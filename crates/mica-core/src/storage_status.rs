//! Read-mostly observation of the fixed storage layout: tiers, the PLAN-063
//! bind namespaces, directory usage, project quotas, media health and pressure.
//!
//! The shape [`crate::network_state`] and [`crate::time_status`] take: a trait
//! with an unavailable default so a dry-run daemon or a test never inspects
//! its host, a production implementation only `main.rs` attaches, and pure
//! rendering functions the tests drive with literal evidence.
//!
//! **The layout is not this module's to define.** `boards/*/board.env` fixes
//! the partition table, `rootfs/overlay/etc/fstab.in` mounts DATA at
//! [`DATA_MOUNT`], and PLAN-063 binds `/mnt/data/mica` at `/mica` and
//! `/mnt/data/srv` at `/srv` through
//! `rootfs/overlay/etc/systemd/system/{mica,srv}.mount`, after
//! `rootfs/overlay/usr/lib/mica/mica-data-layout` has created the roots
//! fail-closed. Nothing here formats anything or moves a partition boundary,
//! and there is deliberately no method on this module, on the bus or in the
//! API that could.
//!
//! **One write exists, and it is the point of a readiness probe.** PLAN-061's
//! readiness contract is explicitly more than `access(W_OK)`, so
//! [`HostStorage::probe`] creates, fsyncs, removes and re-fsyncs a private
//! file under [`PROBE_SUBTREE`]. It is the only write in this module, it
//! happens only in the system-owned namespace, and it cleans up after itself
//! whether or not it succeeded.
//!
//! Two rules govern every field below, because a storage surface that guesses
//! is worse than one that says nothing:
//!
//! 1. **Absence is data.** A tier whose partition is not on this board, a
//!    medium whose wear counters the kernel does not export, a filesystem
//!    that was never checked — each is reported as absent or `unsupported`
//!    with the reason, never as a healthy zero.
//! 2. **No fabricated precision** (PLAN-049 risk #1). eMMC lifetime is a
//!    10%-granularity bucket in the JEDEC register, so it is reported as the
//!    bucket it is, with the raw register value beside it. There is no
//!    single "percent worn" number here because the device does not have one.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value as Json, json};

/// One fixed tier of the layout.
///
/// The table below MIRRORS `boards/*/board.env` — `<TIER>_LABEL` becomes the
/// GPT partition name (the assembler passes it to `sgdisk --change-name`), and
/// `<TIER>_ROLE` is copied verbatim. Never re-derive the layout from anything
/// else: board.env is the single source of truth, and this table exists only
/// because micad runs on a device where the env file is not present.
pub struct TierSpec {
    /// Stable name of the tier on the wire, lowercase.
    pub name: &'static str,
    /// GPT partition name, from board.env `<TIER>_LABEL`.
    pub partition_label: &'static str,
    /// board.env `<TIER>_ROLE`.
    pub role: &'static str,
    /// Fixed mountpoint, absent for the raw firmware partition.
    pub mount: Option<&'static str>,
}

/// Three physical partitions per board. UEFI boards expose ESP; FIT boards
/// expose FIRMWARE. Immutable deployments share SYSTEM; all writable data is DATA.
pub const TIERS: &[TierSpec] = &[
    TierSpec {
        name: "esp",
        partition_label: "esp",
        role: "esp",
        mount: Some("/boot"),
    },
    TierSpec {
        name: "firmware",
        partition_label: "firmware",
        role: "firmware",
        mount: None,
    },
    TierSpec {
        name: "system",
        partition_label: "system",
        role: "ext4",
        mount: Some("/mnt/system"),
    },
    TierSpec {
        name: "data",
        partition_label: "data",
        role: "ext4",
        mount: Some(DATA_MOUNT),
    },
];

/// Where the DATA partition itself mounts (PLAN-063).
///
/// `/mnt/data` is a system implementation detail: PLAN-063 exposes `/mica` and
/// `/srv` and asks product surfaces not to encourage direct writes below it.
/// It is named here because a tier report that hid the actual mountpoint
/// could not be checked against `/etc/fstab`.
pub const DATA_MOUNT: &str = "/mnt/data";

/// The bind namespaces carved out of the DATA filesystem, with
/// the source each is bound from.
///
/// Transcribed from `rootfs/overlay/etc/systemd/system/{mica,srv}.mount`, which
/// are the contract; `rootfs/overlay/usr/lib/mica/mica-data-layout` creates the
/// sources. They are NOT tiers: both live on the one DATA filesystem and
/// share its single capacity pool, which is why they carry no capacity of
/// their own below and why `status_json` says so in as many words.
pub const BINDS: &[BindSpec] = &[
    BindSpec {
        name: "mica",
        mount: "/mica",
        source: "/mnt/data/mica",
        owner: "system",
    },
    BindSpec {
        name: "srv",
        mount: "/srv",
        source: "/mnt/data/srv",
        owner: "user",
    },
    BindSpec {
        name: "containers",
        mount: "/mica/containers",
        source: "/mnt/data/containers",
        owner: "system",
    },
];

/// One PLAN-063 bind namespace.
pub struct BindSpec {
    /// Stable name on the wire.
    pub name: &'static str,
    /// Where the bind is mounted.
    pub mount: &'static str,
    /// The path under [`DATA_MOUNT`] it is bound from.
    pub source: &'static str,
    /// `system` for `/mica`, `user` for `/srv`.
    pub owner: &'static str,
}

/// The subtree the readiness probe writes into.
///
/// This directory is created by mica-data-layout inside the acquisition
/// workspace; the probe never writes to an unrelated DATA namespace.
pub const PROBE_SUBTREE: &str = "/mica/updates/staging";

/// The tier observed by the low-space policy.
///
/// One tier, one filesystem, one capacity pool -- and two namespaces on top of
/// it. Reporting `/mica` and `/srv` as if each had its own capacity would give
/// a reader two numbers that sum to twice the disk.
pub const DATA_TIER: &str = "data";
/// Used-space percentage at or above which a watched tier is `warning`.
pub const WARNING_ENTER_PERCENT: u8 = 80;
/// Used-space percentage a `warning` tier must fall BELOW to clear.
pub const WARNING_CLEAR_PERCENT: u8 = 75;
/// Used-space percentage at or above which a watched tier is `critical`.
pub const CRITICAL_ENTER_PERCENT: u8 = 90;
/// Used-space percentage a `critical` tier must fall BELOW to drop back to
/// `warning`.
pub const CRITICAL_CLEAR_PERCENT: u8 = 85;

/// One tier's space accounting, in bytes.
///
/// `free` is what an unprivileged writer can still use; `reserved` is the
/// filesystem's own reserved-blocks pool, which root can write into and an
/// application cannot. They are separate numbers because conflating them is
/// how a tier reports free space that no application can actually have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FsSpace {
    /// Filesystem size.
    pub total: u64,
    /// Space in use.
    pub used: u64,
    /// Space available to an unprivileged writer.
    pub free: u64,
    /// The filesystem's reserved pool: `total - used - free`.
    pub reserved: u64,
}

impl FsSpace {
    /// Used share of the filesystem, 0–100, rounded down.
    ///
    /// Over `total` rather than `used + free`, so the reserved pool counts as
    /// space the tier does not have left: a DATA filesystem at 100% for an
    /// application is not "95% full" because root could still write.
    #[must_use]
    pub fn used_percent(&self) -> u8 {
        if self.total == 0 {
            return 0;
        }
        u8::try_from(self.used.saturating_mul(100) / self.total).unwrap_or(100)
    }
}

/// One mount, as `/proc/self/mountinfo` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEvidence {
    /// The block device backing the mount, canonical. For a device-mapper
    /// mount this is the BACKING partition (`/sys/block/dm-N/slaves`), not
    /// `/dev/dm-N`: the booted rootfs slot is a partition under a verity
    /// device, and reporting the mapper device would leave the tier that
    /// actually holds it looking unmounted.
    pub device: String,
    /// The filesystem-relative source path from mountinfo, including bind roots.
    pub root: String,
    /// Where it is mounted.
    pub mount: String,
    /// Filesystem type.
    pub fstype: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

/// What the system RECORDS about the last filesystem check of one device.
///
/// systemd runs `systemd-fsck@<escaped device>.service` for every fstab entry
/// with a non-zero pass number, and keeps that unit's result. This is that
/// unit, verbatim — there is no check history here beyond the last boot,
/// because the system does not keep one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckEvidence {
    /// The unit name, so an operator can go read its journal.
    pub unit: String,
    /// systemd's `ActiveState` for the unit.
    pub active_state: Option<String>,
    /// systemd's `Result`: `success`, `exit-code`, `timeout`, …
    pub result: Option<String>,
    /// The checker's exit status. fsck's own encoding: 0 clean, 1 errors
    /// were CORRECTED, 4 errors were left uncorrected.
    pub exit_status: Option<i32>,
}

/// Everything observed about one tier.
#[derive(Debug, Clone, Default)]
pub struct TierEvidence {
    /// The tier's canonical block device, when a partition with its label
    /// exists on this board.
    pub device: Option<String>,
    /// Partition size in bytes, from sysfs. Present even when the tier is
    /// not mounted, which is the only capacity number an inactive A/B slot
    /// has.
    pub partition_bytes: Option<u64>,
    /// The mount the tier's device is on, if any.
    pub mount: Option<MountEvidence>,
    /// Space accounting for a mounted tier.
    pub space: Option<FsSpace>,
    /// The last check systemd recorded for the tier's device.
    pub check: Option<CheckEvidence>,
}

/// Normalized media wear, or the reason there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaHealth {
    /// eMMC JEDEC wear registers, exported by the mmc driver under
    /// `/sys/block/<dev>/device/`.
    Emmc {
        /// Raw `life_time`, e.g. `0x01 0x02`.
        life_time_raw: String,
        /// Raw `pre_eol_info`, e.g. `0x01`.
        pre_eol_raw: Option<String>,
    },
    /// The metric exists for this class of medium but this image cannot read
    /// it. The string is the reason, and it is reported, never hidden.
    Unsupported(String),
}

/// One physical medium.
#[derive(Debug, Clone)]
pub struct MediumEvidence {
    /// Kernel name, e.g. `mmcblk0`.
    pub name: String,
    /// `mmc`, `nvme`, `scsi` or `other`, from the kernel name.
    pub kind: String,
    /// Capacity in bytes, from `/sys/block/<dev>/size` (512-byte sectors).
    pub size_bytes: Option<u64>,
    /// Vendor identity: the mmc product name, or the SCSI/NVMe model.
    pub model: Option<String>,
    /// Whether the kernel calls the medium rotational. `false` on every
    /// medium mica runs on today; recorded rather than assumed.
    pub rotational: Option<bool>,
    /// Wear/EOL, or why there is none.
    pub health: MediaHealth,
}

/// What the readiness probe did, or why it did not run.
///
/// PLAN-061's readiness contract is explicitly more than `access(W_OK)`:
/// create and fsync a private probe file in the owning subtree, remove it,
/// fsync again. `NotAttempted` carries its reason because a probe that did not
/// run must never read as "writable" -- that is this module's absence rule at
/// the one place where getting it wrong would tell an operator their update
/// storage is fine when nobody has checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// A file was created, fsynced, removed and the directory fsynced.
    Passed,
    /// The write was attempted and failed; the string is the OS error.
    Failed(String),
    /// No write was attempted; the string says why.
    NotAttempted(String),
}

/// Everything observed about one PLAN-063 bind namespace.
#[derive(Debug, Clone, Default)]
pub struct BindEvidence {
    /// The mount at the bind's mountpoint, if it is mounted at all.
    pub mount: Option<MountEvidence>,
    /// Whether the bind's source path exists under the DATA mount and is a
    /// real directory rather than a symlink. `mica-data-layout` refuses a
    /// symlink at these paths, so a symlink here is a substituted namespace.
    pub source_is_directory: Option<bool>,
    /// The readiness probe, on `/mica` only.
    pub probe: Option<ProbeOutcome>,
}

/// One bind namespace's readiness, in the PLAN-061 vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Mounted, on DATA, writable.
    Ready,
    /// Mounted and on DATA, but not fully usable: read-only, or the DATA
    /// filesystem is at its critical threshold.
    Degraded,
    /// Not mounted, or mounted from something that is not the DATA
    /// partition. Either way a writer must NOT fall back to another
    /// filesystem.
    Unavailable,
    /// The observer could not establish which of the above holds.
    Unknown,
}

impl Readiness {
    /// The wire spelling the API and UI consume.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }
}

fn mount_matches_namespace(mount: &MountEvidence) -> bool {
    BINDS.iter().any(|spec| {
        spec.mount == mount.mount
            && spec.source.strip_prefix(DATA_MOUNT) == Some(mount.root.as_str())
    })
}

/// Classify one bind namespace against the DATA tier it must live on.
///
/// Pure, and the whole of the readiness decision. Order is meaning:
///
/// 1. **Not mounted, or the source is not a real directory on DATA, is
///    `Unavailable`.** PLAN-061 refuses symlink substitution by name, and
///    PLAN-063's initializer dies on a symlink at these paths, so a bind
///    whose source is not a directory below the DATA mount has had its
///    namespace substituted -- reporting that as merely degraded would invite
///    exactly the fallback the contract forbids.
/// 2. **A read-only mount, or a full DATA filesystem, is `Degraded`.** The
///    namespace is the right one; it just cannot be written now.
/// 3. **A failed probe is `Degraded`**, and a probe that was never attempted
///    leaves the verdict at whatever the evidence above supports -- it never
///    upgrades one.
#[must_use]
pub fn classify_readiness(
    bind: &BindEvidence,
    data: Option<&TierEvidence>,
    pressure: Pressure,
) -> Readiness {
    let Some(mount) = &bind.mount else {
        // A missing mount is unavailable even without a DATA tier to compare
        // against: nothing is mounted there, so nothing may be written there.
        return Readiness::Unavailable;
    };
    if bind.source_is_directory == Some(false) {
        return Readiness::Unavailable;
    }
    let Some(data) = data else {
        return Readiness::Unknown;
    };
    let Some(device) = &data.device else {
        return Readiness::Unknown;
    };
    // The mount source must resolve to the DATA partition. A bind carrying
    // any other device is a different filesystem wearing the right path.
    if &mount.device != device || !mount_matches_namespace(mount) {
        return Readiness::Unavailable;
    }
    if mount.read_only || data.mount.as_ref().is_some_and(|m| m.read_only) {
        return Readiness::Degraded;
    }
    if matches!(bind.probe, Some(ProbeOutcome::Failed(_))) {
        return Readiness::Degraded;
    }
    if pressure == Pressure::Critical {
        return Readiness::Degraded;
    }
    if bind.source_is_directory.is_none() {
        return Readiness::Unknown;
    }
    Readiness::Ready
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectUsage {
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub used_inodes: u64,
    pub limit_inodes: u64,
}

fn parse_project_quotas(csv: &str) -> Option<BTreeMap<u32, ProjectUsage>> {
    let mut lines = csv.lines();
    if lines.next()?
        != "Project,BlockStatus,FileStatus,BlockUsed,BlockSoftLimit,BlockHardLimit,BlockGrace,FileUsed,FileSoftLimit,FileHardLimit,FileGrace"
    {
        return None;
    }
    let mut projects = BTreeMap::new();
    for line in lines {
        let fields: Vec<_> = line.split(',').collect();
        if fields.len() != 11 {
            return None;
        }
        let id: u32 = fields[0].strip_prefix('#')?.parse().ok()?;
        if ![100, 101, 102].contains(&id) {
            continue;
        }
        if projects.contains_key(&id) {
            return None;
        }
        projects.insert(
            id,
            ProjectUsage {
                used_bytes: fields[3].parse::<u64>().ok()?.checked_mul(1024)?,
                limit_bytes: fields[5].parse::<u64>().ok()?.checked_mul(1024)?,
                used_inodes: fields[7].parse().ok()?,
                limit_inodes: fields[9].parse().ok()?,
            },
        );
    }
    (projects.len() == 3).then_some(projects)
}

const DATA_DIRECTORIES: [&str; 8] = [
    "state",
    "meta",
    "mica",
    "srv",
    "cache",
    "tmp",
    "var",
    "containers",
];

/// One observation of the whole storage surface.
#[derive(Debug, Clone, Default)]
pub struct StorageEvidence {
    pub directory_bytes: BTreeMap<String, u64>,
    pub project_quotas: Option<BTreeMap<u32, ProjectUsage>>,
    /// Per-tier evidence, keyed by [`TierSpec::name`]. A tier missing from
    /// the map is a tier whose partition label names nothing on this board.
    pub tiers: BTreeMap<String, TierEvidence>,
    /// Every whole-disk medium the kernel shows.
    pub media: Vec<MediumEvidence>,
    /// Per-bind evidence, keyed by [`BindSpec::name`].
    pub binds: BTreeMap<String, BindEvidence>,
}

impl StorageEvidence {
    /// The DATA tier's evidence, when this board has one.
    #[must_use]
    pub fn data_tier(&self) -> Option<&TierEvidence> {
        self.tiers.get(DATA_TIER)
    }
}

/// One tier's low-space classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pressure {
    /// Below every threshold, or back below a clear threshold.
    #[default]
    Normal,
    /// At or above [`WARNING_ENTER_PERCENT`].
    Warning,
    /// At or above [`CRITICAL_ENTER_PERCENT`].
    Critical,
}

impl Pressure {
    /// The wire spelling the API and UI consume.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// Apply the hysteresis band to one reading.
///
/// Pure, and the whole of the low-space decision. Rising uses the enter
/// thresholds and falling uses the clear thresholds, so a tier hovering at 80%
/// does not flap between two states on every poll — which is the entire point
/// of a hysteresis band and the one property a threshold pair alone cannot
/// give.
#[must_use]
pub fn next_pressure(previous: Pressure, used_percent: u8) -> Pressure {
    if used_percent >= CRITICAL_ENTER_PERCENT {
        return Pressure::Critical;
    }
    if used_percent >= WARNING_ENTER_PERCENT {
        // Falling out of critical needs the clear threshold, not merely
        // dropping below the enter threshold.
        if previous == Pressure::Critical && used_percent >= CRITICAL_CLEAR_PERCENT {
            return Pressure::Critical;
        }
        return Pressure::Warning;
    }
    if used_percent >= WARNING_CLEAR_PERCENT {
        // Between the two warning thresholds: hold whatever was already
        // reported, dropping only from critical to warning.
        return match previous {
            Pressure::Normal => Pressure::Normal,
            _ => Pressure::Warning,
        };
    }
    Pressure::Normal
}

/// Remembers each watched tier's last reported [`Pressure`] so
/// [`next_pressure`]'s hysteresis has a previous state to work from.
///
/// The state is per-daemon and in RAM on purpose: it exists to damp flapping
/// between polls, not to be a history, and a fresh daemon classifying from
/// `Normal` reaches the same steady state within one poll.
#[derive(Debug, Default)]
pub struct PressureTracker {
    states: std::sync::Mutex<BTreeMap<String, Pressure>>,
}

impl PressureTracker {
    /// The last classification recorded for `tier`, without taking a new
    /// reading. `Normal` when the tier has never been observed.
    pub fn current(&self, tier: &str) -> Pressure {
        self.states
            .lock()
            .expect("pressure state is never poisoned")
            .get(tier)
            .copied()
            .unwrap_or_default()
    }

    /// Record `used_percent` for `tier` and return the classification.
    pub fn observe(&self, tier: &str, used_percent: u8) -> Pressure {
        let mut states = self
            .states
            .lock()
            .expect("pressure state is never poisoned");
        let previous = states.get(tier).copied().unwrap_or_default();
        let next = next_pressure(previous, used_percent);
        states.insert(tier.to_string(), next);
        next
    }
}

/// The lifecycle decisions PLAN-049 requires to be EXPLICIT.
///
/// Every one is a product decision, and every current answer is
/// `unsupported` — "Unselected features remain unsupported". They are served
/// so a client learns the answer from the device instead of inferring it from
/// a missing route, and so that promoting one to `supported` is a visible
/// change to this list rather than a quiet new endpoint.
///
/// `docs/design/storage.md` carries the reason for each; changing a value
/// here without changing that document is the failure this pairing is meant
/// to prevent.
const LIFECYCLE: &[(&str, &str)] = &[
    ("backupRestore", "unsupported"),
    ("offlineRepair", "unsupported"),
    ("dataPreservingReplacement", "unsupported"),
    ("factoryReset", "unsupported"),
    ("secureErase", "unsupported"),
    ("encryption", "unsupported"),
    ("removableMedia", "unsupported"),
];

/// `evidence` rendered as the JSON the bus method serves.
///
/// A tier the board does not have is present in the array with
/// `"present": false` rather than omitted, and a metric the device does not
/// export is absent rather than zero.
#[must_use]
pub fn status_json(evidence: &StorageEvidence, pressure: &PressureTracker) -> Json {
    let tiers: Vec<Json> = TIERS
        .iter()
        .map(|spec| tier_json(spec, evidence.tiers.get(spec.name), pressure))
        .collect();
    let media: Vec<Json> = evidence.media.iter().map(medium_json).collect();
    let data = evidence.data_tier();
    // The binds are classified against the DATA tier's CURRENT pressure, and
    // reading it here rather than re-observing keeps one poll's verdicts
    // consistent: `tier_json` above already advanced the hysteresis state for
    // this observation, so asking the tracker again would be a second
    // reading of the same sample.
    let data_pressure = pressure.current(DATA_TIER);
    let binds: Vec<Json> = BINDS
        .iter()
        .map(|spec| bind_json(spec, evidence.binds.get(spec.name), data, data_pressure))
        .collect();
    json!({
        "tiers": tiers,
        // One filesystem, multiple namespaces. This member is not a second tier
        // list: every filesystem capacity number for the binds is the DATA tier's, and
        // saying so here is what stops a reader adding them together.
        "namespaces": {
            "sharedCapacityTier": DATA_TIER,
            "detail": "/mica, /srv and /mica/containers bind directories of one DATA filesystem; capacity is reported once on the data tier, with independent project accounting; system/user and container limits are zero (unlimited), while variable data is bounded",
            "binds": binds,
            "directories": DATA_DIRECTORIES.map(|name| json!({
                "name": name,
                "usedBytes": evidence.directory_bytes.get(name),
                "project": match name { "mica" | "srv" => Some(100), "cache" | "tmp" | "var" => Some(101), "containers" => Some(102), _ => None },
            })),
            "projectQuotas": evidence.project_quotas,
        },
        "media": media,
        "policy": {
            "warningPercent": WARNING_ENTER_PERCENT,
            "warningClearPercent": WARNING_CLEAR_PERCENT,
            "criticalPercent": CRITICAL_ENTER_PERCENT,
            "criticalClearPercent": CRITICAL_CLEAR_PERCENT,
            "watchedTiers": [DATA_TIER],
        },
        "lifecycle": LIFECYCLE
            .iter()
            .map(|(name, decision)| ((*name).to_string(), Json::from(*decision)))
            .collect::<serde_json::Map<String, Json>>(),
    })
}

fn tier_json(spec: &TierSpec, evidence: Option<&TierEvidence>, pressure: &PressureTracker) -> Json {
    let mut root = serde_json::Map::new();
    root.insert("name".to_string(), json!(spec.name));
    root.insert("role".to_string(), json!(spec.role));
    root.insert("partitionLabel".to_string(), json!(spec.partition_label));
    if let Some(mount) = spec.mount {
        root.insert("expectedMount".to_string(), json!(mount));
    }
    let Some(evidence) = evidence else {
        root.insert("present".to_string(), json!(false));
        root.insert(
            "detail".to_string(),
            json!(format!(
                "no partition named `{}` on this board",
                spec.partition_label
            )),
        );
        return Json::Object(root);
    };
    root.insert("present".to_string(), json!(true));
    if let Some(device) = &evidence.device {
        root.insert("device".to_string(), json!(device));
    }
    if let Some(bytes) = evidence.partition_bytes {
        root.insert("partitionBytes".to_string(), json!(bytes));
    }
    root.insert("mounted".to_string(), json!(evidence.mount.is_some()));
    if let Some(mount) = &evidence.mount {
        root.insert("mount".to_string(), json!(mount.mount));
        root.insert("filesystem".to_string(), json!(mount.fstype));
        root.insert("readOnly".to_string(), json!(mount.read_only));
    }
    if let Some(space) = evidence.space {
        root.insert(
            "space".to_string(),
            json!({
                "totalBytes": space.total,
                "usedBytes": space.used,
                "freeBytes": space.free,
                "reservedBytes": space.reserved,
                "usedPercent": space.used_percent(),
            }),
        );
        if spec.name == DATA_TIER {
            root.insert(
                "pressure".to_string(),
                json!(pressure.observe(spec.name, space.used_percent()).as_str()),
            );
        }
    }
    root.insert(
        "check".to_string(),
        match &evidence.check {
            Some(check) => json!({
                "unit": check.unit,
                "activeState": check.active_state,
                "result": check.result,
                "exitStatus": check.exit_status,
            }),
            // Not "clean": a tier with no fsck unit was never checked, and
            // there is no history anywhere that says otherwise.
            None => json!({ "recorded": false }),
        },
    );
    Json::Object(root)
}

/// One bind namespace rendered.
///
/// Deliberately carries NO capacity of its own: `/mica` and `/srv` are two
/// views of the DATA filesystem, and a `space` object here would be the same
/// bytes reported a second and third time.
fn bind_json(
    spec: &BindSpec,
    evidence: Option<&BindEvidence>,
    data: Option<&TierEvidence>,
    pressure: Pressure,
) -> Json {
    let mut root = serde_json::Map::new();
    root.insert("name".to_string(), json!(spec.name));
    root.insert("mount".to_string(), json!(spec.mount));
    root.insert("source".to_string(), json!(spec.source));
    root.insert("owner".to_string(), json!(spec.owner));
    let Some(evidence) = evidence else {
        root.insert("readiness".to_string(), json!(Readiness::Unknown.as_str()));
        root.insert(
            "detail".to_string(),
            json!("this daemon observed no mount table"),
        );
        return Json::Object(root);
    };
    let readiness = classify_readiness(evidence, data, pressure);
    root.insert("readiness".to_string(), json!(readiness.as_str()));
    root.insert("mounted".to_string(), json!(evidence.mount.is_some()));
    if let Some(mount) = &evidence.mount {
        root.insert("device".to_string(), json!(mount.device));
        root.insert("readOnly".to_string(), json!(mount.read_only));
        // The contract's first question, answered as its own member rather
        // than folded into the verdict: a writer that falls back to another
        // filesystem is the failure PLAN-061 names, so "is this actually
        // DATA?" has to be legible on its own.
        root.insert(
            "sourceOnData".to_string(),
            json!(data.and_then(|tier| tier.device.as_deref()) == Some(mount.device.as_str())),
        );
    }
    root.insert(
        "sourceMatchesNamespace".to_string(),
        json!(evidence.mount.as_ref().is_some_and(mount_matches_namespace)),
    );
    if let Some(is_directory) = evidence.source_is_directory {
        root.insert("sourceIsDirectory".to_string(), json!(is_directory));
    }
    if let Some(probe) = &evidence.probe {
        root.insert(
            "probe".to_string(),
            match probe {
                ProbeOutcome::Passed => json!({ "attempted": true, "passed": true }),
                ProbeOutcome::Failed(error) => {
                    json!({ "attempted": true, "passed": false, "error": error })
                }
                // Never `passed: true`. A probe that did not run is the one
                // thing this member must not be mistaken for.
                ProbeOutcome::NotAttempted(reason) => {
                    json!({ "attempted": false, "reason": reason })
                }
            },
        );
    }
    Json::Object(root)
}

fn medium_json(medium: &MediumEvidence) -> Json {
    let mut root = serde_json::Map::new();
    root.insert("name".to_string(), json!(medium.name));
    root.insert("kind".to_string(), json!(medium.kind));
    if let Some(bytes) = medium.size_bytes {
        root.insert("sizeBytes".to_string(), json!(bytes));
    }
    if let Some(model) = &medium.model {
        root.insert("model".to_string(), json!(model));
    }
    if let Some(rotational) = medium.rotational {
        root.insert("rotational".to_string(), json!(rotational));
    }
    root.insert("health".to_string(), health_json(&medium.health));
    Json::Object(root)
}

fn health_json(health: &MediaHealth) -> Json {
    match health {
        MediaHealth::Emmc {
            life_time_raw,
            pre_eol_raw,
        } => {
            let estimates: Vec<Json> = life_time_raw
                .split_whitespace()
                .map(|field| match parse_life_time(field) {
                    Some(bucket) => json!({
                        "raw": field,
                        "usedPercentMin": bucket.0,
                        "usedPercentMax": bucket.1,
                    }),
                    None => json!({ "raw": field, "detail": "the device does not define this estimate" }),
                })
                .collect();
            json!({
                "supported": true,
                "source": "sysfs mmc life_time / pre_eol_info",
                // The raw registers travel with the normalization so support
                // can read what the device actually said, not only what this
                // module made of it.
                "raw": { "lifeTime": life_time_raw, "preEolInfo": pre_eol_raw },
                "lifetimeEstimates": estimates,
                "preEol": pre_eol_raw.as_deref().map(parse_pre_eol),
            })
        }
        MediaHealth::Unsupported(reason) => json!({ "supported": false, "reason": reason }),
    }
}

/// The JEDEC `DEVICE_LIFE_TIME_EST_TYP_A/B` bucket a raw field names, as the
/// inclusive-exclusive percentage range it actually means.
///
/// `0x01` is 0–10% of rated life used, `0x02` is 10–20%, up to `0x0A`
/// (90–100%); `0x0B` means the rated lifetime is EXCEEDED. `0x00` is "not
/// defined" and is not a bucket at all. A single percentage is deliberately
/// not derived from this: the register carries one decimal digit of
/// resolution and pretending otherwise is PLAN-049's risk #1.
#[must_use]
pub fn parse_life_time(raw: &str) -> Option<(u8, u8)> {
    let value = u8::from_str_radix(raw.trim().trim_start_matches("0x"), 16).ok()?;
    match value {
        0 => None,
        0x0b => Some((100, 100)),
        1..=0x0a => Some(((value - 1) * 10, value * 10)),
        _ => None,
    }
}

/// The JEDEC `PRE_EOL_INFO` value a raw field names.
#[must_use]
pub fn parse_pre_eol(raw: &str) -> &'static str {
    match u8::from_str_radix(raw.trim().trim_start_matches("0x"), 16) {
        Ok(0x01) => "normal",
        Ok(0x02) => "warning",
        Ok(0x03) => "urgent",
        // Includes 0x00, which JEDEC defines as "not defined": the device
        // declines to answer, which is not the same as "normal".
        _ => "undefined",
    }
}

/// Parse `/proc/self/mountinfo`.
///
/// Field layout: `id parent major:minor root mountpoint options [tags...] -
/// fstype source superoptions`. The optional tag run before the `-` is why
/// the fields after it cannot be indexed from the left.
#[must_use]
pub fn parse_mountinfo(text: &str) -> Vec<MountEvidence> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let left: Vec<&str> = left.split_whitespace().collect();
            let right: Vec<&str> = right.split_whitespace().collect();
            Some(MountEvidence {
                device: (*right.get(1)?).to_string(),
                root: unescape_octal(left.get(3)?),
                mount: unescape_octal(left.get(4)?),
                fstype: (*right.first()?).to_string(),
                read_only: left.get(5)?.split(',').any(|option| option == "ro"),
            })
        })
        .collect()
}

/// Decode the octal escapes the kernel writes into mountinfo paths (space,
/// tab, newline and backslash).
fn unescape_octal(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) if digits.len() == 3 => {
                out.push(char::from(byte));
                chars.nth(2);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Decode a systemd unit-name escape back to the path it names.
///
/// systemd writes `/` as `-` and every other non-alphanumeric byte as
/// `\xNN`, so `systemd-fsck@dev-disk-by\x2dpartuuid-1234.service` is a check
/// of `/dev/disk/by-partuuid/1234`. The `\x2d` case is why this cannot be a
/// plain `replace('-', "/")`: a partuuid path is full of literal hyphens.
#[must_use]
pub fn unescape_unit_name(escaped: &str) -> String {
    let bytes = escaped.as_bytes();
    let mut out = String::with_capacity(escaped.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if index + 3 < bytes.len() && bytes[index + 1] == b'x' => {
                let digits = &escaped[index + 2..index + 4];
                match u8::from_str_radix(digits, 16) {
                    Ok(byte) => {
                        out.push(char::from(byte));
                        index += 4;
                    }
                    Err(_) => {
                        out.push('\\');
                        index += 1;
                    }
                }
            }
            b'-' => {
                out.push('/');
                index += 1;
            }
            byte => {
                out.push(char::from(byte));
                index += 1;
            }
        }
    }
    out
}

/// The device path a `systemd-fsck@….service` unit checks, or `None` when the
/// unit is not one of those.
#[must_use]
pub fn fsck_unit_device(unit: &str) -> Option<String> {
    let instance = unit
        .strip_prefix("systemd-fsck@")?
        .strip_suffix(".service")?;
    if instance.is_empty() {
        return None;
    }
    Some(format!("/{}", unescape_unit_name(instance)))
}

/// Read-only source of storage evidence.
#[async_trait::async_trait]
pub trait StorageStatusSource: Send + Sync {
    /// Observe the current storage evidence.
    ///
    /// # Errors
    ///
    /// Returns an error only when this daemon has no observer at all; a
    /// production observer answers with absent evidence instead.
    async fn observe(&self) -> Result<StorageEvidence>;
}

/// The observer a daemon without host access has: none.
pub struct UnavailableStorageStatus;

#[async_trait::async_trait]
impl StorageStatusSource for UnavailableStorageStatus {
    async fn observe(&self) -> Result<StorageEvidence> {
        Err(anyhow::anyhow!("this daemon observes no storage"))
    }
}

/// Pair each recorded fsck unit with the canonical device it checked.
///
/// `resolve` turns the path a unit name encodes — normally a
/// `/dev/disk/by-partuuid/…` symlink, because that is how `/etc/fstab` names
/// the tiers — into the canonical `/dev/…` node the tier evidence carries.
/// Pure over that closure, so the pairing is testable without a bus or a
/// `/dev`.
#[must_use]
pub fn checks_by_device<R>(units: &[CheckEvidence], resolve: R) -> BTreeMap<String, CheckEvidence>
where
    R: Fn(&str) -> Option<String>,
{
    units
        .iter()
        .filter_map(|check| {
            let named = fsck_unit_device(&check.unit)?;
            let device = resolve(&named).unwrap_or(named);
            Some((device, check.clone()))
        })
        .collect()
}

/// Parse one `df -P -B1 <mount>` answer into a [`FsSpace`].
///
/// `df` rather than `statvfs(3)`: the workspace forbids `unsafe`, so there is
/// no FFI call available, and coreutils is Essential on the image. The
/// reserved pool is `total - used - free`, which is the only place that
/// number is visible at all.
#[must_use]
pub fn parse_df(output: &str) -> Option<FsSpace> {
    let line = output.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    // Filesystem, blocks, used, available, capacity, mountpoint.
    if fields.len() < 6 {
        return None;
    }
    let total: u64 = fields[1].parse().ok()?;
    let used: u64 = fields[2].parse().ok()?;
    let free: u64 = fields[3].parse().ok()?;
    Some(FsSpace {
        total,
        used,
        free,
        reserved: total.saturating_sub(used).saturating_sub(free),
    })
}

/// Production observer over sysfs, `/proc/self/mountinfo`, `df` and systemd.
///
/// `root` is a path prefix rather than a hard-coded `/` so the assembly — the
/// tier-to-mount-to-medium pairing, which is where this module's bugs would
/// live — can be driven over a fixture tree in a unit test. Production uses
/// [`HostStorage::production`], which is the same thing rooted at `/` with the
/// `df` reader attached.
pub struct HostStorage {
    root: std::path::PathBuf,
    space: SpaceReader,
}

/// Answers one mount point's space. Injected so a fixture tree cannot make a
/// test shell out to `df` against the machine running it.
type SpaceReader = Box<dyn Fn(&str) -> Option<FsSpace> + Send + Sync>;

impl HostStorage {
    /// The observer `main.rs` attaches on a device.
    #[must_use]
    pub fn production() -> Self {
        Self::at("/").with_space_reader(df_space)
    }

    /// An observer rooted at `root`, with NO space reader: nothing here shells
    /// out until one is attached, so a fixture tree cannot make a test run
    /// `df` against the machine it is running on.
    #[must_use]
    pub fn at(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: root.into(),
            space: Box::new(|_| None),
        }
    }

    /// Attach the reader that answers a mount point's space.
    #[must_use]
    pub fn with_space_reader(
        mut self,
        reader: impl Fn(&str) -> Option<FsSpace> + Send + Sync + 'static,
    ) -> Self {
        self.space = Box::new(reader);
        self
    }

    fn path(&self, relative: &str) -> std::path::PathBuf {
        self.root.join(relative)
    }

    fn read_trimmed(&self, relative: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.path(relative)).ok()?;
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Resolve a `/dev/disk/by-*` symlink to its `/dev/<name>` target, under
    /// this observer's root.
    fn resolve_dev(&self, device: &str) -> Option<String> {
        let relative = device.strip_prefix('/')?;
        let target = std::fs::read_link(self.path(relative)).ok()?;
        let name = target.file_name()?.to_str()?;
        Some(format!("/dev/{name}"))
    }

    /// Every whole-disk medium, with its partitions' sizes.
    fn media(&self) -> (Vec<MediumEvidence>, BTreeMap<String, u64>) {
        let mut media = Vec::new();
        let mut partitions = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(self.path("sys/block")) else {
            return (media, partitions);
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| !is_virtual_block(name))
            .collect();
        names.sort();
        for name in names {
            for (part, size) in self.partitions_of(&name) {
                partitions.insert(part, size);
            }
            media.push(self.medium(&name));
        }
        (media, partitions)
    }

    fn partitions_of(&self, disk: &str) -> Vec<(String, u64)> {
        let Ok(entries) = std::fs::read_dir(self.path(&format!("sys/block/{disk}"))) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                let relative = format!("sys/block/{disk}/{name}");
                self.path(&format!("{relative}/partition"))
                    .exists()
                    .then(|| {
                        let sectors: u64 = self
                            .read_trimmed(&format!("{relative}/size"))
                            .and_then(|text| text.parse().ok())
                            .unwrap_or_default();
                        (format!("/dev/{name}"), sectors * SECTOR_BYTES)
                    })
            })
            .collect()
    }

    fn medium(&self, name: &str) -> MediumEvidence {
        let block = format!("sys/block/{name}");
        MediumEvidence {
            kind: medium_kind(name).to_string(),
            size_bytes: self
                .read_trimmed(&format!("{block}/size"))
                .and_then(|text| text.parse::<u64>().ok())
                .map(|sectors| sectors * SECTOR_BYTES),
            model: self
                .read_trimmed(&format!("{block}/device/model"))
                .or_else(|| self.read_trimmed(&format!("{block}/device/name"))),
            rotational: self
                .read_trimmed(&format!("{block}/queue/rotational"))
                .map(|text| text == "1"),
            health: match self.read_trimmed(&format!("{block}/device/life_time")) {
                Some(life_time_raw) => MediaHealth::Emmc {
                    life_time_raw,
                    pre_eol_raw: self.read_trimmed(&format!("{block}/device/pre_eol_info")),
                },
                None => MediaHealth::Unsupported(unsupported_reason(name).to_string()),
            },
            name: name.to_string(),
        }
    }

    fn mounts(&self) -> Vec<MountEvidence> {
        let Ok(text) = std::fs::read_to_string(self.path("proc/self/mountinfo")) else {
            return Vec::new();
        };
        parse_mountinfo(&text)
    }

    /// Observe one PLAN-063 bind namespace.
    fn bind(&self, spec: &BindSpec, mounts: &[MountEvidence]) -> BindEvidence {
        let mount = mounts
            .iter()
            .find(|mount| mount.mount == spec.mount)
            .cloned();
        // A symlink here is a substituted namespace, which `mica-data-layout`
        // refuses outright; `symlink_metadata` is what makes that visible,
        // because `metadata` would follow the link and call it a directory.
        let source_is_directory = spec
            .source
            .strip_prefix('/')
            .and_then(|relative| std::fs::symlink_metadata(self.path(relative)).ok())
            .map(|meta| meta.is_dir());
        BindEvidence {
            probe: self.probe(spec, mount.as_ref()),
            mount,
            source_is_directory,
        }
    }

    /// PLAN-061's readiness probe, on the system namespace only.
    ///
    /// `/srv` is user-owned: micad writing a probe file into it would put a
    /// daemon's private file in a namespace the product gives to the
    /// operator, so `/srv` gets no probe and says so rather than getting a
    /// silent pass.
    fn probe(&self, spec: &BindSpec, mount: Option<&MountEvidence>) -> Option<ProbeOutcome> {
        if spec.name != "mica" {
            return Some(ProbeOutcome::NotAttempted(format!(
                "{} is outside the system readiness probe namespace; no probe write is authorized",
                spec.mount
            )));
        }
        let Some(mount) = mount else {
            return Some(ProbeOutcome::NotAttempted(format!(
                "{} is not mounted",
                spec.mount
            )));
        };
        if !mount_matches_namespace(mount) {
            return Some(ProbeOutcome::NotAttempted(
                "unexpected namespace source".to_string(),
            ));
        }
        if mount.read_only {
            return Some(ProbeOutcome::NotAttempted(format!(
                "{} is mounted read-only",
                spec.mount
            )));
        }
        let subtree = self.path(PROBE_SUBTREE.trim_start_matches('/'));
        if !subtree.is_dir() {
            return Some(ProbeOutcome::NotAttempted(format!(
                "{PROBE_SUBTREE} does not exist; mica-data-layout has not run"
            )));
        }
        Some(match write_probe(&subtree) {
            Ok(()) => ProbeOutcome::Passed,
            Err(err) => ProbeOutcome::Failed(format!("{err:#}")),
        })
    }

    /// The fsck units systemd recorded, over the system bus. Soft: a bus that
    /// does not answer yields no check evidence, never an error.
    async fn checks(&self) -> Vec<CheckEvidence> {
        let Ok(connection) = zbus::Connection::system().await else {
            return Vec::new();
        };
        let Ok(manager) = zbus::Proxy::new(
            &connection,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await
        else {
            return Vec::new();
        };
        let listed: zbus::Result<Vec<SystemdUnit>> = manager
            .call(
                "ListUnitsByPatterns",
                &(Vec::<String>::new(), vec![FSCK_UNIT_PATTERN.to_string()]),
            )
            .await;
        let Ok(listed) = listed else {
            return Vec::new();
        };
        let mut checks = Vec::new();
        for unit in listed {
            let mut check = CheckEvidence {
                unit: unit.0,
                active_state: Some(unit.3),
                result: None,
                exit_status: None,
            };
            if let Ok(service) = zbus::Proxy::new(
                &connection,
                "org.freedesktop.systemd1",
                unit.6.clone(),
                "org.freedesktop.systemd1.Service",
            )
            .await
            {
                check.result = service.get_property::<String>("Result").await.ok();
                check.exit_status = service.get_property::<i32>("ExecMainStatus").await.ok();
            }
            checks.push(check);
        }
        checks
    }
}

/// `ListUnitsByPatterns` returns `a(ssssssouso)`: name, description, load
/// state, active state, sub state, followed unit, object path, job id, job
/// type, job object path. Only the name, the active state and the object path
/// are read.
type SystemdUnit = (
    String,
    String,
    String,
    String,
    String,
    String,
    zbus::zvariant::OwnedObjectPath,
    u32,
    String,
    zbus::zvariant::OwnedObjectPath,
);

/// The unit family systemd runs one instance of per fstab entry with a
/// non-zero pass number.
const FSCK_UNIT_PATTERN: &str = "systemd-fsck@*.service";

/// sysfs reports block sizes in 512-byte sectors regardless of the device's
/// own block size.
const SECTOR_BYTES: u64 = 512;

/// Block devices that are not physical media: nothing under these names has
/// wear to report or a lifetime to run out.
fn is_virtual_block(name: &str) -> bool {
    ["loop", "ram", "zram", "dm-", "md", "sr"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// The medium class a kernel block name implies.
fn medium_kind(name: &str) -> &'static str {
    if name.starts_with("mmcblk") {
        "mmc"
    } else if name.starts_with("nvme") {
        "nvme"
    } else if name.starts_with("sd") {
        "scsi"
    } else {
        "other"
    }
}

/// Why a medium has no normalized health, said plainly.
///
/// NVMe and SATA wear lives behind SMART, and this image ships no reader for
/// it — no `smartctl`, no `nvme-cli`, and micad links no SMART library. That
/// is a build decision with a name, so the surface reports the decision
/// rather than an empty health object that reads as "fine".
fn unsupported_reason(name: &str) -> &'static str {
    match medium_kind(name) {
        "mmc" => "the mmc driver exports no life_time for this device",
        "nvme" | "scsi" => {
            "SMART is not readable: this image ships no smartctl or nvme-cli, by design"
        }
        _ => "no health source is defined for this medium class",
    }
}

/// PLAN-061's probe write, in full: create exclusively, fsync the file,
/// remove it, fsync the directory.
///
/// The whole sequence, not just the create. A create that never reached the
/// medium proves nothing about a filesystem that will be asked to hold an
/// update bundle across a reboot, and leaving the file behind would make the
/// probe a slow leak. The name carries the pid so two daemons cannot collide,
/// and mode 0600 so nothing else reads or writes it.
fn write_probe(subtree: &Path) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let path = subtree.join(format!(".micad-storage-probe.{}", std::process::id()));
    let outcome = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        file.write_all(b"micad storage readiness probe\n")
            .with_context(|| format!("write {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync {}", path.display()))?;
        Ok(())
    })();
    // The removal runs whatever the write did, so a failed probe does not
    // leave its own evidence behind on the filesystem it just failed on.
    let removed = std::fs::remove_file(&path);
    outcome?;
    removed.with_context(|| format!("remove {}", path.display()))?;
    std::fs::File::open(subtree)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("fsync {}", subtree.display()))?;
    Ok(())
}

/// One mount point's space, from `df -P -B1`.
fn df_space(mount: &str) -> Option<FsSpace> {
    let output = std::process::Command::new("df")
        .args(["-P", "-B1", mount])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_df(&String::from_utf8_lossy(&output.stdout))
}

async fn bounded_storage_output(mut command: tokio::process::Command) -> Option<String> {
    command
        .kill_on_drop(true)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let output = tokio::time::timeout(std::time::Duration::from_secs(2), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() || output.stdout.len() > 16384 {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[async_trait::async_trait]
impl StorageStatusSource for HostStorage {
    async fn observe(&self) -> Result<StorageEvidence> {
        let (media, partition_sizes) = self.media();
        let mounts = self.mounts();
        let checks = checks_by_device(&self.checks().await, |device| self.resolve_dev(device));

        let mut tiers = BTreeMap::new();
        for spec in TIERS {
            let Some(device) =
                self.resolve_dev(&format!("/dev/disk/by-partlabel/{}", spec.partition_label))
            else {
                // No partition with this label: the tier is not on this board,
                // which is an answer and is reported as one.
                continue;
            };
            // The tier's OWN mountpoint, preferred over any other mount of the
            // same device. Under PLAN-063 the DATA partition appears three
            // times in the mount table -- at /mnt/data and at both binds --
            // so matching on the device alone would report whichever the
            // kernel happened to list first as "the DATA tier's mount".
            let mount = spec
                .mount
                .and_then(|want| {
                    mounts
                        .iter()
                        .find(|mount| mount.device == device && mount.mount == want)
                })
                .cloned();
            let space = mount.as_ref().and_then(|mount| (self.space)(&mount.mount));
            tiers.insert(
                spec.name.to_string(),
                TierEvidence {
                    partition_bytes: partition_sizes.get(&device).copied(),
                    check: checks.get(&device).cloned(),
                    device: Some(device),
                    mount,
                    space,
                },
            );
        }

        let data_path = self.path("mnt/data");
        let directory_paths: Vec<_> = DATA_DIRECTORIES
            .iter()
            .map(|name| data_path.join(name))
            .filter(|path| path.symlink_metadata().is_ok_and(|meta| meta.is_dir()))
            .collect();
        let mut directory_bytes = BTreeMap::new();
        if !directory_paths.is_empty() {
            let mut command = tokio::process::Command::new("/usr/bin/du");
            command
                .args(["-s", "-x", "-B1", "--"])
                .args(&directory_paths);
            if let Some(output) = bounded_storage_output(command).await {
                for line in output.lines() {
                    if let Some((size, path)) = line.split_once('\t') {
                        for name in DATA_DIRECTORIES {
                            if std::path::Path::new(path) == data_path.join(name)
                                && let Ok(size) = size.parse::<u64>()
                            {
                                directory_bytes.insert(name.to_string(), size);
                            }
                        }
                    }
                }
            }
        }
        let mut command = tokio::process::Command::new("/usr/sbin/repquota");
        command.args(["-P", "-n", "-O", "csv"]).arg(&data_path);
        let project_quotas = bounded_storage_output(command)
            .await
            .and_then(|output| parse_project_quotas(&output));

        let binds = BINDS
            .iter()
            .map(|spec| (spec.name.to_string(), self.bind(spec, &mounts)))
            .collect();
        Ok(StorageEvidence {
            directory_bytes,
            project_quotas,
            tiers,
            media,
            binds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(total: u64, used: u64, free: u64) -> FsSpace {
        FsSpace {
            total,
            used,
            free,
            reserved: total.saturating_sub(used).saturating_sub(free),
        }
    }

    fn mounted(device: &str, mount: &str) -> MountEvidence {
        MountEvidence {
            device: device.to_string(),
            root: BINDS
                .iter()
                .find(|spec| spec.mount == mount)
                .and_then(|spec| spec.source.strip_prefix(DATA_MOUNT))
                .unwrap_or("/")
                .to_string(),
            mount: mount.to_string(),
            fstype: "ext4".to_string(),
            read_only: false,
        }
    }

    /// The band, driven as a sequence rather than as isolated readings: a
    /// hysteresis threshold is only meaningful against the state before it.
    /// A reading inside a band must HOLD the previous state, which is the
    /// property that stops a tier hovering on a threshold from flapping.
    #[test]
    fn the_pressure_band_holds_its_state_between_the_thresholds() {
        let tracker = PressureTracker::default();
        assert_eq!(tracker.observe("data", 10), Pressure::Normal);
        // 78% is inside the warning band but has not crossed the enter
        // threshold: still normal.
        assert_eq!(tracker.observe("data", 78), Pressure::Normal);
        assert_eq!(tracker.observe("data", 80), Pressure::Warning);
        // Back to 78: the enter threshold was crossed, so it holds until the
        // clear threshold.
        assert_eq!(tracker.observe("data", 78), Pressure::Warning);
        assert_eq!(tracker.observe("data", 74), Pressure::Normal);

        assert_eq!(tracker.observe("data", 91), Pressure::Critical);
        // 86% is below the critical enter threshold and above its clear
        // threshold: still critical.
        assert_eq!(tracker.observe("data", 86), Pressure::Critical);
        assert_eq!(tracker.observe("data", 84), Pressure::Warning);
        assert_eq!(tracker.observe("data", 60), Pressure::Normal);

        // Each tier carries its own state; one tier's pressure must not
        // decide another's.
        assert_eq!(tracker.observe("state", 86), Pressure::Warning);
        assert_eq!(tracker.observe("data", 86), Pressure::Warning);
    }

    /// Without hysteresis both readings below would classify identically.
    /// This is the mutation control for [`next_pressure`]: collapse the two
    /// thresholds into one and this test fails.
    #[test]
    fn the_same_reading_classifies_differently_by_where_it_came_from() {
        assert_eq!(next_pressure(Pressure::Normal, 77), Pressure::Normal);
        assert_eq!(next_pressure(Pressure::Warning, 77), Pressure::Warning);
        assert_eq!(next_pressure(Pressure::Critical, 87), Pressure::Critical);
        assert_eq!(next_pressure(Pressure::Warning, 87), Pressure::Warning);
    }

    fn data_tier(device: &str) -> TierEvidence {
        TierEvidence {
            device: Some(device.to_string()),
            mount: Some(mounted(device, DATA_MOUNT)),
            space: Some(space(1000, 100, 900)),
            ..TierEvidence::default()
        }
    }

    fn bound(device: &str) -> BindEvidence {
        BindEvidence {
            mount: Some(mounted(device, "/mica")),
            source_is_directory: Some(true),
            probe: Some(ProbeOutcome::Passed),
        }
    }

    /// PLAN-061's readiness contract, driven case by case. The two
    /// `Unavailable` cases are the ones that matter: each is a state in which
    /// a writer must NOT fall back to another filesystem, so neither may be
    /// softened to `degraded`.
    #[test]
    fn a_bind_is_ready_only_when_it_is_actually_data_and_writable() {
        let data = data_tier("/dev/mmcblk0p11");
        assert_eq!(
            classify_readiness(&bound("/dev/mmcblk0p11"), Some(&data), Pressure::Normal),
            Readiness::Ready
        );

        // Not mounted at all.
        let unmounted = BindEvidence {
            mount: None,
            ..bound("/dev/mmcblk0p11")
        };
        assert_eq!(
            classify_readiness(&unmounted, Some(&data), Pressure::Normal),
            Readiness::Unavailable
        );

        // Mounted, but from something that is not the DATA partition: the
        // path is right and the filesystem is wrong, which is exactly the
        // fallback PLAN-061 forbids.
        assert_eq!(
            classify_readiness(&bound("/dev/mmcblk0p9"), Some(&data), Pressure::Normal),
            Readiness::Unavailable
        );

        // The source under /mnt/data is a symlink, not a directory:
        // substitution, which mica-data-layout dies on.
        let substituted = BindEvidence {
            source_is_directory: Some(false),
            ..bound("/dev/mmcblk0p11")
        };
        assert_eq!(
            classify_readiness(&substituted, Some(&data), Pressure::Normal),
            Readiness::Unavailable
        );

        // Read-only, a failed probe and a critically full pool are each
        // degraded: the namespace is the right one, it just cannot be
        // written now.
        let read_only = BindEvidence {
            mount: Some(MountEvidence {
                read_only: true,
                ..mounted("/dev/mmcblk0p11", "/mica")
            }),
            ..bound("/dev/mmcblk0p11")
        };
        assert_eq!(
            classify_readiness(&read_only, Some(&data), Pressure::Normal),
            Readiness::Degraded
        );
        let failed = BindEvidence {
            probe: Some(ProbeOutcome::Failed("ENOSPC".to_string())),
            ..bound("/dev/mmcblk0p11")
        };
        assert_eq!(
            classify_readiness(&failed, Some(&data), Pressure::Normal),
            Readiness::Degraded
        );
        assert_eq!(
            classify_readiness(&bound("/dev/mmcblk0p11"), Some(&data), Pressure::Critical),
            Readiness::Degraded
        );

        // A probe that did not run never upgrades a verdict and never
        // downgrades one: it is silence, and silence is neither.
        let not_attempted = BindEvidence {
            probe: Some(ProbeOutcome::NotAttempted("no subtree".to_string())),
            ..bound("/dev/mmcblk0p11")
        };
        assert_eq!(
            classify_readiness(&not_attempted, Some(&data), Pressure::Normal),
            Readiness::Ready
        );

        // No DATA tier to compare against: unknown, not ready. Claiming
        // readiness here would rest on a comparison nobody made.
        assert_eq!(
            classify_readiness(&bound("/dev/mmcblk0p11"), None, Pressure::Normal),
            Readiness::Unknown
        );
    }

    /// A probe is rendered as what it was. The `NotAttempted` case must never
    /// serialize anything a client could read as a pass.
    #[test]
    fn a_probe_that_did_not_run_never_renders_as_passed() {
        let render = |probe: ProbeOutcome| {
            bind_json(
                &BINDS[0],
                Some(&BindEvidence {
                    probe: Some(probe),
                    ..bound("/dev/mmcblk0p11")
                }),
                Some(&data_tier("/dev/mmcblk0p11")),
                Pressure::Normal,
            )["probe"]
                .clone()
        };
        assert_eq!(render(ProbeOutcome::Passed)["passed"], true);
        assert_eq!(
            render(ProbeOutcome::Failed("EROFS".into()))["passed"],
            false
        );
        assert_eq!(
            render(ProbeOutcome::Failed("EROFS".into()))["error"],
            "EROFS"
        );

        let skipped = render(ProbeOutcome::NotAttempted(
            "mica-data-layout has not run".into(),
        ));
        assert_eq!(skipped["attempted"], false);
        assert_eq!(skipped.get("passed"), None, "{skipped}");
        assert_eq!(skipped["reason"], "mica-data-layout has not run");
    }

    /// `sourceOnData` answers the contract's first question on its own,
    /// rather than only through the verdict.
    #[test]
    fn a_bind_reports_whether_its_source_is_really_data() {
        let data = data_tier("/dev/mmcblk0p11");
        let right = bind_json(
            &BINDS[0],
            Some(&bound("/dev/mmcblk0p11")),
            Some(&data),
            Pressure::Normal,
        );
        assert_eq!(right["sourceOnData"], true);
        assert_eq!(right["readiness"], "ready");
        assert_eq!(right["source"], "/mnt/data/mica");
        assert_eq!(right["owner"], "system");

        let wrong = bind_json(
            &BINDS[0],
            Some(&bound("/dev/mmcblk0p9")),
            Some(&data),
            Pressure::Normal,
        );
        assert_eq!(wrong["sourceOnData"], false);
        assert_eq!(wrong["readiness"], "unavailable");

        // No evidence at all is `unknown` with a reason, not a quiet default.
        let unseen = bind_json(&BINDS[1], None, Some(&data), Pressure::Normal);
        assert_eq!(unseen["readiness"], "unknown");
        assert!(unseen["detail"].as_str().is_some(), "{unseen}");
    }

    /// The JEDEC lifetime register is a 10% bucket, and it is reported as one.
    /// `0x00` is "not defined" and must not become a bucket.
    #[test]
    fn the_emmc_lifetime_register_decodes_to_the_bucket_it_is() {
        assert_eq!(parse_life_time("0x01"), Some((0, 10)));
        assert_eq!(parse_life_time("0x02"), Some((10, 20)));
        assert_eq!(parse_life_time("0x0A"), Some((90, 100)));
        assert_eq!(parse_life_time("0x0B"), Some((100, 100)));
        assert_eq!(parse_life_time("0x00"), None);
        assert_eq!(parse_life_time("0x0F"), None);
        assert_eq!(parse_life_time("nonsense"), None);

        assert_eq!(parse_pre_eol("0x01"), "normal");
        assert_eq!(parse_pre_eol("0x02"), "warning");
        assert_eq!(parse_pre_eol("0x03"), "urgent");
        // 0x00 is the device declining to answer, which is NOT "normal".
        assert_eq!(parse_pre_eol("0x00"), "undefined");
        assert_eq!(parse_pre_eol(""), "undefined");
    }

    /// A device with no wear registers reports unsupported WITH the reason,
    /// and the reason names the build decision rather than the device.
    #[test]
    fn a_medium_without_wear_registers_reports_unsupported_and_why() {
        let health = health_json(&MediaHealth::Unsupported(
            unsupported_reason("nvme0n1").to_string(),
        ));
        assert_eq!(health["supported"], false);
        assert!(
            health["reason"].as_str().unwrap().contains("smartctl"),
            "{health}"
        );
        assert!(health.get("lifetimeEstimates").is_none(), "{health}");

        let emmc = health_json(&MediaHealth::Emmc {
            life_time_raw: "0x03 0x02".to_string(),
            pre_eol_raw: Some("0x02".to_string()),
        });
        assert_eq!(emmc["supported"], true);
        assert_eq!(emmc["preEol"], "warning");
        assert_eq!(emmc["lifetimeEstimates"][0]["usedPercentMin"], 20);
        assert_eq!(emmc["lifetimeEstimates"][0]["usedPercentMax"], 30);
        assert_eq!(emmc["lifetimeEstimates"][1]["usedPercentMax"], 20);
        // The raw registers survive normalization for support to read.
        assert_eq!(emmc["raw"]["lifeTime"], "0x03 0x02");
        assert_eq!(emmc["raw"]["preEolInfo"], "0x02");
    }

    /// A unit name is a path, and a partuuid path is full of literal hyphens:
    /// a plain `-` to `/` replacement would name a device that does not exist.
    #[test]
    fn an_fsck_unit_name_decodes_to_the_device_it_checked() {
        assert_eq!(
            fsck_unit_device(
                r"systemd-fsck@dev-disk-by\x2dpartuuid-5ac35760\x2d0002\x2d4000.service"
            )
            .as_deref(),
            Some("/dev/disk/by-partuuid/5ac35760-0002-4000")
        );
        assert_eq!(
            fsck_unit_device("systemd-fsck@dev-mmcblk0p9.service").as_deref(),
            Some("/dev/mmcblk0p9")
        );
        assert_eq!(fsck_unit_device("systemd-fsck-root.service"), None);
        assert_eq!(fsck_unit_device("micad.service"), None);
        assert_eq!(fsck_unit_device("systemd-fsck@.service"), None);
    }

    /// The pairing from unit to tier device, through the symlink `/etc/fstab`
    /// actually names the tiers by.
    #[test]
    fn recorded_checks_pair_with_the_devices_they_checked() {
        let units = vec![
            CheckEvidence {
                unit: r"systemd-fsck@dev-disk-by\x2dpartuuid-state\x2duuid.service".to_string(),
                active_state: Some("inactive".to_string()),
                result: Some("success".to_string()),
                exit_status: Some(0),
            },
            CheckEvidence {
                unit: "not-an-fsck-unit.service".to_string(),
                active_state: None,
                result: None,
                exit_status: None,
            },
        ];
        let paired = checks_by_device(&units, |device| {
            (device == "/dev/disk/by-partuuid/state-uuid").then(|| "/dev/mmcblk0p9".to_string())
        });
        assert_eq!(paired.len(), 1);
        assert_eq!(paired["/dev/mmcblk0p9"].exit_status, Some(0));

        // A unit whose symlink no longer resolves keeps the name it had,
        // rather than being dropped: unmatched evidence is still evidence.
        let unresolved = checks_by_device(&units, |_| None);
        assert!(unresolved.contains_key("/dev/disk/by-partuuid/state-uuid"));
    }

    #[test]
    fn mountinfo_yields_the_device_mount_and_read_only_flag() {
        let text = concat!(
            "25 1 254:0 / / ro,noatime shared:1 - squashfs /dev/dm-0 ro\n",
            "31 25 179:9 / /mnt/state rw,noatime shared:2 - ext4 /dev/mmcblk0p9 rw\n",
            r"33 25 179:11 / /srv\040data rw,noatime - ext4 /dev/mmcblk0p11 rw",
            "\n",
        );
        let mounts = parse_mountinfo(text);
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].device, "/dev/dm-0");
        assert!(mounts[0].read_only);
        assert_eq!(mounts[0].fstype, "squashfs");
        assert_eq!(mounts[1].mount, "/mnt/state");
        assert!(!mounts[1].read_only);
        // The optional-tag run before the `-` varies in length, so the fields
        // after it cannot be indexed from the left; line 3 has none at all.
        assert_eq!(mounts[2].device, "/dev/mmcblk0p11");
        // Octal escapes are the kernel's, and a path is not a mount point
        // until they are decoded.
        assert_eq!(mounts[2].mount, "/srv data");
    }

    #[test]
    fn df_output_yields_the_reserved_pool_as_its_own_number() {
        let output = concat!(
            "Filesystem     1B-blocks       Used  Available Capacity Mounted on\n",
            "/dev/mmcblk0p11 1000000000 700000000  250000000      74% /srv\n",
        );
        let space = parse_df(output).expect("the POSIX layout parses");
        assert_eq!(space.total, 1_000_000_000);
        assert_eq!(space.used, 700_000_000);
        assert_eq!(space.free, 250_000_000);
        // Neither free nor used: the pool only root can write into.
        assert_eq!(space.reserved, 50_000_000);
        // 70%, not 73%: the reserved pool is space the tier does not have.
        assert_eq!(space.used_percent(), 70);

        assert_eq!(parse_df("Filesystem\n"), None);
        assert_eq!(parse_df(""), None);
    }

    /// The served shape, member by member. The consumers are apid's route in
    /// another crate and the UI beyond it, and they read these names.
    #[test]
    fn the_status_json_carries_every_tier_and_names_the_absent_ones() {
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "data".to_string(),
            TierEvidence {
                device: Some("/dev/mmcblk0p11".to_string()),
                partition_bytes: Some(30_000_000_000),
                mount: Some(mounted("/dev/mmcblk0p11", DATA_MOUNT)),
                space: Some(space(1000, 850, 100)),
                check: Some(CheckEvidence {
                    unit: "systemd-fsck@dev-mmcblk0p11.service".to_string(),
                    active_state: Some("inactive".to_string()),
                    result: Some("success".to_string()),
                    exit_status: Some(1),
                }),
            },
        );
        tiers.insert(
            "system".to_string(),
            TierEvidence {
                device: Some("/dev/mmcblk0p6".to_string()),
                partition_bytes: Some(268_435_456),
                mount: Some(MountEvidence {
                    device: "/dev/mmcblk0p6".to_string(),
                    root: "/".to_string(),
                    mount: "/mnt/system".to_string(),
                    fstype: "ext4".to_string(),
                    read_only: true,
                }),
                ..TierEvidence::default()
            },
        );
        let evidence = StorageEvidence {
            directory_bytes: BTreeMap::new(),
            project_quotas: None,
            tiers,
            binds: [("mica".to_string(), bound("/dev/mmcblk0p11"))]
                .into_iter()
                .collect(),
            media: vec![MediumEvidence {
                name: "mmcblk0".to_string(),
                kind: "mmc".to_string(),
                size_bytes: Some(31_000_000_000),
                model: Some("SDINBDA4".to_string()),
                rotational: Some(false),
                health: MediaHealth::Emmc {
                    life_time_raw: "0x01 0x01".to_string(),
                    pre_eol_raw: Some("0x01".to_string()),
                },
            }],
        };
        let value = status_json(&evidence, &PressureTracker::default());

        let tiers = value["tiers"].as_array().expect("tiers is an array");
        assert_eq!(tiers.len(), TIERS.len(), "every tier is reported: {value}");
        let by_name = |name: &str| {
            tiers
                .iter()
                .find(|tier| tier["name"] == name)
                .unwrap_or_else(|| panic!("{name} is missing from {value}"))
                .clone()
        };

        let data = by_name("data");
        assert_eq!(data["present"], true);
        assert_eq!(data["mounted"], true);
        // The DATA partition's own mountpoint, not either bind's: PLAN-063
        // puts the filesystem at /mnt/data and exposes /mica and /srv on top.
        assert_eq!(data["mount"], DATA_MOUNT);
        assert_eq!(data["readOnly"], false);
        assert_eq!(data["role"], "ext4");
        assert_eq!(data["space"]["reservedBytes"], 50);
        assert_eq!(data["space"]["usedPercent"], 85);
        assert_eq!(data["pressure"], "warning");

        // One filesystem, multiple namespaces. The binds carry no capacity of
        // their own -- a `space` object on either would be the DATA tier's
        // bytes reported a second time, and a reader summing the three would
        // get three times the disk.
        let namespaces = &value["namespaces"];
        assert_eq!(namespaces["sharedCapacityTier"], DATA_TIER);
        let binds = namespaces["binds"].as_array().expect("binds is an array");
        assert_eq!(binds.len(), BINDS.len());
        for bind in binds {
            assert!(
                bind.get("space").is_none(),
                "a bind reported capacity: {bind}"
            );
            assert!(
                bind.get("partitionBytes").is_none(),
                "a bind reported a partition size: {bind}"
            );
        }
        // fsck exit 1 is "errors were corrected", which is real repair
        // evidence and is surfaced rather than folded into a boolean.
        assert_eq!(data["check"]["exitStatus"], 1);
        assert_eq!(data["check"]["result"], "success");

        // SYSTEM holds immutable deployment objects and stays read-only.
        let system = by_name("system");
        assert_eq!(system["mount"], "/mnt/system");
        assert_eq!(system["readOnly"], true);
        assert_eq!(system["partitionBytes"], 268_435_456u64);
        // No mounted filesystem to measure, so no invented space object.
        assert!(system.get("space").is_none(), "{system}");
        // Never checked, and it says so rather than reading as clean.
        assert_eq!(system["check"]["recorded"], false);

        // A tier this board does not have is present in the array and says
        // it is absent; omitting it would leave the reader to guess.
        let esp = by_name("esp");
        assert_eq!(esp["present"], false);
        assert!(esp["detail"].as_str().unwrap().contains("esp"), "{esp}");

        // Only the watched tiers carry a pressure classification; the others
        // would need thresholds nobody has set.
        assert!(by_name("system").get("pressure").is_none());

        assert_eq!(value["media"][0]["kind"], "mmc");
        assert_eq!(value["media"][0]["health"]["supported"], true);
        assert_eq!(value["policy"]["warningPercent"], WARNING_ENTER_PERCENT);
    }

    #[test]
    fn storage_does_not_invent_an_update_space_reservation() {
        let mut evidence = StorageEvidence::default();
        evidence.tiers.insert(
            "data".into(),
            TierEvidence {
                space: Some(space(1_000_000_000, 100_000_000, 900_000_000)),
                ..TierEvidence::default()
            },
        );
        let value = status_json(&evidence, &PressureTracker::default());
        let data = value["tiers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "data")
            .unwrap();
        assert!(data.get("updateWorkspace").is_none());
        assert!(
            value["policy"]
                .get("updateWorkspaceReservedBytes")
                .is_none()
        );
    }

    /// Every lifecycle decision PLAN-049 lists is answered, and every current
    /// answer is `unsupported`. The list is asserted by name so that adding a
    /// capability without deciding its lifecycle answer fails here.
    #[test]
    fn every_lifecycle_decision_is_explicit_and_currently_unsupported() {
        let value = status_json(&StorageEvidence::default(), &PressureTracker::default());
        let lifecycle = value["lifecycle"]
            .as_object()
            .expect("lifecycle is an object");
        let mut names: Vec<&str> = lifecycle.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "backupRestore",
                "dataPreservingReplacement",
                "encryption",
                "factoryReset",
                "offlineRepair",
                "removableMedia",
                "secureErase",
            ]
        );
        for (name, decision) in lifecycle {
            assert_eq!(decision, "unsupported", "{name} claims more than it has");
        }
    }

    /// The observer's assembly — label to device to mount to medium — over a
    /// fixture sysfs, so the pairing that would break silently on a device is
    /// exercised on the build host.
    #[tokio::test]
    async fn the_host_observer_pairs_tiers_with_their_devices_and_media() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path();
        let write = |relative: &str, contents: &str| {
            let target = path.join(relative);
            std::fs::create_dir_all(target.parent().expect("a parent")).expect("mkdir");
            std::fs::write(target, contents).expect("write");
        };

        // One eMMC with the two tiers this test cares about.
        write("sys/block/mmcblk0/size", "60000000\n");
        write("sys/block/mmcblk0/queue/rotational", "0\n");
        write("sys/block/mmcblk0/device/name", "SDINBDA4\n");
        write("sys/block/mmcblk0/device/life_time", "0x02 0x01\n");
        write("sys/block/mmcblk0/device/pre_eol_info", "0x01\n");
        write("sys/block/mmcblk0/mmcblk0p6/partition", "6\n");
        write("sys/block/mmcblk0/mmcblk0p6/size", "524288\n");
        write("sys/block/mmcblk0/mmcblk0p11/partition", "11\n");
        write("sys/block/mmcblk0/mmcblk0p11/size", "40000000\n");
        // A loop device, which is not a medium and must not be reported as one.
        write("sys/block/loop0/size", "1024\n");
        // The verity device the root is mounted from, and the slot under it.
        write("sys/block/dm-0/slaves/loop0/.keep", "");

        std::fs::create_dir_all(path.join("dev/disk/by-partlabel")).expect("mkdir");
        std::os::unix::fs::symlink("../../mmcblk0p11", path.join("dev/disk/by-partlabel/data"))
            .expect("symlink");
        std::os::unix::fs::symlink("../../mmcblk0p6", path.join("dev/disk/by-partlabel/system"))
            .expect("symlink");

        // The PLAN-063 layout: DATA at /mnt/data, and the same device bound
        // twice on top of it. The DATA row is listed LAST on purpose -- a
        // tier lookup that matched on the device alone would pick /mica here
        // and report it as the DATA tier's own mountpoint.
        write(
            "proc/self/mountinfo",
            concat!(
                "25 1 254:0 / / ro,noatime shared:1 - squashfs /dev/dm-0 ro\n",
                "32 25 179:6 / /mnt/system ro - ext4 /dev/mmcblk0p6 ro\n",
                "33 25 179:11 /mica /mica rw,noatime - ext4 /dev/mmcblk0p11 rw\n",
                "34 25 179:11 /srv /srv rw,noatime - ext4 /dev/mmcblk0p11 rw\n",
                "35 25 179:11 / /mnt/data rw,noatime - ext4 /dev/mmcblk0p11 rw\n",
            ),
        );
        // The two bind sources mica-data-layout creates, and the probe subtree
        // under the system one.
        std::fs::create_dir_all(path.join("mnt/data/mica")).expect("mkdir");
        std::fs::create_dir_all(path.join("mnt/data/srv")).expect("mkdir");
        std::fs::create_dir_all(path.join("mica/updates/staging")).expect("mkdir");

        let observer = HostStorage::at(path)
            .with_space_reader(|mount| (mount == DATA_MOUNT).then(|| space(1000, 100, 850)));
        let evidence = observer.observe().await.expect("the fixture observes");

        // Only the two labelled tiers exist here; the rest are absent, which
        // status_json renders as `present: false`.
        let mut names: Vec<&str> = evidence.tiers.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["data", "system"]);

        let data = &evidence.tiers["data"];
        assert_eq!(data.device.as_deref(), Some("/dev/mmcblk0p11"));
        assert_eq!(data.partition_bytes, Some(40_000_000 * 512));
        assert_eq!(
            data.mount.as_ref().map(|mount| mount.mount.as_str()),
            Some(DATA_MOUNT),
            "the DATA tier reported a bind's mountpoint as its own"
        );
        assert_eq!(data.space.map(|space| space.used), Some(100));

        // Both binds observed, both on the DATA device, and the probe ran in
        // the system namespace only.
        let mica = &evidence.binds["mica"];
        assert_eq!(
            mica.mount.as_ref().map(|mount| mount.device.as_str()),
            Some("/dev/mmcblk0p11")
        );
        assert_eq!(mica.source_is_directory, Some(true));
        assert_eq!(mica.probe, Some(ProbeOutcome::Passed));
        // The probe cleans up after itself: a readiness check that leaves
        // files behind is a slow leak on the filesystem it is vouching for.
        let leftovers: Vec<_> = std::fs::read_dir(path.join("mica/updates/staging"))
            .expect("read staging")
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .collect();
        assert!(leftovers.is_empty(), "probe left {leftovers:?} behind");

        let srv = &evidence.binds["srv"];
        assert_eq!(srv.source_is_directory, Some(true));
        // No probe in the user-owned namespace, and it says why rather than
        // reporting a pass nobody earned.
        match &srv.probe {
            Some(ProbeOutcome::NotAttempted(reason)) => {
                assert!(reason.contains("no probe write"), "{reason}")
            }
            other => panic!("expected /srv to be un-probed, got {other:?}"),
        }

        assert_eq!(
            classify_readiness(mica, evidence.data_tier(), Pressure::Normal),
            Readiness::Ready
        );

        // SYSTEM is the physical filesystem containing immutable images.
        let rootfs = &evidence.tiers["system"];
        assert_eq!(
            rootfs.mount.as_ref().map(|mount| mount.mount.as_str()),
            Some("/mnt/system")
        );
        assert!(rootfs.mount.as_ref().is_some_and(|mount| mount.read_only));
        // No space reader answers for `/`, and none is invented.
        assert_eq!(rootfs.space, None);

        assert_eq!(evidence.media.len(), 1, "loop0 is not a medium");
        let medium = &evidence.media[0];
        assert_eq!(medium.name, "mmcblk0");
        assert_eq!(medium.size_bytes, Some(60_000_000 * 512));
        assert_eq!(medium.model.as_deref(), Some("SDINBDA4"));
        assert_eq!(medium.rotational, Some(false));
        assert_eq!(
            medium.health,
            MediaHealth::Emmc {
                life_time_raw: "0x02 0x01".to_string(),
                pre_eol_raw: Some("0x01".to_string()),
            }
        );
    }

    /// The fail-closed side of the layout contract, over a fixture where
    /// `mica-data-layout` has NOT run: the bind sources are absent, the probe
    /// subtree is absent, and every one of those is reported as the reason it
    /// is rather than as a quiet pass.
    #[tokio::test]
    async fn an_uninitialized_layout_reports_unavailable_and_an_unattempted_probe() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path();
        std::fs::create_dir_all(path.join("dev/disk/by-partlabel")).expect("mkdir");
        std::fs::create_dir_all(path.join("sys/block/mmcblk0/mmcblk0p11")).expect("mkdir");
        std::fs::write(path.join("sys/block/mmcblk0/size"), "60000000\n").expect("write");
        std::fs::write(path.join("sys/block/mmcblk0/mmcblk0p11/partition"), "11\n").expect("write");
        std::fs::write(path.join("sys/block/mmcblk0/mmcblk0p11/size"), "40000000\n")
            .expect("write");
        std::os::unix::fs::symlink("../../mmcblk0p11", path.join("dev/disk/by-partlabel/data"))
            .expect("symlink");
        // DATA is mounted, but nothing has created the namespaces on it.
        std::fs::create_dir_all(path.join("proc/self")).expect("mkdir");
        std::fs::write(
            path.join("proc/self/mountinfo"),
            "35 25 179:11 / /mnt/data rw,noatime - ext4 /dev/mmcblk0p11 rw\n",
        )
        .expect("write");

        let evidence = HostStorage::at(path)
            .observe()
            .await
            .expect("the fixture observes");

        for name in ["mica", "srv"] {
            let bind = &evidence.binds[name];
            assert_eq!(bind.mount, None, "{name} is not mounted in this fixture");
            assert_eq!(bind.source_is_directory, None, "{name} has no source yet");
            assert_eq!(
                classify_readiness(bind, evidence.data_tier(), Pressure::Normal),
                Readiness::Unavailable,
                "{name} must be unavailable, so no writer falls back elsewhere"
            );
        }
        match &evidence.binds["mica"].probe {
            Some(ProbeOutcome::NotAttempted(reason)) => {
                assert!(reason.contains("/mica"), "{reason}")
            }
            other => panic!("expected an unattempted probe, got {other:?}"),
        }
    }

    /// A medium with no wear registers in sysfs takes the unsupported path
    /// with the reason attached — the negative control for the test above.
    #[tokio::test]
    async fn a_medium_without_sysfs_wear_registers_is_reported_unsupported() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path();
        std::fs::create_dir_all(path.join("sys/block/nvme0n1/device")).expect("mkdir");
        std::fs::write(path.join("sys/block/nvme0n1/size"), "2000000\n").expect("write");
        std::fs::write(path.join("sys/block/nvme0n1/device/model"), "SOME SSD\n").expect("write");

        let evidence = HostStorage::at(path)
            .observe()
            .await
            .expect("the fixture observes");
        assert!(
            evidence.tiers.is_empty(),
            "no partition labels in the fixture"
        );
        assert_eq!(evidence.media.len(), 1);
        match &evidence.media[0].health {
            MediaHealth::Unsupported(reason) => assert!(reason.contains("SMART"), "{reason}"),
            other => panic!("expected unsupported health, got {other:?}"),
        }
    }

    /// The default observer inspects nothing at all, which is what makes it
    /// safe as the daemon's default.
    #[tokio::test]
    async fn the_unavailable_observer_answers_no_evidence() {
        assert!(UnavailableStorageStatus.observe().await.is_err());
    }

    #[test]
    fn physical_tiers_follow_the_file_deployment_layout() {
        assert_eq!(
            TIERS.iter().map(|tier| tier.name).collect::<Vec<_>>(),
            ["esp", "firmware", "system", "data"]
        );
        let status = status_json(&StorageEvidence::default(), &PressureTracker::default());
        assert_eq!(status["policy"]["watchedTiers"], json!(["data"]));
    }

    #[test]
    fn a_bind_of_another_data_directory_is_unavailable() {
        let mut bind = bound("/dev/vda3");
        bind.mount = parse_mountinfo("33 25 254:3 /state /mica rw - ext4 /dev/vda3 rw\n").pop();
        assert_eq!(
            classify_readiness(&bind, Some(&data_tier("/dev/vda3")), Pressure::Normal),
            Readiness::Unavailable
        );
    }

    #[test]
    fn project_reports_use_bytes_and_inodes_without_duplicating_data_capacity() {
        let csv = "Project,BlockStatus,FileStatus,BlockUsed,BlockSoftLimit,BlockHardLimit,BlockGrace,FileUsed,FileSoftLimit,FileHardLimit,FileGrace\n#0,ok,ok,732,0,0,,45,0,0,\n#100,ok,ok,92,0,0,,23,0,0,\n#101,ok,ok,104,0,32768,,10,0,2048,\n#102,ok,ok,200,0,0,,50,0,0,\n";
        let projects = parse_project_quotas(csv).unwrap();
        assert_eq!(projects.len(), 3);
        assert_eq!(projects[&100].limit_bytes, 0);
        assert_eq!(projects[&100].limit_inodes, 0);
        assert_eq!(projects[&102].limit_bytes, 0);
        assert_eq!(projects[&102].limit_inodes, 0);
        assert_eq!(projects[&100].used_bytes, 92 * 1024);
        assert_eq!(projects[&101].limit_bytes, 32 * 1024 * 1024);
        assert_eq!(projects[&101].limit_inodes, 2048);
        assert!(parse_project_quotas("Project\n#100,unknown").is_none());
    }

    #[test]
    fn writable_var_usage_belongs_to_the_bounded_variable_project() {
        let mut evidence = StorageEvidence::default();
        evidence.directory_bytes.insert("var".into(), 4096);
        let status = status_json(&evidence, &PressureTracker::default());
        let directories = status["namespaces"]["directories"].as_array().unwrap();
        let var = directories
            .iter()
            .find(|entry| entry["name"] == "var")
            .unwrap();
        assert_eq!(var["usedBytes"], 4096);
        assert_eq!(var["project"], 101);
    }
    #[test]
    fn container_storage_is_an_independent_bind_and_project() {
        let status = status_json(&StorageEvidence::default(), &PressureTracker::default());
        let dirs = status["namespaces"]["directories"].as_array().unwrap();
        assert!(
            dirs.iter()
                .any(|entry| entry["name"] == "containers" && entry["project"] == 102)
        );
        assert!(
            BINDS
                .iter()
                .any(|spec| spec.mount == "/mica/containers"
                    && spec.source == "/mnt/data/containers")
        );
    }
}

//! Update orchestration: RAUC's D-Bus API behind a mockable trait.
//!
//! Like power actions, updates are actions, not settings — nothing is
//! persisted in the settings tree and nothing is reconciled on boot, so this
//! deliberately lives outside `reconciler/`. Progress and outcomes are
//! recorded in the LIVE-STATE tree under `update` by the bus layer.
//!
//! What this module deliberately does NOT do: confirm the booted slot. The
//! boot health gate (`rootfs/overlay/usr/lib/mos/mos-health`) owns the
//! PENDING_CONFIRM -> CONFIRMED edge — it probes systemd, mosd and apid and
//! only then runs `rauc status mark-good`. An automatic mark-good here would
//! duplicate that gate and, worse, could confirm a slot the gate would have
//! failed. The bus does expose a *manual* mark (`MarkUpdate`) for the operator
//! case the gate cannot decide — e.g. a failed unit outside the allowlist that
//! the operator has judged acceptable — and it is validated down to
//! `good`/`bad` on `booted`/`other`: activation (`active`) is the installer's
//! job and is not offered.
//!
//! The [`RaucClient`] trait exists so the bus layer can be unit-tested against
//! a mock, and so dry-run mode can substitute [`DryRunRauc`], which never
//! touches the host the daemon runs on. The production [`Rauc`] connects
//! lazily inside each call, like `Networkd` and the power control.

use std::collections::HashMap;
use std::future::poll_fn;
use std::path::Path;
use std::pin::pin;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use zbus::export::futures_core::Stream;

/// Well-known bus name RAUC owns.
pub const RAUC_SERVICE: &str = "de.pengutronix.rauc";
/// Object path of RAUC's installer object.
pub const RAUC_PATH: &str = "/";
/// Interface carrying the install/status/mark surface.
pub const RAUC_INTERFACE: &str = "de.pengutronix.rauc.Installer";
/// Signal RAUC emits when an install finishes (result 0 = success).
pub const COMPLETED_SIGNAL: &str = "Completed";

/// One slot's status, curated down to the fields mosd records.
///
/// Curated rather than passed through verbatim: RAUC's per-slot dictionary
/// carries checksums, sizes and device paths that would bloat the live-state
/// tree without informing any decision mosd or its UI makes. A field absent
/// from RAUC's answer is `None` here and absent from the recorded JSON.
#[derive(Debug, Clone, Default)]
pub struct SlotStatus {
    /// RAUC slot name, e.g. `rootfs.0`.
    pub name: String,
    /// `booted`, `active` or `inactive`.
    pub state: Option<String>,
    /// The bootloader-facing name (`A`/`B`), present on rootfs slots only.
    pub bootname: Option<String>,
    /// `good` or `bad`. For the U-Boot backend this reads the `BOOT_x_LEFT`
    /// attempt counter as a boolean — exhausted or not — NOT the
    /// confirmed/pending distinction; see [`unconfirmed_slot_warning`].
    pub boot_status: Option<String>,
    /// Version string of the bundle last installed into this slot.
    pub bundle_version: Option<String>,
    /// Timestamp of the last install into this slot.
    pub installed_timestamp: Option<String>,
    /// RAUC's own per-slot verdict, `ok` after a successful install.
    pub status: Option<String>,
}

impl SlotStatus {
    /// JSON object with only the present fields, so an absent value is a
    /// missing key rather than `null` — the same convention the settings
    /// tree uses for optional values.
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        let fields = [
            ("state", &self.state),
            ("bootname", &self.bootname),
            ("boot_status", &self.boot_status),
            ("bundle_version", &self.bundle_version),
            ("installed_timestamp", &self.installed_timestamp),
            ("status", &self.status),
        ];
        for (key, value) in fields {
            if let Some(value) = value {
                map.insert(key.to_string(), Value::String(value.clone()));
            }
        }
        Value::Object(map)
    }
}

/// Speaks to the update installer on the host.
#[async_trait::async_trait]
pub trait RaucClient: Send + Sync {
    /// Install `bundle` and wait for the install to FINISH — success means
    /// the new slot is written and activated, not merely that the install
    /// started. The caller decides how to run this without blocking anything
    /// (the bus layer spawns it onto a background task).
    async fn install_bundle(&self, bundle: &Path) -> Result<()>;

    /// Status of every configured slot.
    async fn slot_status(&self) -> Result<Vec<SlotStatus>>;

    /// Forward a mark to RAUC; returns `(slot_name, message)` as RAUC does.
    ///
    /// The bus layer validates `state`/`slot` BEFORE this is called; the
    /// trait forwards verbatim so the production behaviour stays RAUC's.
    async fn mark(&self, state: &str, slot: &str) -> Result<(String, String)>;

    /// RAUC's current operation, `idle` or `installing`.
    async fn operation(&self) -> Result<String>;

    /// Human-readable error of the last failed operation, empty when none.
    async fn last_error(&self) -> Result<String>;

    /// Install progress as `(percentage, message, nesting depth)`.
    async fn progress(&self) -> Result<(i32, String, i32)>;

    /// Name of the primary slot — the one the bootloader will try first —
    /// or `None` when RAUC does not name one.
    async fn primary(&self) -> Result<Option<String>>;
}

/// Everything one round of RAUC queries yields, gathered WITHOUT any mosd
/// lock held so a slow installer cannot stall the bus dispatcher.
pub struct UpdateQuery {
    pub operation: String,
    pub last_error: String,
    pub progress: (i32, String, i32),
    pub slots: Vec<SlotStatus>,
    pub primary: Option<String>,
}

/// Run one full round of status queries against `client`.
pub async fn query(client: &dyn RaucClient) -> Result<UpdateQuery> {
    Ok(UpdateQuery {
        operation: client.operation().await?,
        last_error: client.last_error().await?,
        progress: client.progress().await?,
        slots: client.slot_status().await?,
        primary: client.primary().await?,
    })
}

impl UpdateQuery {
    /// Write this query's findings into the live-state `update` object,
    /// field by field, so entries other writers own (`install`, `last_mark`)
    /// survive a refresh.
    pub fn merge_into(&self, entry: &mut serde_json::Map<String, Value>) {
        entry.insert("operation".into(), Value::String(self.operation.clone()));
        entry.insert("last_error".into(), Value::String(self.last_error.clone()));
        entry.insert(
            "progress".into(),
            json!({
                "percentage": self.progress.0,
                "message": self.progress.1,
                "depth": self.progress.2,
            }),
        );
        let slots: serde_json::Map<String, Value> = self
            .slots
            .iter()
            .map(|slot| (slot.name.clone(), slot.to_json()))
            .collect();
        entry.insert("slots".into(), Value::Object(slots));
        entry.insert(
            "booted_slot".into(),
            booted_slot(&self.slots).map_or(Value::Null, |slot| Value::String(slot.name.clone())),
        );
        entry.insert(
            "primary".into(),
            self.primary
                .as_ref()
                .map_or(Value::Null, |name| Value::String(name.clone())),
        );
        entry.insert(
            "pending_not_confirmed".into(),
            Value::Bool(pending_not_confirmed(&self.slots, self.primary.as_deref())),
        );
        entry.insert(
            "rollback".into(),
            rollback_eligibility(&self.slots, self.primary.as_deref()).to_json(),
        );
    }
}

/// The slot the system is running from, when RAUC names one.
pub fn booted_slot(slots: &[SlotStatus]) -> Option<&SlotStatus> {
    slots
        .iter()
        .find(|slot| slot.state.as_deref() == Some("booted"))
}

/// True when an installed-and-activated slot has not yet carried a boot:
/// the bootloader's first pick (`primary`) is not the slot we are running
/// from, which is exactly the window in which a reboot spends one of the new
/// slot's `BOOT_x_LEFT` attempts.
///
/// HONEST LIMIT, stated so nobody widens this check by guesswork: the other
/// unconfirmed window — booted into the new slot but before the health gate's
/// mark-good — is NOT visible here. Confirmation lives in the U-Boot attempt
/// counters, and RAUC's `boot-status` reads them only as exhausted-or-not.
/// Closing that window needs the health gate to report its confirmation into
/// mosd, which is an image-pipeline change and is not made here.
pub fn pending_not_confirmed(slots: &[SlotStatus], primary: Option<&str>) -> bool {
    match (booted_slot(slots), primary) {
        (Some(booted), Some(primary)) => booted.name != primary,
        _ => false,
    }
}

/// Warning to log and record before a reboot, or `None` when the slot state
/// gives no reason to hold the operator's hand.
pub fn unconfirmed_slot_warning(slots: &[SlotStatus], primary: Option<&str>) -> Option<String> {
    let booted = booted_slot(slots)?;
    // Exhausted attempts first: it is the stronger statement, and both can be
    // true at once (a bad booted slot with a fresh install waiting).
    if booted.boot_status.as_deref() == Some("bad") {
        return Some(format!(
            "booted slot {} has boot-status `bad` (its boot attempts are exhausted); \
             a reboot may fall back to the other slot",
            booted.name
        ));
    }
    if pending_not_confirmed(slots, primary) {
        let primary = primary.unwrap_or("(unknown)");
        return Some(format!(
            "slot {primary} is installed and activated but has not completed a \
             confirmed boot; this reboot boots into it and burns one of its boot attempts"
        ));
    }
    None
}

/// A manual rollback is refused because RAUC names no booted slot, so nothing
/// can be resolved relative to it: a container, dry-run, or an image whose
/// kernel command line carries no `rauc.slot=` (`uboot-ab-handshake.md` §5.3).
pub const ROLLBACK_NO_ALTERNATE_SLOT: &str = "no_alternate_slot";
/// A manual rollback is refused because the booted slot is the only member of
/// its slot class — a single-slot `system.conf`, where "the other slot" would
/// resolve to the slot already running.
pub const ROLLBACK_ALTERNATE_IS_BOOTED: &str = "alternate_is_booted_slot";
/// A manual rollback is refused because the alternate slot has never been
/// written: no bundle version and no install timestamp, so there is no system
/// there to fall back to.
pub const ROLLBACK_ALTERNATE_NEVER_INSTALLED: &str = "alternate_never_installed";
/// A manual rollback is refused because the alternate slot's boot-status is
/// `bad`: the bootloader has already condemned it.
pub const ROLLBACK_ALTERNATE_MARKED_BAD: &str = "alternate_marked_bad";
/// A manual rollback is refused because the alternate slot holds a system
/// installed MORE RECENTLY than the running one. That is not a rollback
/// target: it is a pending or skipped update, and switching the boot order to
/// it is "apply the untested thing" rather than "go back to the tested one".
///
/// This orders the two INSTALLS, and that the older one therefore booted is a
/// derivation, not a reading — see [`rollback_eligibility`] for the premise
/// it stands on.
pub const ROLLBACK_ALTERNATE_IS_NEWER: &str = "alternate_is_newer";
/// A manual rollback is refused because the two slots' install timestamps
/// cannot be ordered — one is absent, unparseable, or they are equal — so
/// nothing establishes that the alternate is the OLDER system. Fails closed:
/// see [`rollback_eligibility`] for why the unknown case refuses.
pub const ROLLBACK_INSTALL_ORDER_UNKNOWN: &str = "install_order_unknown";
/// A manual rollback is refused because the booted slot is itself
/// pending-not-confirmed. That window belongs to the automatic bad-slot path
/// — the bootloader's attempt counter — and a manual rollback taken inside it
/// races the boot credit it is already spending.
pub const ROLLBACK_BOOTED_NOT_CONFIRMED: &str = "booted_slot_not_confirmed";

/// Whether the device's slot state permits a manual rollback, which slot one
/// would boot into, and — when it does not — why.
///
/// INVARIANT, and the whole point of this type: a rollback is a `bad` mark on
/// the BOOTED slot. It never names the target in a mark and it can never emit
/// `good`, so no path through here confirms a slot no boot has verified —
/// PLAN-048's "cannot mark an unverified slot good". [`Self::mark`] is the
/// only mark this type produces, and its unit test enumerates the input space
/// rather than trusting the sentence above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackEligibility {
    /// The alternate slot a rollback would boot into, or `None` when the slot
    /// state names none.
    pub target: Option<String>,
    /// The stable refusal reason, `None` when the rollback is permitted.
    ///
    /// The only field a refusal is carried in: there is no separate
    /// `permitted` flag to disagree with it, so "permitted, and here is why
    /// not" is not a state this type can hold.
    pub reason: Option<&'static str>,
}

impl RollbackEligibility {
    /// Permitted exactly when nothing refused it.
    pub fn permitted(&self) -> bool {
        self.reason.is_none()
    }

    /// The mark a permitted rollback emits, or `None` when it is refused.
    ///
    /// `bad` on `booted`, always: condemning the slot we are running from is
    /// what makes the bootloader pick the other one (`uboot-ab-handshake.md`
    /// §5.3 walks `BOOT_ORDER` for a slot that still has credits). The
    /// vocabulary is [`validate_mark`]'s, unchanged — there is no second slot
    /// state machine here, and no way to express "mark the target good".
    pub fn mark(&self) -> Option<(&'static str, &'static str)> {
        self.permitted().then_some(("bad", "booted"))
    }

    /// The `rollback` object recorded under live-state `update`: `target`,
    /// `permitted` and a stable `reason`, with `null` for the two that are
    /// absent rather than a missing key — every consumer reads all three.
    pub fn to_json(&self) -> Value {
        json!({
            "target": self.target.as_ref().map_or(Value::Null, |name| Value::String(name.clone())),
            // Written as "there is a mark to emit" rather than as a flag of
            // its own, so the document an operator reads and the action they
            // then take cannot disagree about whether a rollback is offered.
            "permitted": self.mark().is_some(),
            "reason": self.reason.map_or(Value::Null, |reason| Value::String(reason.to_string())),
        })
    }

    fn refused(target: Option<&str>, reason: &'static str) -> Self {
        Self {
            target: target.map(str::to_string),
            reason: Some(reason),
        }
    }
}

/// A slot's class: the name up to the first `.`, which is how RAUC groups
/// slots (`rootfs.0`/`rootfs.1` are one group, `boot.0`/`boot.1` another).
fn slot_class(name: &str) -> &str {
    name.split_once('.').map_or(name, |(class, _)| class)
}

/// Is `target` the STRICTLY OLDER install of the two? `None` when the two
/// cannot be ordered at all.
///
/// RAUC writes `installed.timestamp` as an ISO-8601 instant when it installs
/// into a slot, so this is parsed rather than compared as text: a string
/// compare would silently order two different offsets wrongly.
///
/// `None` — and therefore a refusal — for every case that is not a proven
/// ordering: either timestamp absent, either unparseable, or the two equal.
/// Equal is not "old enough": two slots written in the same operation is the
/// shape of a factory flash, where the alternate has never booted.
fn older_install(target: &SlotStatus, booted: &SlotStatus) -> Option<bool> {
    let parse = |slot: &SlotStatus| {
        slot.installed_timestamp
            .as_deref()
            .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
    };
    let (target, booted) = (parse(target)?, parse(booted)?);
    (target != booted).then_some(target < booted)
}

/// Decide whether a manual rollback is permitted, and to which slot.
///
/// Pure over RAUC's own answers, so the entire decision is unit-testable with
/// no bus: the caller supplies the slot list and the primary slot name that
/// one [`query`] round already gathered.
///
/// The alternate is resolved the way RAUC resolves its own `other` — the
/// booted slot's class, minus the booted slot — so this decides *about* the
/// `booted`/`other` vocabulary [`validate_mark`] already owns rather than
/// introducing a second one. `system.conf` declares exactly two members of
/// the rootfs class (asserted on the built image by verify's
/// `rauc-rootfs-bootnames`), so at most one sibling can be found; the first
/// is taken rather than an ambiguity being invented for a shape the image
/// cannot have.
///
/// # What the install-order step checks, and the premise it stands on
///
/// The rule this enforces is "**a rollback goes backward**": the target must
/// be the strictly older of the two installs.
///
/// THE PREMISE, written down here because it is the only defence available:
/// this refusal derives `docs/design/recovery.md` §3 node 2's precondition —
/// "the other slot holds a system that booted successfully before" — from
/// RAUC's invariant that **an install never writes the running slot**. Given
/// that, a booted slot installed AFTER the target means the device was
/// running the target when that install happened, which is a successful boot
/// of the target. IF THAT INVARIANT EVER STOPS HOLDING — a future install
/// path able to target the booted slot, or an out-of-band flash that also
/// rewrites the status file's `installed.timestamp` — THE DERIVATION DOES
/// NOT, and the guard silently weakens. The invariant belongs to RAUC and to
/// the image pipeline, not to this tree, so nothing here goes red if it
/// changes: this paragraph is the warning, deliberately not a check.
///
/// HOW WELL THE PREMISE IS ESTABLISHED, so it is relied on with its evidence
/// visible. RAUC's target-selection code has been read at the pinned v1.13,
/// and the premise survives as a CONJUNCTION whose halves are both VERIFIED.
/// (1) RAUC only ever selects a slot it believes is inactive:
/// `select_inactive_slot_class_member`, reached from
/// `determine_target_install_group` (`src/install.c`), skips every slot whose
/// state is not `ST_INACTIVE`, and no install option, config key or D-Bus
/// argument can name a target instead. (2) Nothing here tells RAUC that the
/// wrong slot is booted: `install_bundle` below calls `InstallBundle` with the
/// bundle path and an empty options map, no target argument; the D-Bus install
/// API has no boot-slot or target key to pass; and the one lever that exists,
/// `--override-boot-slot`, is not on the install command in this build and
/// appears nowhere in this repository. This repository also recorded the
/// behaviour independently of this guard, for a different feature and before
/// it existed (`docs/design/updates.md`'s lifecycle table: the install task
/// "is writing the other slot", authored in 98379d18). It stays A PREMISE: it
/// is established at the pinned version, and a pin bump can move it.
/// `docs/design/recovery.md` §3 node 2 carries the reading, the function
/// names and the recipe for re-running it.
///
/// WHAT "APPEARS NOWHERE IN THIS REPOSITORY" DOES NOT MEAN, because a reader
/// can otherwise infer total absence from it. The string `override-boot-slot`
/// IS in the shipped `/usr/bin/rauc` — measured, once, in its help text.
/// `entries_install` compiles the option in only under `#if ENABLE_SERVICE ==
/// 0` and `pkgs/rauc/Dockerfile` builds `-Dservice=true`, so it is compiled out
/// of the INSTALL SUBCOMMAND, not out of the binary; it survives on
/// `entries_service`, the daemon's own argv. The sentence above is about this
/// repository's own text, and the honest statement about the image is the
/// narrower one: nothing on the device passes it.
///
/// AND THAT NARROWER STATEMENT IS NOW ASSERTED RATHER THAN MERELY WRITTEN
/// DOWN. `rauc-units-never-override-boot-slot` (`verify/src/checks-rauc-units.ts`)
/// reads every unit and drop-in under `/etc/systemd/system`,
/// `/usr/lib/systemd/system` and `/usr/local/lib/systemd/system` in the packed
/// root, folds continuation lines, and fails if any `Exec*=` command line that
/// starts rauc names the option — and fails, too, if it found no rauc command
/// line to look at. Half (1) above is still a premise a pin bump can move; this
/// half is a gate.
///
/// Why the property is derived rather than read: RAUC v1.13, the version
/// `pkgs/rauc/versions.env` pins, records no boot anywhere mosd can see, and
/// that was checked rather than assumed:
/// - `RaucSlotStatus` (`include/slot.h`) — everything the status file holds —
///   has no mark field at all: bundle metadata, `status`, checksum,
///   `installed.*` and `activated.*`, and nothing else.
/// - `r_mark_good` (`src/mark.c`) calls the bootloader backend and writes an
///   event-log line. It does NOT call `r_slot_status_save`, unlike
///   `r_mark_active` beside it, which does. A mark-good leaves no persisted
///   trace.
/// - `convert_slot_status_to_dict` (`src/service.c`) is the exhaustive
///   `GetSlotStatus` key list, and `boot-status` in it is computed live as
///   `slot->boot_good ? "good" : "bad"` — the attempt counter as a boolean,
///   the same limit [`pending_not_confirmed`] states.
/// - `status` (which [`SlotStatus`] already carries) is written only by the
///   installer, `pending` -> `update` -> `ok` (`src/install.c`). It records an
///   install, never a boot.
///
/// A direct confirmed-boot record would make the guard independent of the
/// premise above; it needs mosd to record its own, and is a separate design
/// rather than something to approximate here.
///
/// Everything that cannot be ordered refuses. Equal timestamps are the shape
/// of a factory flash that wrote both slots in one operation; an absent or
/// unparseable timestamp on EITHER slot proves nothing at all. This is a
/// brick-avoidance guard, so the unorderable case fails CLOSED — it refuses a
/// rollback it cannot justify rather than permitting one. The ordering is also
/// only as good as the clock at install time: a device that installed with a
/// wrong clock can record an order that did not happen.
pub fn rollback_eligibility(slots: &[SlotStatus], primary: Option<&str>) -> RollbackEligibility {
    let Some(booted) = booted_slot(slots) else {
        return RollbackEligibility::refused(None, ROLLBACK_NO_ALTERNATE_SLOT);
    };
    let class = slot_class(&booted.name);
    let target = slots
        .iter()
        .find(|slot| slot_class(&slot.name) == class && slot.name != booted.name);
    let Some(target) = target else {
        return RollbackEligibility::refused(None, ROLLBACK_ALTERNATE_IS_BOOTED);
    };
    // Never written: RAUC records a bundle version and an install timestamp
    // into a slot it has installed into, and the factory image writes only
    // slot A — so a device that has never taken an update has neither on the
    // other slot, and there is no system there to fall back to.
    if target.bundle_version.is_none() && target.installed_timestamp.is_none() {
        return RollbackEligibility::refused(
            Some(&target.name),
            ROLLBACK_ALTERNATE_NEVER_INSTALLED,
        );
    }
    if target.boot_status.as_deref() == Some("bad") {
        return RollbackEligibility::refused(Some(&target.name), ROLLBACK_ALTERNATE_MARKED_BAD);
    }
    // A rollback goes BACKWARD, and this is the step that enforces it.
    match older_install(target, booted) {
        Some(true) => {}
        Some(false) => {
            return RollbackEligibility::refused(Some(&target.name), ROLLBACK_ALTERNATE_IS_NEWER);
        }
        None => {
            return RollbackEligibility::refused(
                Some(&target.name),
                ROLLBACK_INSTALL_ORDER_UNKNOWN,
            );
        }
    }
    // Checked last because it is a fact about the slot we are LEAVING, not
    // the one we would arrive at: an operator reading a refusal wants to hear
    // about the target's own defects first.
    if pending_not_confirmed(slots, primary) {
        return RollbackEligibility::refused(Some(&target.name), ROLLBACK_BOOTED_NOT_CONFIRMED);
    }
    RollbackEligibility {
        target: Some(target.name.clone()),
        reason: None,
    }
}

/// The `update` object in the live-state tree, created empty on first use.
///
/// One accessor so every writer — the install task, the mark recorder, the
/// query merge — lands in the same place, and none of them can clobber the
/// whole entry while meaning to touch one key.
pub fn update_entry(state: &mut Value) -> &mut serde_json::Map<String, Value> {
    state
        .as_object_mut()
        .expect("live-state root is always an object")
        .entry("update")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .expect("update entry is only ever written as an object")
}

/// Validate an install request's bundle path: absolute, exists, and names a
/// regular file (after following symlinks — RAUC itself opens the target).
///
/// Absolute-only because the path crosses the bus and is opened by RAUC in
/// RAUC's working directory, not the caller's: a relative path would name a
/// file the caller has never seen. The error is a plain message for
/// `InvalidArgs`; it echoes the path, which the caller sent, and nothing else.
pub fn validate_bundle_path(raw: &str) -> Result<std::path::PathBuf, String> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(format!("bundle path must be absolute, got `{raw}`"));
    }
    let metadata = std::fs::metadata(path).map_err(|err| format!("bundle path `{raw}`: {err}"))?;
    if !metadata.is_file() {
        return Err(format!("bundle path `{raw}` is not a regular file"));
    }
    Ok(path.to_path_buf())
}

/// Validate a manual mark request BEFORE it reaches RAUC: `state` down to
/// `good`/`bad`, `slot` down to `booted`/`other`.
///
/// Narrower than RAUC on purpose. RAUC's `Mark` also accepts `active` (make a
/// slot the bootloader's first pick) and concrete slot names (`rootfs.1`);
/// neither is offered here. Activation is the installer's job — `InstallBundle`
/// activates what it installs — and a manual re-activation bypassing an
/// install is exactly the operation an operator should not reach by typo.
/// `booted`/`other` are RAUC's own relative identifiers and cover both slots
/// without letting a caller name one that does not exist.
pub fn validate_mark(state: &str, slot: &str) -> Result<(), String> {
    if !matches!(state, "good" | "bad") {
        return Err(format!("mark state must be `good` or `bad`, got `{state}`"));
    }
    if !matches!(slot, "booted" | "other") {
        return Err(format!(
            "mark slot must be `booted` or `other`, got `{slot}`"
        ));
    }
    Ok(())
}

/// Production client calling RAUC on the system bus.
///
/// The bus connection is created lazily inside each call, so constructing
/// this never touches the host.
#[derive(Default)]
pub struct Rauc {
    /// Explicit bus address; `None` selects the host system bus.
    address: Option<String>,
}

impl Rauc {
    /// Client talking to RAUC on the host system bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Client talking to RAUC on an explicit bus address.
    ///
    /// Test-only: it exists so the production call path can be driven against
    /// a fake RAUC on a private bus, never against the host.
    #[cfg(test)]
    pub fn at_address(address: &str) -> Self {
        Self {
            address: Some(address.to_string()),
        }
    }

    /// Proxy for RAUC's installer object, on a fresh lazy connection.
    async fn proxy(&self) -> Result<zbus::Proxy<'static>> {
        let connection = match &self.address {
            Some(address) => {
                zbus::connection::Builder::address(address.as_str())?
                    .build()
                    .await?
            }
            None => zbus::Connection::system().await?,
        };
        Ok(zbus::Proxy::new(&connection, RAUC_SERVICE, RAUC_PATH, RAUC_INTERFACE).await?)
    }
}

#[async_trait::async_trait]
impl RaucClient for Rauc {
    async fn install_bundle(&self, bundle: &Path) -> Result<()> {
        let proxy = self.proxy().await?;
        // Subscribe BEFORE asking for the install: RAUC's InstallBundle
        // returns as soon as the install has started, and completion arrives
        // only as the Completed signal. Subscribing after the call would race
        // a fast failure (missing keyring, unreadable bundle) and wait
        // forever on a signal that already fired.
        let mut completed = proxy.receive_signal(COMPLETED_SIGNAL).await?;
        let args: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        proxy
            .call_method("InstallBundle", &(bundle.to_string_lossy().as_ref(), args))
            .await
            .context("rauc InstallBundle")?;
        let signal = {
            let mut completed = pin!(&mut completed);
            poll_fn(|cx| completed.as_mut().poll_next(cx)).await
        };
        let Some(signal) = signal else {
            anyhow::bail!("bus connection closed before rauc reported completion");
        };
        let result: i32 = signal
            .body()
            .deserialize()
            .context("decode rauc Completed signal")?;
        if result == 0 {
            return Ok(());
        }
        // The signal carries only a status code; the human-readable reason
        // lives in LastError, so fetch it while the failure is fresh.
        let reason = self
            .last_error()
            .await
            .unwrap_or_else(|_| "(LastError unavailable)".to_string());
        anyhow::bail!("rauc install failed: {reason}")
    }

    async fn slot_status(&self) -> Result<Vec<SlotStatus>> {
        let proxy = self.proxy().await?;
        let reply = proxy
            .call_method("GetSlotStatus", &())
            .await
            .context("rauc GetSlotStatus")?;
        let raw: Vec<(String, HashMap<String, zbus::zvariant::OwnedValue>)> =
            reply.body().deserialize()?;
        Ok(raw
            .into_iter()
            .map(|(name, fields)| slot_from_fields(name, &fields))
            .collect())
    }

    async fn mark(&self, state: &str, slot: &str) -> Result<(String, String)> {
        let proxy = self.proxy().await?;
        let reply = proxy
            .call_method("Mark", &(state, slot))
            .await
            .context("rauc Mark")?;
        Ok(reply.body().deserialize()?)
    }

    async fn operation(&self) -> Result<String> {
        Ok(self.proxy().await?.get_property("Operation").await?)
    }

    async fn last_error(&self) -> Result<String> {
        Ok(self.proxy().await?.get_property("LastError").await?)
    }

    async fn progress(&self) -> Result<(i32, String, i32)> {
        Ok(self.proxy().await?.get_property("Progress").await?)
    }

    async fn primary(&self) -> Result<Option<String>> {
        let proxy = self.proxy().await?;
        // RAUC answers GetPrimary with a D-Bus error BOTH when it simply has
        // no primary and when something is genuinely wrong; the two are not
        // distinguishable from the error alone. The slot-status query that
        // accompanies every use of this is the connectivity probe, so "no
        // answer" is reported as "no primary" rather than failing the whole
        // state read.
        match proxy.call_method("GetPrimary", &()).await {
            Ok(reply) => Ok(Some(reply.body().deserialize()?)),
            Err(err) => {
                tracing::debug!(error = %err, "rauc GetPrimary answered with an error; treating as no primary");
                Ok(None)
            }
        }
    }
}

/// Curate RAUC's per-slot `a{sv}` dictionary into a [`SlotStatus`].
fn slot_from_fields(
    name: String,
    fields: &HashMap<String, zbus::zvariant::OwnedValue>,
) -> SlotStatus {
    let get = |key: &str| -> Option<String> {
        fields
            .get(key)
            .and_then(|value| value.downcast_ref::<&str>().ok())
            .map(str::to_string)
    };
    SlotStatus {
        state: get("state"),
        bootname: get("bootname"),
        boot_status: get("boot-status"),
        bundle_version: get("bundle.version"),
        installed_timestamp: get("installed.timestamp"),
        status: get("status"),
        name,
    }
}

/// No-op [`RaucClient`] installed in dry-run mode.
///
/// Dry-run is what the integration tests run the daemon under, so this is the
/// reason no test can install a bundle on the build host: the production
/// [`Rauc`] client is never constructed there. It reports the shape of a
/// system with no update in flight — idle, no slots, no primary — so the
/// recorded state stays parseable by the same consumers.
pub struct DryRunRauc;

#[async_trait::async_trait]
impl RaucClient for DryRunRauc {
    async fn install_bundle(&self, bundle: &Path) -> Result<()> {
        tracing::info!(bundle = %bundle.display(), "dry run: install request not forwarded to rauc");
        Ok(())
    }

    async fn slot_status(&self) -> Result<Vec<SlotStatus>> {
        Ok(Vec::new())
    }

    async fn mark(&self, state: &str, slot: &str) -> Result<(String, String)> {
        tracing::info!(state, slot, "dry run: mark request not forwarded to rauc");
        Ok((
            "(dry-run)".to_string(),
            format!("dry run: would mark {slot} as {state}"),
        ))
    }

    async fn operation(&self) -> Result<String> {
        Ok("idle".to_string())
    }

    async fn last_error(&self) -> Result<String> {
        Ok(String::new())
    }

    async fn progress(&self) -> Result<(i32, String, i32)> {
        Ok((0, String::new(), 0))
    }

    async fn primary(&self) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Configurable recording [`RaucClient`] for the bus-layer unit tests.
#[cfg(test)]
pub struct MockRauc {
    /// Shared with the test so the log stays readable after the mock is
    /// moved into the service.
    pub calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Answer for [`RaucClient::slot_status`].
    pub slots: Vec<SlotStatus>,
    /// Answer for [`RaucClient::primary`].
    pub primary: Option<String>,
    /// When `Some`, [`RaucClient::install_bundle`] fails with this message.
    pub install_error: Option<String>,
    /// When `Some`, an install blocks until the test calls `notify_one` —
    /// how "a second install while one runs" becomes a deterministic state
    /// instead of a race.
    pub install_gate: Option<std::sync::Arc<tokio::sync::Notify>>,
    /// When true, every query method fails — the shape of a host whose RAUC
    /// is absent or wedged.
    pub queries_fail: bool,
}

#[cfg(test)]
impl Default for MockRauc {
    fn default() -> Self {
        Self {
            calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            slots: Vec::new(),
            primary: None,
            install_error: None,
            install_gate: None,
            queries_fail: false,
        }
    }
}

#[cfg(test)]
impl MockRauc {
    fn record(&self, call: String) {
        self.calls.lock().expect("mock lock").push(call);
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl RaucClient for MockRauc {
    async fn install_bundle(&self, bundle: &Path) -> Result<()> {
        self.record(format!("install {}", bundle.display()));
        if let Some(gate) = &self.install_gate {
            gate.notified().await;
        }
        match &self.install_error {
            Some(message) => Err(anyhow::anyhow!("{message}")),
            None => Ok(()),
        }
    }

    async fn slot_status(&self) -> Result<Vec<SlotStatus>> {
        if self.queries_fail {
            anyhow::bail!("mock rauc: unreachable");
        }
        Ok(self.slots.clone())
    }

    async fn mark(&self, state: &str, slot: &str) -> Result<(String, String)> {
        self.record(format!("mark {state} {slot}"));
        if self.queries_fail {
            anyhow::bail!("mock rauc: unreachable");
        }
        Ok(("rootfs.9".to_string(), format!("marked {slot} as {state}")))
    }

    async fn operation(&self) -> Result<String> {
        if self.queries_fail {
            anyhow::bail!("mock rauc: unreachable");
        }
        Ok("idle".to_string())
    }

    async fn last_error(&self) -> Result<String> {
        if self.queries_fail {
            anyhow::bail!("mock rauc: unreachable");
        }
        Ok(String::new())
    }

    async fn progress(&self) -> Result<(i32, String, i32)> {
        if self.queries_fail {
            anyhow::bail!("mock rauc: unreachable");
        }
        Ok((0, String::new(), 0))
    }

    async fn primary(&self) -> Result<Option<String>> {
        if self.queries_fail {
            anyhow::bail!("mock rauc: unreachable");
        }
        Ok(self.primary.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};

    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::{OwnedValue, Value};

    use super::*;

    fn slot(name: &str, state: &str, boot_status: Option<&str>) -> SlotStatus {
        SlotStatus {
            name: name.to_string(),
            state: Some(state.to_string()),
            boot_status: boot_status.map(str::to_string),
            ..SlotStatus::default()
        }
    }

    #[test]
    fn a_converged_system_is_not_pending_and_warns_about_nothing() {
        let slots = [
            slot("rootfs.0", "booted", Some("good")),
            slot("rootfs.1", "inactive", Some("good")),
        ];
        assert!(!pending_not_confirmed(&slots, Some("rootfs.0")));
        assert_eq!(unconfirmed_slot_warning(&slots, Some("rootfs.0")), None);
    }

    #[test]
    fn a_freshly_installed_other_slot_is_pending_and_warned_about() {
        let slots = [
            slot("rootfs.0", "booted", Some("good")),
            slot("rootfs.1", "inactive", Some("good")),
        ];
        assert!(pending_not_confirmed(&slots, Some("rootfs.1")));
        let warning =
            unconfirmed_slot_warning(&slots, Some("rootfs.1")).expect("pending must warn");
        assert!(warning.contains("rootfs.1"), "warning was: {warning}");
        assert!(warning.contains("boot attempts"), "warning was: {warning}");
    }

    #[test]
    fn an_exhausted_booted_slot_outranks_the_pending_warning() {
        let slots = [
            slot("rootfs.0", "booted", Some("bad")),
            slot("rootfs.1", "inactive", Some("good")),
        ];
        let warning =
            unconfirmed_slot_warning(&slots, Some("rootfs.1")).expect("bad boot-status must warn");
        assert!(warning.contains("boot-status `bad`"), "warning: {warning}");
        assert!(warning.contains("rootfs.0"), "warning: {warning}");
    }

    #[test]
    fn no_slots_or_no_primary_means_no_verdict_at_all() {
        // A v1 image, a container, or dry-run: nothing to warn about, and
        // crucially not an error — a reboot must still go through.
        assert!(!pending_not_confirmed(&[], None));
        assert_eq!(unconfirmed_slot_warning(&[], None), None);
        let slots = [slot("rootfs.0", "booted", Some("good"))];
        assert!(!pending_not_confirmed(&slots, None));
        assert_eq!(unconfirmed_slot_warning(&slots, None), None);
    }

    /// A slot that has been written: RAUC records both of these into a slot
    /// it installed into. `stamp` is the install instant, which the
    /// backward-only step orders the two slots by.
    fn installed_at(name: &str, state: &str, boot_status: Option<&str>, stamp: &str) -> SlotStatus {
        SlotStatus {
            bundle_version: Some("2026.08".to_string()),
            installed_timestamp: Some(stamp.to_string()),
            ..slot(name, state, boot_status)
        }
    }

    /// The ordinary post-update shape: the running slot was installed AFTER
    /// the one we would roll back to, which is the only ordering the
    /// backward-only step permits.
    const OLDER: &str = "2026-08-01T10:00:00Z";
    const NEWER: &str = "2026-08-30T10:00:00Z";

    #[test]
    fn a_converged_two_slot_system_permits_a_rollback_to_the_other_slot() {
        let slots = [
            installed_at("rootfs.0", "booted", Some("good"), NEWER),
            installed_at("rootfs.1", "inactive", Some("good"), OLDER),
        ];
        let decision = rollback_eligibility(&slots, Some("rootfs.0"));
        assert_eq!(decision.target.as_deref(), Some("rootfs.1"));
        assert!(decision.permitted());
        assert_eq!(decision.reason, None);
        assert_eq!(decision.mark(), Some(("bad", "booted")));
    }

    #[test]
    fn without_a_booted_slot_there_is_no_alternate_to_resolve() {
        // Dry-run, a container, or an image booted without `rauc.slot=`.
        for slots in [
            Vec::new(),
            vec![
                installed_at("rootfs.0", "inactive", Some("good"), NEWER),
                installed_at("rootfs.1", "inactive", Some("good"), OLDER),
            ],
        ] {
            let decision = rollback_eligibility(&slots, None);
            assert_eq!(decision.target, None);
            assert!(!decision.permitted());
            assert_eq!(decision.reason, Some(ROLLBACK_NO_ALTERNATE_SLOT));
            assert_eq!(decision.mark(), None);
        }
    }

    #[test]
    fn a_single_slot_class_refuses_because_the_alternate_would_be_the_booted_slot() {
        // A `system.conf` with one rootfs slot: the boot slots are a class of
        // their own and must not be offered as a rollback target.
        let slots = [
            installed_at("rootfs.0", "booted", Some("good"), NEWER),
            installed_at("boot.0", "inactive", Some("good"), OLDER),
            installed_at("boot.1", "inactive", Some("good"), OLDER),
        ];
        let decision = rollback_eligibility(&slots, Some("rootfs.0"));
        assert_eq!(decision.target, None);
        assert_eq!(decision.reason, Some(ROLLBACK_ALTERNATE_IS_BOOTED));
        assert_eq!(decision.mark(), None);
    }

    #[test]
    fn a_never_written_alternate_is_not_a_rollback_target() {
        // The shape of a device that has never taken an update: the factory
        // image wrote slot A and slot B holds nothing.
        let slots = [
            installed_at("rootfs.0", "booted", Some("good"), NEWER),
            slot("rootfs.1", "inactive", Some("good")),
        ];
        let decision = rollback_eligibility(&slots, Some("rootfs.0"));
        assert_eq!(decision.target.as_deref(), Some("rootfs.1"));
        assert!(!decision.permitted());
        assert_eq!(decision.reason, Some(ROLLBACK_ALTERNATE_NEVER_INSTALLED));
        assert_eq!(decision.mark(), None);
    }

    #[test]
    fn an_alternate_the_bootloader_condemned_is_not_a_rollback_target() {
        let slots = [
            installed_at("rootfs.0", "booted", Some("good"), NEWER),
            installed_at("rootfs.1", "inactive", Some("bad"), OLDER),
        ];
        let decision = rollback_eligibility(&slots, Some("rootfs.0"));
        assert_eq!(decision.target.as_deref(), Some("rootfs.1"));
        assert_eq!(decision.reason, Some(ROLLBACK_ALTERNATE_MARKED_BAD));
        assert_eq!(decision.mark(), None);
    }

    #[test]
    fn an_unconfirmed_booted_slot_leaves_the_rollback_to_the_bootloader() {
        // Installed and activated but not yet booted: `primary` is the other
        // slot. A manual rollback here races the boot credit the automatic
        // path is already counting down.
        let slots = [
            installed_at("rootfs.0", "booted", Some("good"), NEWER),
            installed_at("rootfs.1", "inactive", Some("good"), OLDER),
        ];
        let decision = rollback_eligibility(&slots, Some("rootfs.1"));
        assert!(pending_not_confirmed(&slots, Some("rootfs.1")));
        assert_eq!(decision.target.as_deref(), Some("rootfs.1"));
        assert_eq!(decision.reason, Some(ROLLBACK_BOOTED_NOT_CONFIRMED));
        assert_eq!(decision.mark(), None);
    }

    #[test]
    fn a_newer_alternate_is_a_pending_update_not_a_rollback_target() {
        // The running system is the OLDER install and the other slot holds a
        // more recent one: switching to it applies an untested system rather
        // than returning to a tested one.
        let slots = [
            installed_at("rootfs.0", "booted", Some("good"), OLDER),
            installed_at("rootfs.1", "inactive", Some("good"), NEWER),
        ];
        let decision = rollback_eligibility(&slots, Some("rootfs.0"));
        assert_eq!(decision.target.as_deref(), Some("rootfs.1"));
        assert!(!decision.permitted());
        assert_eq!(decision.reason, Some(ROLLBACK_ALTERNATE_IS_NEWER));
        assert_eq!(decision.mark(), None);
    }

    #[test]
    fn an_unorderable_pair_of_installs_fails_closed() {
        // Every shape that does not PROVE the target is the older system.
        // Each is a real slot state, not a contrived one: equal stamps are a
        // factory flash that wrote both slots at once, a missing stamp is a
        // slot whose status file carries a version and no instant, and an
        // unparseable one is a status file written by something else.
        let cases: [(SlotStatus, SlotStatus); 4] = [
            (
                installed_at("rootfs.0", "booted", Some("good"), NEWER),
                installed_at("rootfs.1", "inactive", Some("good"), NEWER),
            ),
            (
                installed_at("rootfs.0", "booted", Some("good"), NEWER),
                SlotStatus {
                    bundle_version: Some("2026.07".to_string()),
                    ..slot("rootfs.1", "inactive", Some("good"))
                },
            ),
            (
                SlotStatus {
                    bundle_version: Some("2026.08".to_string()),
                    ..slot("rootfs.0", "booted", Some("good"))
                },
                installed_at("rootfs.1", "inactive", Some("good"), OLDER),
            ),
            (
                installed_at("rootfs.0", "booted", Some("good"), NEWER),
                installed_at("rootfs.1", "inactive", Some("good"), "yesterday"),
            ),
        ];
        for (booted, target) in cases {
            let label = format!(
                "{:?} / {:?}",
                booted.installed_timestamp, target.installed_timestamp
            );
            let slots = [booted, target];
            let decision = rollback_eligibility(&slots, Some("rootfs.0"));
            assert!(!decision.permitted(), "{label} must not permit a rollback");
            assert_eq!(
                decision.reason,
                Some(ROLLBACK_INSTALL_ORDER_UNKNOWN),
                "{label}"
            );
            assert_eq!(decision.mark(), None, "{label}");
        }
    }

    /// PLAN-048's clause, as a guarantee rather than an intention: across the
    /// whole two-slot input space, the mark a rollback emits is `bad` on
    /// `booted` or nothing at all. It never says `good`, and it never names
    /// the target slot — so no input reaches "mark the unverified slot good".
    #[test]
    fn no_input_lets_a_rollback_mark_the_target_good() {
        let states = ["booted", "active", "inactive"];
        let boot_statuses = [None, Some("good"), Some("bad")];
        let versions = [None, Some("2026.08")];
        // Absent, older, newer and unparseable — varied PER SLOT, so the
        // enumeration covers every ordering the backward-only step decides:
        // older, newer, equal, and not orderable at all.
        let stamps = [None, Some(OLDER), Some(NEWER), Some("yesterday")];
        let primaries = [None, Some("rootfs.0"), Some("rootfs.1")];

        let mut permitted_seen = 0_usize;
        let mut refused_seen = 0_usize;
        let mut reasons_seen = std::collections::BTreeSet::new();
        for a_state in states {
            for b_state in states {
                for a_boot in boot_statuses {
                    for b_boot in boot_statuses {
                        for a_version in versions {
                            for b_version in versions {
                                for a_stamp in stamps {
                                    for b_stamp in stamps {
                                        for primary in primaries {
                                            let slots = [
                                                SlotStatus {
                                                    bundle_version: a_version.map(str::to_string),
                                                    installed_timestamp: a_stamp
                                                        .map(str::to_string),
                                                    ..slot("rootfs.0", a_state, a_boot)
                                                },
                                                SlotStatus {
                                                    bundle_version: b_version.map(str::to_string),
                                                    installed_timestamp: b_stamp
                                                        .map(str::to_string),
                                                    ..slot("rootfs.1", b_state, b_boot)
                                                },
                                            ];
                                            let decision = rollback_eligibility(&slots, primary);
                                            if let Some(reason) = decision.reason {
                                                reasons_seen.insert(reason);
                                            }
                                            match decision.mark() {
                                                None => {
                                                    refused_seen += 1;
                                                    assert!(!decision.permitted());
                                                    assert!(decision.reason.is_some());
                                                }
                                                Some(mark) => {
                                                    permitted_seen += 1;
                                                    assert_eq!(
                                                        mark,
                                                        ("bad", "booted"),
                                                        "the only mark a rollback may emit"
                                                    );
                                                    assert_eq!(
                                                        validate_mark(mark.0, mark.1),
                                                        Ok(()),
                                                        "the mark stays inside the offered vocabulary"
                                                    );
                                                    assert_ne!(mark.0, "good");
                                                    assert_ne!(
                                                        Some(mark.1),
                                                        decision.target.as_deref(),
                                                        "a rollback never marks the slot it rolls back TO"
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // The enumeration must actually reach both verdicts; an input space
        // that only ever refuses would pass the assertions above without
        // having tested the permitted path at all.
        assert!(permitted_seen > 0, "no input reached a permitted rollback");
        assert!(refused_seen > 0, "no input reached a refusal");
        // And it must reach every refusal a two-slot rootfs class can produce
        // — otherwise a reason could be deleted from the predicate and this
        // test would still pass. `alternate_is_booted_slot` is the one
        // exception: it needs a class with a single member, which this
        // enumeration never builds, and it has its own test above.
        assert_eq!(
            reasons_seen.into_iter().collect::<Vec<_>>(),
            vec![
                ROLLBACK_ALTERNATE_IS_NEWER,
                ROLLBACK_ALTERNATE_MARKED_BAD,
                ROLLBACK_ALTERNATE_NEVER_INSTALLED,
                ROLLBACK_BOOTED_NOT_CONFIRMED,
                ROLLBACK_INSTALL_ORDER_UNKNOWN,
                ROLLBACK_NO_ALTERNATE_SLOT,
            ],
            "every refusal this input space can reach must actually be reached"
        );
    }

    #[test]
    fn every_offered_mark_is_accepted_and_nothing_else_is() {
        for state in ["good", "bad"] {
            for slot in ["booted", "other"] {
                assert_eq!(validate_mark(state, slot), Ok(()), "{state} {slot}");
            }
        }
        // RAUC accepts all three of these; this surface deliberately does not.
        assert!(validate_mark("active", "other").is_err());
        assert!(validate_mark("good", "rootfs.0").is_err());
        assert!(validate_mark("Good", "booted").is_err(), "no case folding");
    }

    #[test]
    fn a_bundle_path_must_be_an_absolute_existing_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("ok.raucb");
        std::fs::write(&bundle, b"bytes").expect("seed");

        let ok = validate_bundle_path(bundle.to_str().expect("utf-8")).expect("valid");
        assert_eq!(ok, bundle);

        assert!(validate_bundle_path("ok.raucb").is_err(), "relative");
        assert!(
            validate_bundle_path(dir.path().join("gone.raucb").to_str().expect("utf-8")).is_err(),
            "missing"
        );
        assert!(
            validate_bundle_path(dir.path().to_str().expect("utf-8")).is_err(),
            "directory"
        );
    }

    #[test]
    fn the_update_entry_is_created_once_and_preserves_siblings() {
        let mut state = json!({});
        update_entry(&mut state).insert("install".into(), json!({"status": "running"}));
        // A later accessor sees the same object rather than a fresh one.
        update_entry(&mut state).insert("last_error".into(), json!(""));
        assert_eq!(state["update"]["install"]["status"], "running");
        assert_eq!(state["update"]["last_error"], "");
    }

    #[test]
    fn slot_json_omits_absent_fields_instead_of_writing_null() {
        let json = slot("rootfs.1", "inactive", None).to_json();
        let map = json.as_object().expect("object");
        assert_eq!(map.get("state"), Some(&serde_json::json!("inactive")));
        assert!(
            !map.contains_key("boot_status"),
            "an absent field must be a missing key, got {json}"
        );
    }

    /// Kills the wrapped child on drop, including on panic.
    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Locate `dbus-daemon`: `/usr/bin/dbus-daemon` first, then `$PATH`.
    fn find_dbus_daemon() -> Option<PathBuf> {
        let fixed = PathBuf::from("/usr/bin/dbus-daemon");
        if fixed.exists() {
            return Some(fixed);
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("dbus-daemon"))
            .find(|candidate| candidate.exists())
    }

    fn str_value(value: &str) -> OwnedValue {
        OwnedValue::try_from(Value::from(value)).expect("string OwnedValue")
    }

    /// Stand-in for RAUC on a private bus, recording the calls it receives.
    ///
    /// The member names, property names and the Completed signal are spelled
    /// out rather than derived, so this fake cannot silently agree with a
    /// typo in the production constants.
    struct FakeRauc {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "de.pengutronix.rauc.Installer")]
    impl FakeRauc {
        #[zbus(name = "InstallBundle")]
        async fn install_bundle(
            &self,
            #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
            source: String,
            _args: HashMap<String, OwnedValue>,
        ) {
            self.calls
                .lock()
                .expect("fake lock")
                .push(format!("InstallBundle {source}"));
            // Real RAUC returns from InstallBundle immediately and reports
            // the outcome via Completed; the fake mirrors that split.
            let result = i32::from(source.ends_with("bad.raucb"));
            let _ = Self::completed(&emitter, result).await;
        }

        #[zbus(name = "GetSlotStatus")]
        async fn get_slot_status(&self) -> Vec<(String, HashMap<String, OwnedValue>)> {
            vec![
                (
                    "rootfs.0".to_string(),
                    HashMap::from([
                        ("state".to_string(), str_value("booted")),
                        ("bootname".to_string(), str_value("A")),
                        ("boot-status".to_string(), str_value("good")),
                        ("bundle.version".to_string(), str_value("2026.08")),
                        // A non-string field the curation must tolerate.
                        (
                            "installed.count".to_string(),
                            OwnedValue::try_from(Value::from(3u32)).expect("u32"),
                        ),
                    ]),
                ),
                (
                    "rootfs.1".to_string(),
                    HashMap::from([("state".to_string(), str_value("inactive"))]),
                ),
            ]
        }

        #[zbus(name = "GetPrimary")]
        async fn get_primary(&self) -> String {
            "rootfs.1".to_string()
        }

        #[zbus(name = "Mark")]
        async fn mark(&self, state: String, slot: String) -> (String, String) {
            self.calls
                .lock()
                .expect("fake lock")
                .push(format!("Mark {state} {slot}"));
            ("rootfs.0".to_string(), format!("marked {slot} as {state}"))
        }

        #[zbus(property)]
        async fn operation(&self) -> String {
            "idle".to_string()
        }

        #[zbus(property)]
        async fn last_error(&self) -> String {
            "signature verification failed".to_string()
        }

        #[zbus(property)]
        async fn progress(&self) -> (i32, String, i32) {
            (100, "Installing done.".to_string(), 1)
        }

        #[zbus(signal)]
        async fn completed(emitter: &SignalEmitter<'_>, result: i32) -> zbus::Result<()>;
    }

    /// Drives the PRODUCTION [`Rauc`] call path against a fake RAUC on a
    /// private session bus. A wrong service name, object path, interface,
    /// member, property or signal name fails to dispatch and this test goes
    /// red — which a mock handed the same wrong name would not.
    #[tokio::test(flavor = "multi_thread")]
    async fn production_path_dispatches_to_the_real_rauc_names() -> Result<()> {
        let Some(dbus_daemon) = find_dbus_daemon() else {
            eprintln!(
                "skipping production_path_dispatches_to_the_real_rauc_names: dbus-daemon not found"
            );
            return Ok(());
        };

        // Private session bus; never the host system bus.
        let mut bus_child = Command::new(dbus_daemon)
            .args(["--session", "--print-address=1", "--nofork"])
            .stdout(Stdio::piped())
            .spawn()?;
        let bus_stdout = bus_child.stdout.take().expect("piped stdout");
        let _bus_guard = ChildGuard(bus_child);
        let mut address = String::new();
        BufReader::new(bus_stdout).read_line(&mut address)?;
        let address = address.trim().to_string();
        anyhow::ensure!(!address.is_empty(), "dbus-daemon printed no address");

        let calls = Arc::new(Mutex::new(Vec::new()));
        let _server = zbus::connection::Builder::address(address.as_str())?
            .name(RAUC_SERVICE)?
            .serve_at(
                RAUC_PATH,
                FakeRauc {
                    calls: Arc::clone(&calls),
                },
            )?
            .build()
            .await?;

        let rauc = Rauc::at_address(&address);

        let slots = rauc.slot_status().await?;
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].name, "rootfs.0");
        assert_eq!(slots[0].state.as_deref(), Some("booted"));
        assert_eq!(slots[0].boot_status.as_deref(), Some("good"));
        assert_eq!(slots[0].bundle_version.as_deref(), Some("2026.08"));
        assert_eq!(slots[1].state.as_deref(), Some("inactive"));
        assert_eq!(slots[1].boot_status, None);

        assert_eq!(rauc.primary().await?, Some("rootfs.1".to_string()));
        assert_eq!(rauc.operation().await?, "idle");
        assert_eq!(rauc.progress().await?.0, 100);

        let (slot_name, message) = rauc.mark("good", "booted").await?;
        assert_eq!(slot_name, "rootfs.0");
        assert!(message.contains("good"), "message: {message}");

        rauc.install_bundle(Path::new("/data/ok.raucb")).await?;
        let err = rauc
            .install_bundle(Path::new("/data/bad.raucb"))
            .await
            .expect_err("Completed(1) must surface as an error");
        assert!(
            err.to_string().contains("signature verification failed"),
            "the LastError text must reach the caller, got: {err}"
        );

        assert_eq!(
            *calls.lock().expect("lock"),
            vec![
                "Mark good booted".to_string(),
                "InstallBundle /data/ok.raucb".to_string(),
                "InstallBundle /data/bad.raucb".to_string(),
            ]
        );
        Ok(())
    }
}

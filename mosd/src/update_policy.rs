//! Update policy: the operator document, the baked defaults it overrides,
//! maintenance windows, metered/offline mode, the auto-check cadence and the
//! safe-to-reboot gate.
//!
//! # Three layers, one precedence rule (PLAN-070 §5.1)
//!
//! 1. **The baked manifest**, [`crate::baked_meta`]: fleet-identical and
//!    per-build. It owns the trust anchors and carries *defaults* for the
//!    source URL, the channel, the policy and the check interval.
//! 2. **`/mos/config/updates.json`**, on DATA: operator-owned, and the only
//!    place any of those four is overridden. It also *owns* the keys layer 1
//!    never carries -- the windows, the network mode, the workspace paths and
//!    the reboot-gate keys -- which fall back to the code defaults here.
//! 3. **The running state**, which configures nothing and lives in
//!    [`crate::update_lifecycle`].
//!
//! Per key: layer 2 wins where it speaks, layer 1 where it does not. The
//! baked value is a **default**, not a fallback and not a floor -- it is
//! consulted when layer 2 is silent about the key and at no other moment. An
//! overridden source that does not answer reports the failure; it does not
//! revert to the baked address, because a device that quietly re-pointed
//! itself at the vendor's server would be talking to a host nobody selected.
//!
//! # The four load outcomes, and the one that has to be right
//!
//! - **Absent** -> the baked defaults. A device that has never been configured
//!   follows what it shipped with.
//! - **Malformed** -> fail closed on the *actions*, fail open on the *device*,
//!   and **never** silently adopt the baked channel. Every capability the
//!   document gates is refused with a message naming the file; the reboot
//!   gate keeps evaluating with the code defaults, because an unreadable file
//!   must not brick the reboot button. This is the case worth the care: a
//!   parse error is not absence, and treating it as absence would put a
//!   device on a channel its operator did not choose. It is enforced by shape
//!   rather than by discipline -- [`EffectivePolicy::unknown_selection`] takes
//!   no arguments, so there is no version of that path that could reach the
//!   baked layer.
//! - **An anchor-shaped key** -> a load error naming the key. The schema has
//!   no `trust` object at all, and `trust`, `signingKeys`, `signingKeyId(s)`,
//!   `rootPath` and `keyring` are refused **by name**, at any depth, rather
//!   than only by `deny_unknown_fields`: the whole safety of a changeable
//!   address is that the signature check is unchangeable (PLAN-070 §5.3.5),
//!   and a generic refusal would evaporate the day somebody widens the schema
//!   for a benign reason.
//! - **Anything else unknown** -> also a load error, for the settings tree's
//!   reason: a mistyped key must fail loudly, not silently configure nothing.
//!
//! The file is read fresh on every policy decision -- a handful of bytes per
//! action is cheaper than a watch, and an operator edit takes effect on the
//! next decision with no restart and no reload verb.
//!
//! What this module does not do: hold state. The reboot-gate override and the
//! lifecycle machine live in [`crate::update_lifecycle`]; everything here is
//! a pure function of the two documents, the clock and the health tree, so
//! every rule is unit-testable without a daemon.

use std::path::PathBuf;

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::baked_meta::{BakedUpdate, UpdateMode};

/// Where the operator document lives (PLAN-070 §5.1): the update subsystem's
/// occupant of the `/mos/config/` namespace, on the DATA pool that also backs
/// the `/mos/updates` workspace, so one readiness probe gates both.
pub const DEFAULT_POLICY_PATH: &str = "/mos/config/updates.json";

/// The document's schema tag (PLAN-071 §1). Optional -- a key the document
/// does not name is a key that takes its default -- but checked when present,
/// so a `fleet.json` poured into this path is refused rather than read.
pub const SCHEMA_TAG: &str = "mos/update-config/v1";

/// Longest administrative reboot-gate override a policy may allow, and the
/// built-in default. One hour: long enough to carry a maintenance action,
/// short enough that a forgotten override does not stand disarmed for a week.
pub const OVERRIDE_CEILING_SECONDS: u64 = 3600;

/// The pinned trusted root passed to `rauc-update --root`.
///
/// **No longer an operator key.** `source.rootPath` was the anchor half of
/// the old `[source]` block and PLAN-070 §5.3.5 keeps it retired while the
/// URL beside it became overridable: the address is the operator's, what the
/// device will accept is not. The value is a build-side constant until F7
/// replaces the flag with the baked manifest's `trust.signingKeys`, which is
/// where the anchor now lives; nothing provisions a file here today.
pub const DEFAULT_ROOT_PATH: &str = "/usr/share/mos/uptane/root.json";

/// Key names that would move a trust anchor into an operator document.
///
/// Refused by name, at any depth, rather than left to `deny_unknown_fields`.
/// `signingKeyIds` is here as well as the singular the plan names, because
/// the plural is what the baked manifest actually calls the field and a list
/// that missed it would have a hole exactly where the sibling document has a
/// key. Widening the schema to admit any of these is not a smaller version of
/// the overridable-address decision; it is the deletion of its premise.
const ANCHOR_KEYS: [&str; 6] = [
    "trust",
    "signingKeys",
    "signingKeyId",
    "signingKeyIds",
    "rootPath",
    "keyring",
];

/// The operator document, exactly as parsed -- layer 2, and nothing resolved.
///
/// Every key layer 1 also carries is an `Option` so that *absent* is
/// distinguishable from *set to the same value the default happens to have*;
/// the keys layer 1 never carries take the code defaults here.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OperatorDocument {
    /// Checked against [`SCHEMA_TAG`] when present.
    #[serde(default)]
    pub schema: Option<String>,
    /// What the device does on its own. Overrides `update.policy`.
    #[serde(default)]
    pub policy: Option<UpdateMode>,
    /// Minutes between automatic checks; `0` disables them. Overrides
    /// `update.checkIntervalMinutes`.
    #[serde(default)]
    pub check_interval_minutes: Option<u64>,
    #[serde(default)]
    pub source: OperatorSource,
    #[serde(default)]
    pub network: NetworkPolicy,
    #[serde(default)]
    pub maintenance: MaintenancePolicy,
    #[serde(default)]
    pub reboot_gate: RebootGatePolicy,
}

/// Where updates come from and how much of the `/mos/updates` workspace they
/// may hold.
///
/// `url` and `channel` override the baked defaults; the three workspace
/// values are layer 2's own and default to the deployment contract
/// `docs/design/updates.md` records. Where bundles are staged is NOT a policy
/// knob: the client's workspace is `/mos/updates` and nothing else
/// (PLAN-061/063), so there is no key that could point it elsewhere.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OperatorSource {
    /// Base URL of the published repository. Absent = the baked default,
    /// which may itself be absent -- no online source, so `check`/`fetch` are
    /// refused and the offline import path remains.
    #[serde(default)]
    pub url: Option<String>,
    /// Release channel to follow (`rauc-update check --channel`). Absent =
    /// the baked default.
    #[serde(default)]
    pub channel: Option<String>,
    /// Local metadata mirror directory (`rauc-update --repo`).
    #[serde(default = "default_repo_dir")]
    pub repo_dir: String,
    /// Persistent rollback state (`rauc-update --state`).
    #[serde(default = "default_state_path")]
    pub state_path: String,
    /// Byte budget for the workspace -- downloads/, verified/ and staging/
    /// together (`rauc-update --max-bytes`); readiness also requires the
    /// DATA pool to back what is unspent of it.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
}

fn default_repo_dir() -> String {
    "/var/lib/mos/update/tuf-mirror".to_string()
}
fn default_state_path() -> String {
    "/var/lib/mos/update/uptane-state.json".to_string()
}
fn default_max_bytes() -> u64 {
    // Half a gigabyte: comfortably one compressed rootfs bundle, a small
    // share of the growable DATA pool the workspace lives on. The operator
    // raises it deliberately for larger bundles; PLAN-049 owns the quota.
    500_000_000
}

impl Default for OperatorSource {
    fn default() -> Self {
        Self {
            url: None,
            channel: None,
            repo_dir: default_repo_dir(),
            state_path: default_state_path(),
            max_bytes: default_max_bytes(),
        }
    }
}

/// How the device's connectivity is classed. Declared by the operator, not
/// detected: mosd has no metering signal to read, and a policy that guessed
/// would be wrong in exactly the deployments that care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NetworkMode {
    /// Unrestricted: metadata sync and bundle downloads allowed.
    #[default]
    Online,
    /// Metered: metadata checks (KiB) allowed, bundle downloads (hundreds of
    /// MiB) refused unless `meteredAllowsFetch` says otherwise.
    Metered,
    /// No network use at all: import-only. `check` and `fetch` are refused;
    /// the offline lockbox path is the update channel.
    Offline,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NetworkPolicy {
    #[serde(default)]
    pub mode: NetworkMode,
    /// Permit bundle downloads on a metered link. Explicitly the exception,
    /// so the metered default is the cheap one.
    #[serde(default)]
    pub metered_allows_fetch: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MaintenancePolicy {
    /// When installs may run. An empty list means "any time" -- maintenance
    /// windows are opt-in, because a device with no operator-set window must
    /// still be updatable.
    #[serde(default)]
    pub windows: Vec<MaintenanceWindow>,
}

/// One recurring window, in UTC. UTC rather than local time because the
/// appliance has no trustworthy local-time configuration to read, and a
/// window that silently shifted with a timezone guess would fire in
/// somebody's business hours.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MaintenanceWindow {
    /// Days the window opens on: `mon`..`sun`. Empty means every day.
    #[serde(default)]
    pub days: Vec<String>,
    /// Opening time, `HH:MM` UTC.
    pub start: String,
    /// Closing time, `HH:MM` UTC. A close at or before the open wraps past
    /// midnight into the next day.
    pub end: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RebootGatePolicy {
    /// Health statuses (live-state `health.<component>.status`) that close
    /// the safe-to-reboot gate. The default contract is the single word
    /// `blocking`: an application that must not be interrupted reports
    /// `ReportHealth(component, "blocking", why)` and clears it when done.
    /// `mos-health`'s `degraded` (disk pressure) deliberately does NOT block
    /// -- a reboot neither worsens nor is worsened by a full `/var`.
    #[serde(default = "default_blocking_statuses")]
    pub blocking_statuses: Vec<String>,
    /// Longest override TTL this device grants, capped at
    /// [`OVERRIDE_CEILING_SECONDS`] whatever the file says.
    #[serde(default = "default_override_max")]
    pub override_max_seconds: u64,
}

fn default_blocking_statuses() -> Vec<String> {
    vec!["blocking".to_string()]
}
fn default_override_max() -> u64 {
    OVERRIDE_CEILING_SECONDS
}

impl Default for RebootGatePolicy {
    fn default() -> Self {
        Self {
            blocking_statuses: default_blocking_statuses(),
            override_max_seconds: default_override_max(),
        }
    }
}

impl RebootGatePolicy {
    /// The TTL ceiling actually granted: the file's value, never above the
    /// built-in ceiling -- a policy file cannot mint a week-long override.
    pub fn override_ceiling(&self) -> u64 {
        self.override_max_seconds.min(OVERRIDE_CEILING_SECONDS)
    }
}

/// What this device follows and where it looks: the four keys layer 1 bakes
/// defaults for, after layer 2 has had its say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The effective source URL. `None` is *no online source configured
    /// anywhere*, not *fall back to the baked one*.
    pub url: Option<String>,
    pub channel: String,
    pub mode: UpdateMode,
    pub check_interval_minutes: u64,
}

/// The workspace values layer 2 owns outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub repo_dir: String,
    pub state_path: String,
    pub max_bytes: u64,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            repo_dir: default_repo_dir(),
            state_path: default_state_path(),
            max_bytes: default_max_bytes(),
        }
    }
}

/// The policy after §5.1's precedence: what every caller reads.
#[derive(Debug, Clone, Default)]
pub struct EffectivePolicy {
    /// `None` when the operator document exists and did not load.
    ///
    /// Not the baked selection, and not the code default either: a device
    /// whose configuration is unreadable does not know which channel it is
    /// on, and saying so is the whole point. Every action that turns on the
    /// answer is refused while this is `None`.
    pub selection: Option<Selection>,
    pub workspace: Workspace,
    pub network: NetworkPolicy,
    pub maintenance: MaintenancePolicy,
    pub reboot_gate: RebootGatePolicy,
}

impl EffectivePolicy {
    /// The policy of a device whose operator document did not load.
    ///
    /// **Takes no arguments, and that is the design.** The silent defect this
    /// slice exists to prevent is a malformed layer 2 quietly resolving to
    /// the baked channel; a constructor with no access to the baked layer
    /// cannot do it, whatever a later caller passes. The gate and the windows
    /// come up on the code defaults so the device stays operable.
    fn unknown_selection() -> Self {
        Self::default()
    }

    /// Resolve layer 2 over layer 1, per key.
    fn resolve(baked: &BakedUpdate, document: OperatorDocument) -> Self {
        Self {
            selection: Some(Selection {
                url: document.source.url.or_else(|| baked.source.clone()),
                channel: document
                    .source
                    .channel
                    .unwrap_or_else(|| baked.channel.clone()),
                mode: document.policy.unwrap_or(baked.policy),
                check_interval_minutes: document
                    .check_interval_minutes
                    .unwrap_or(baked.check_interval_minutes),
            }),
            workspace: Workspace {
                repo_dir: document.source.repo_dir,
                state_path: document.source.state_path,
                max_bytes: document.source.max_bytes,
            },
            network: document.network,
            maintenance: document.maintenance,
            reboot_gate: document.reboot_gate,
        }
    }
}

/// One load of the operator document: the effective policy, or the reason it
/// could not be read. Both, never neither -- a caller always has a policy
/// object to evaluate the gate with, and always knows whether it may trust
/// the selection inside it.
pub struct LoadedPolicy {
    pub policy: EffectivePolicy,
    /// `Some` when the file exists and does not parse or validate. The
    /// restricted actions are refused while this is set (fail closed); the
    /// reboot gate keeps evaluating with the defaults (fail open there would
    /// mean an unreadable file bricks the reboot button).
    pub error: Option<String>,
}

impl LoadedPolicy {
    /// Minutes between automatic checks, or `None` when this device
    /// initiates nothing: `policy = "off"`, an interval of `0`, or a document
    /// that did not load -- a device whose configuration is unreadable does
    /// not go and check on its own.
    pub fn auto_check_minutes(&self) -> Option<u64> {
        let selection = self.policy.selection.as_ref()?;
        match selection.mode {
            UpdateMode::Off => None,
            // `auto` checks on the same cadence `check` does; the fetch and
            // install steps behind it are PLAN-071's slice.
            UpdateMode::Check | UpdateMode::Auto => {
                (selection.check_interval_minutes > 0).then_some(selection.check_interval_minutes)
            }
        }
    }
}

/// Reads the operator document fresh per decision, resolved over the baked
/// defaults it was built with.
#[derive(Clone)]
pub struct PolicyStore {
    /// `None` = no file to read (dry-run daemons): the baked layer, always.
    path: Option<PathBuf>,
    /// Layer 1. [`BakedUpdate::code_defaults`] until `main.rs` attaches the
    /// device's own, so a test store resolves against something inert.
    baked: BakedUpdate,
}

impl PolicyStore {
    /// Store reading `path`; a missing file is the baked defaults.
    pub fn at(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            baked: BakedUpdate::code_defaults(),
        }
    }

    /// Store with no file at all -- the dry-run/test shape.
    pub fn defaults() -> Self {
        Self {
            path: None,
            baked: BakedUpdate::code_defaults(),
        }
    }

    /// Attach the baked layer read from the image (PLAN-070 §5.1 layer 1).
    #[must_use]
    pub fn with_baked(mut self, baked: BakedUpdate) -> Self {
        self.baked = baked;
        self
    }

    /// The path decisions are read from, for the recorded state.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// Load the current policy. Missing file = the baked defaults; unreadable
    /// or invalid file = an unknown selection plus the error that makes
    /// actions refuse.
    pub fn load(&self) -> LoadedPolicy {
        let invalid = |error: String| LoadedPolicy {
            policy: EffectivePolicy::unknown_selection(),
            error: Some(error),
        };
        let resolved = |document: OperatorDocument| LoadedPolicy {
            policy: EffectivePolicy::resolve(&self.baked, document),
            error: None,
        };
        let Some(path) = &self.path else {
            return resolved(OperatorDocument::default());
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return resolved(OperatorDocument::default());
            }
            Err(err) => return invalid(format!("read {}: {err}", path.display())),
        };
        // Parsed to a `Value` first so the anchor scan sees every key the
        // document names, including ones a widened schema would accept.
        let value = match serde_json::from_str::<Value>(&raw) {
            Ok(value) => value,
            Err(err) => return invalid(format!("parse {}: {err}", path.display())),
        };
        if let Some(key) = anchor_key(&value) {
            return invalid(format!(
                "{}: `{key}` names a trust anchor, and anchors are baked into the image. \
                 The address this device dials is yours to set; what it will accept is not",
                path.display()
            ));
        }
        match serde_json::from_value::<OperatorDocument>(value) {
            Ok(document) => match validate(&document) {
                Ok(()) => resolved(document),
                Err(reason) => invalid(format!("{}: {reason}", path.display())),
            },
            Err(err) => invalid(format!("parse {}: {err}", path.display())),
        }
    }
}

/// The first anchor-shaped key `value` names, at any depth, or `None`.
///
/// Arrays are walked as well as objects: a `trust` block inside a maintenance
/// window would be refused by `deny_unknown_fields` anyway, but this scan is
/// the one that must not have a hole in it.
fn anchor_key(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if ANCHOR_KEYS
                    .iter()
                    .any(|anchor| anchor.eq_ignore_ascii_case(key))
                {
                    return Some(key.clone());
                }
                if let Some(found) = anchor_key(child) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(anchor_key),
        _ => None,
    }
}

/// Validate what serde cannot: the schema tag when the document names one,
/// and that window times parse and days are day names.
fn validate(document: &OperatorDocument) -> Result<(), String> {
    if let Some(schema) = &document.schema
        && schema != SCHEMA_TAG
    {
        return Err(format!(
            "schema is `{schema}`, and this reader knows `{SCHEMA_TAG}`"
        ));
    }
    for window in &document.maintenance.windows {
        minutes_of_day(&window.start)
            .ok_or_else(|| format!("maintenance window start `{}` is not HH:MM", window.start))?;
        minutes_of_day(&window.end)
            .ok_or_else(|| format!("maintenance window end `{}` is not HH:MM", window.end))?;
        for day in &window.days {
            day_index(day).ok_or_else(|| {
                format!("maintenance window day `{day}` is not mon/tue/wed/thu/fri/sat/sun")
            })?;
        }
    }
    Ok(())
}

/// `HH:MM` -> minutes since midnight, or `None` when it is not that.
fn minutes_of_day(clock: &str) -> Option<u32> {
    let (hours, minutes) = clock.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: u32 = hours.parse().ok()?;
    let minutes: u32 = minutes.parse().ok()?;
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

/// `mon`..`sun` -> 0..6, Monday first (chrono's `num_days_from_monday`).
fn day_index(day: &str) -> Option<u32> {
    Some(match day {
        "mon" => 0,
        "tue" => 1,
        "wed" => 2,
        "thu" => 3,
        "fri" => 4,
        "sat" => 5,
        "sun" => 6,
        _ => return None,
    })
}

/// Minutes into the UTC week (Monday 00:00 = 0) for `now`.
fn minutes_of_week(now: DateTime<Utc>) -> u32 {
    now.weekday().num_days_from_monday() * 1440 + now.hour() * 60 + now.minute()
}

const WEEK_MINUTES: u32 = 7 * 1440;

/// Whether `now` (UTC) falls inside `window`.
///
/// A window whose end is at or before its start wraps past midnight; a
/// window listing no days opens every day.
fn window_contains(window: &MaintenanceWindow, now: DateTime<Utc>) -> bool {
    // Validation ran at load; an unparseable window here (impossible via
    // `PolicyStore::load`) simply never matches, which is the closed side.
    let Some(start) = minutes_of_day(&window.start) else {
        return false;
    };
    let Some(end) = minutes_of_day(&window.end) else {
        return false;
    };
    let now_week = minutes_of_week(now);
    let days: Vec<u32> = if window.days.is_empty() {
        (0..7).collect()
    } else {
        window
            .days
            .iter()
            .filter_map(|day| day_index(day))
            .collect()
    };
    for day in days {
        let open = day * 1440 + start;
        let span = if end > start {
            end - start
        } else {
            // Wrapping: 23:00-01:00 is two hours into the next day. Equal
            // start and end reads as a full 24 hours.
            1440 - start + end
        };
        let offset = (now_week + WEEK_MINUTES - open) % WEEK_MINUTES;
        if offset < span {
            return true;
        }
    }
    false
}

/// The refusal for a document that did not load. Reached only alongside
/// [`LoadedPolicy::error`], and written once so no caller has to decide what
/// an unknown selection means.
fn unknown_selection_refusal() -> String {
    "the update policy document did not load, so this device's channel and \
     source are unknown"
        .to_string()
}

/// Why `check` is refused right now, or `None` when it may run.
pub fn check_refusal(loaded: &LoadedPolicy) -> Option<String> {
    if let Some(error) = &loaded.error {
        return Some(format!("update policy file is invalid ({error})"));
    }
    let Some(selection) = &loaded.policy.selection else {
        return Some(unknown_selection_refusal());
    };
    match loaded.policy.network.mode {
        NetworkMode::Offline => {
            Some("network mode is offline: updates arrive by import only".to_string())
        }
        NetworkMode::Online | NetworkMode::Metered => {
            if selection.url.is_none() {
                Some("no update source configured (source.url is unset)".to_string())
            } else {
                None
            }
        }
    }
}

/// Why `fetch` is refused right now, or `None` when it may run.
pub fn fetch_refusal(loaded: &LoadedPolicy) -> Option<String> {
    if let Some(reason) = check_refusal(loaded) {
        return Some(reason);
    }
    if loaded.policy.network.mode == NetworkMode::Metered
        && !loaded.policy.network.metered_allows_fetch
    {
        return Some(
            "network mode is metered: bundle downloads are refused \
             (set network.meteredAllowsFetch to allow them)"
                .to_string(),
        );
    }
    None
}

/// Why an install is refused at `now`, or `None` when it may run.
///
/// Only the maintenance window gates installs: the bundle is already local
/// and verified, so network mode has nothing left to protect.
pub fn install_refusal(loaded: &LoadedPolicy, now: DateTime<Utc>) -> Option<String> {
    if let Some(error) = &loaded.error {
        return Some(format!("update policy file is invalid ({error})"));
    }
    let windows = &loaded.policy.maintenance.windows;
    if windows.is_empty() || windows.iter().any(|window| window_contains(window, now)) {
        return None;
    }
    Some("outside every configured maintenance window".to_string())
}

/// The safe-to-reboot verdict and every reason it is not safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateVerdict {
    pub safe: bool,
    pub reasons: Vec<String>,
    /// True when an active administrative override is what made an otherwise
    /// blocked gate safe. Never true for the install block, which no override
    /// lifts.
    pub overridden: bool,
}

impl GateVerdict {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "safe": self.safe,
            "reasons": self.reasons,
            "overridden": self.overridden,
        })
    }
}

/// Evaluate the safe-to-reboot gate.
///
/// Two classes of block, deliberately unequal:
///
/// - **An install in flight** blocks and no override lifts it. RAUC is
///   mid-write; the A/B design survives the power cut, but nothing is gained
///   by inviting it, and the install finishes in minutes.
/// - **A blocking health report** blocks until the reporter clears it or an
///   administrator overrides -- the override is exactly the judgement call
///   "I know what this application is doing and the reboot outranks it",
///   which is why it is bounded and audited.
pub fn evaluate_gate(
    policy: &RebootGatePolicy,
    health: &Value,
    installing: bool,
    override_active: bool,
) -> GateVerdict {
    let mut reasons = Vec::new();
    if installing {
        reasons
            .push("an update install is writing the other slot; wait for it to finish".to_string());
    }
    let mut health_blocks = Vec::new();
    if let Some(entries) = health.as_object() {
        for (component, entry) in entries {
            let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
            if policy.blocking_statuses.iter().any(|s| s == status) {
                let detail = entry.get("detail").and_then(Value::as_str).unwrap_or("");
                health_blocks.push(format!("health.{component} reports `{status}`: {detail}"));
            }
        }
    }
    let overridden = override_active && !health_blocks.is_empty() && reasons.is_empty();
    if !override_active {
        reasons.extend(health_blocks);
    }
    GateVerdict {
        safe: reasons.is_empty(),
        reasons,
        overridden,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    fn at(weekday_iso: &str) -> DateTime<Utc> {
        // 2026-09-07 is a Monday; offsets below pick the weekday/time.
        DateTime::parse_from_rfc3339(weekday_iso)
            .expect("test instant")
            .with_timezone(&Utc)
    }

    fn window(days: &[&str], start: &str, end: &str) -> MaintenanceWindow {
        MaintenanceWindow {
            days: days.iter().map(|d| (*d).to_string()).collect(),
            start: start.to_string(),
            end: end.to_string(),
        }
    }

    fn loaded(policy: EffectivePolicy) -> LoadedPolicy {
        LoadedPolicy {
            policy,
            error: None,
        }
    }

    /// An effective policy resolved from the code defaults on both layers --
    /// the shape a device with no operator document has.
    fn effective() -> EffectivePolicy {
        EffectivePolicy::resolve(&BakedUpdate::code_defaults(), OperatorDocument::default())
    }

    fn with_url(url: &str) -> EffectivePolicy {
        let mut policy = effective();
        policy
            .selection
            .as_mut()
            .expect("resolved")
            .url
            .replace(url.to_string());
        policy
    }

    #[test]
    fn a_missing_file_is_the_baked_default_and_no_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PolicyStore::at(dir.path().join("updates.json"));
        let loaded = store.load();
        assert!(loaded.error.is_none());
        assert_eq!(loaded.policy.network.mode, NetworkMode::Online);
        assert_eq!(
            loaded.policy.selection.expect("a document").channel,
            "stable"
        );
        assert!(loaded.policy.maintenance.windows.is_empty());
    }

    #[test]
    fn a_parseable_file_is_read_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("updates.json");
        std::fs::write(
            &path,
            r#"{"network": {"mode": "metered"}, "source": {"url": "http://mirror/tuf"}}"#,
        )
        .expect("seed");
        let store = PolicyStore::at(path.clone());
        assert_eq!(store.load().policy.network.mode, NetworkMode::Metered);
        // An edit takes effect on the next load, with no reload verb.
        std::fs::write(&path, r#"{"network": {"mode": "offline"}}"#).expect("edit");
        assert_eq!(store.load().policy.network.mode, NetworkMode::Offline);
    }

    #[test]
    fn an_unparseable_file_fails_closed_not_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("updates.json");
        std::fs::write(&path, "{not json").expect("seed");
        let loaded = PolicyStore::at(path).load();
        let error = loaded
            .error
            .clone()
            .expect("a bad file must carry its error");
        assert!(error.contains("parse"), "error was: {error}");
        // Every restricted action refuses, and the refusal names the file.
        assert!(check_refusal(&loaded).expect("refused").contains("invalid"));
        assert!(fetch_refusal(&loaded).is_some());
        assert!(install_refusal(&loaded, Utc::now()).is_some());
    }

    #[test]
    fn an_unknown_key_is_an_error_not_a_silent_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("updates.json");
        std::fs::write(&path, r#"{"network": {"moed": "offline"}}"#).expect("seed");
        assert!(PolicyStore::at(path).load().error.is_some());
    }

    #[test]
    fn a_malformed_window_is_a_load_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("updates.json");
        for (body, needle) in [
            (
                r#"{"maintenance": {"windows": [{"start": "2:00", "end": "04:00"}]}}"#,
                "not HH:MM",
            ),
            (
                r#"{"maintenance": {"windows": [{"start": "02:00", "end": "24:00"}]}}"#,
                "not HH:MM",
            ),
            (
                r#"{"maintenance": {"windows": [{"days": ["monday"], "start": "02:00", "end": "04:00"}]}}"#,
                "not mon/tue",
            ),
        ] {
            std::fs::write(&path, body).expect("seed");
            let error = PolicyStore::at(path.clone())
                .load()
                .error
                .unwrap_or_else(|| panic!("{body:?} must fail validation"));
            assert!(error.contains(needle), "for {body:?} got: {error}");
        }
    }

    #[test]
    fn offline_mode_refuses_check_and_fetch() {
        let mut policy = with_url("http://mirror/tuf");
        policy.network.mode = NetworkMode::Offline;
        let loaded = loaded(policy);
        assert!(check_refusal(&loaded).expect("refused").contains("offline"));
        assert!(fetch_refusal(&loaded).expect("refused").contains("offline"));
    }

    #[test]
    fn metered_mode_allows_check_but_refuses_fetch_until_allowed() {
        let mut policy = with_url("http://mirror/tuf");
        policy.network.mode = NetworkMode::Metered;
        assert_eq!(check_refusal(&loaded(policy.clone())), None);
        assert!(
            fetch_refusal(&loaded(policy.clone()))
                .expect("refused")
                .contains("metered")
        );
        policy.network.metered_allows_fetch = true;
        assert_eq!(fetch_refusal(&loaded(policy)), None);
    }

    #[test]
    fn no_configured_source_refuses_check_with_its_own_reason() {
        let loaded = loaded(effective());
        assert!(
            check_refusal(&loaded)
                .expect("refused")
                .contains("source.url")
        );
    }

    #[test]
    fn no_windows_means_installs_run_any_time() {
        assert_eq!(
            install_refusal(&loaded(with_url("http://mirror/tuf")), Utc::now()),
            None
        );
    }

    #[test]
    fn a_window_admits_inside_and_refuses_outside() {
        let mut policy = effective();
        policy.maintenance.windows = vec![window(&["mon"], "02:00", "04:00")];
        let loaded = loaded(policy);
        // 2026-09-07 is a Monday.
        assert_eq!(
            install_refusal(&loaded, at("2026-09-07T03:00:00Z")),
            None,
            "inside the Monday window"
        );
        assert!(
            install_refusal(&loaded, at("2026-09-07T04:00:00Z")).is_some(),
            "the end minute is outside"
        );
        assert!(
            install_refusal(&loaded, at("2026-09-08T03:00:00Z")).is_some(),
            "Tuesday is outside a Monday-only window"
        );
    }

    #[test]
    fn a_wrapping_window_covers_past_midnight() {
        let mut policy = effective();
        policy.maintenance.windows = vec![window(&["mon"], "23:00", "01:00")];
        let loaded = loaded(policy);
        assert_eq!(
            install_refusal(&loaded, at("2026-09-07T23:30:00Z")),
            None,
            "Monday 23:30 is inside"
        );
        assert_eq!(
            install_refusal(&loaded, at("2026-09-08T00:30:00Z")),
            None,
            "Tuesday 00:30 is still the Monday window"
        );
        assert!(
            install_refusal(&loaded, at("2026-09-08T01:30:00Z")).is_some(),
            "Tuesday 01:30 is past the wrap"
        );
    }

    #[test]
    fn a_dayless_window_opens_every_day() {
        let mut policy = effective();
        policy.maintenance.windows = vec![window(&[], "02:00", "04:00")];
        let loaded = loaded(policy);
        for day in 7..14 {
            let now = Utc
                .with_ymd_and_hms(2026, 9, day, 3, 0, 0)
                .single()
                .expect("valid date");
            assert_eq!(install_refusal(&loaded, now), None, "day {day}");
        }
    }

    #[test]
    fn an_open_gate_has_no_reasons() {
        let verdict = evaluate_gate(&RebootGatePolicy::default(), &json!({}), false, false);
        assert_eq!(
            verdict,
            GateVerdict {
                safe: true,
                reasons: vec![],
                overridden: false
            }
        );
    }

    #[test]
    fn an_install_in_flight_closes_the_gate_and_no_override_lifts_it() {
        let verdict = evaluate_gate(&RebootGatePolicy::default(), &json!({}), true, true);
        assert!(!verdict.safe);
        assert!(!verdict.overridden);
        assert!(verdict.reasons[0].contains("install"));
    }

    #[test]
    fn a_blocking_health_report_closes_the_gate_and_an_override_lifts_it() {
        let health = json!({
            "batch-writer": { "status": "blocking", "detail": "flushing journal" },
            "var": { "status": "degraded", "detail": "/var at 91% of capacity" },
        });
        let policy = RebootGatePolicy::default();
        let closed = evaluate_gate(&policy, &health, false, false);
        assert!(!closed.safe);
        assert_eq!(closed.reasons.len(), 1, "degraded must not block");
        assert!(closed.reasons[0].contains("batch-writer"));
        let lifted = evaluate_gate(&policy, &health, false, true);
        assert!(lifted.safe);
        assert!(lifted.overridden);
    }

    #[test]
    fn the_override_ceiling_caps_whatever_the_file_says() {
        let policy = RebootGatePolicy {
            override_max_seconds: 86_400,
            ..RebootGatePolicy::default()
        };
        assert_eq!(policy.override_ceiling(), OVERRIDE_CEILING_SECONDS);
        let tighter = RebootGatePolicy {
            override_max_seconds: 60,
            ..RebootGatePolicy::default()
        };
        assert_eq!(tighter.override_ceiling(), 60);
    }
}

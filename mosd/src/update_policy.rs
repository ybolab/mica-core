//! Update policy: what the device does on its own (`off`/`check`/`auto`),
//! maintenance windows, metered/offline mode, the check cadence, what
//! happens after an automatic install, and the safe-to-reboot gate.
//!
//! The policy lives in its own TOML file beside the settings store
//! (`update-policy.toml` in the STATE directory) rather than in the settings
//! tree. Deliberate: adding settings keys means a schema bump plus a
//! migration, and a concurrent workstream owns the next bump — a second
//! bumper would hand the merge an unresolvable version conflict. STATE is
//! the right tier for it, too: PLAN-061 keeps small authoritative metadata on
//! STATE and sends only large bytes to the `/mos` DATA workspace, and a
//! policy file is a few hundred bytes whose loss would make the workspace
//! ambiguous. The file is operator-edited, read fresh on every policy
//! decision, and a missing file is the default policy. A file that exists but does not parse is NOT the
//! default policy: every action the policy could restrict is refused until
//! the file is fixed, because "unreadable" silently becoming "unrestricted"
//! is how a metered device downloads a 500 MB bundle.
//!
//! What this module does not do: hold state. The reboot-gate override and the
//! lifecycle machine live in [`crate::update_lifecycle`]; everything here is
//! a pure function of the policy document, the clock and the health tree, so
//! every rule is unit-testable without a daemon.

use std::path::PathBuf;

use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::Deserialize;
use serde_json::Value;

/// Longest administrative reboot-gate override a policy may allow, and the
/// built-in default. One hour: long enough to carry a maintenance action,
/// short enough that a forgotten override does not stand disarmed for a week.
pub const OVERRIDE_CEILING_SECONDS: u64 = 3600;

/// What the device does on its own, and the one key that says it.
///
/// One enum rather than three booleans (`autoCheck`, `autoFetch`,
/// `autoInstall`): three booleans admit combinations with no meaning —
/// install without fetch — and the one combination worth having,
/// fetch-but-not-install, is [`RebootPolicy::Manual`] under [`Self::Auto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateMode {
    /// The device initiates nothing and no timer arms. Manual check, fetch
    /// and install stay available behind their existing gates, and so does
    /// the offline import: `off` is not "updates disabled", it is "the
    /// device starts nothing".
    Off,
    /// Metadata checks on `checkIntervalMinutes` and nothing else — never
    /// fetches, never installs. The default, so what a shipped device does
    /// is what it did before this enum existed.
    #[default]
    Check,
    /// Checks, then fetches, then installs inside a maintenance window, then
    /// reboots or does not per [`RebootPolicy`]. The driver and every gate
    /// it meets are [`crate::update_auto`].
    Auto,
}

impl UpdateMode {
    /// The document's spelling, for the recorded state and for a log line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Check => "check",
            Self::Auto => "auto",
        }
    }
}

/// What the automatic path does once a bundle is installed and the new slot
/// waits for its first boot.
///
/// Separate from [`UpdateMode`] because "install automatically" and "reboot
/// automatically" are not the same promise: an appliance running a machine
/// may well want the new slot written and staged while the reboot is
/// reserved for a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RebootPolicy {
    /// Stop at `reboot-required` and wait for an operator. The default,
    /// because it is what makes `auto` safe to recommend to someone who has
    /// not read the design.
    #[default]
    Manual,
    /// Reboot inside the same maintenance window, honouring the
    /// safe-to-reboot gate exactly as `Reboot` does — and never arming its
    /// override.
    Window,
}

impl RebootPolicy {
    /// The document's spelling, for the recorded state and for a log line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Window => "window",
        }
    }
}

/// The policy document. Every field has a default, so an absent file — the
/// state of every device until an operator writes one — is a complete policy.
///
/// `deny_unknown_fields` for the reason the settings tree carries it: a
/// mistyped key must fail loudly, not silently configure nothing.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UpdatePolicy {
    /// What the device does on its own. The document's key is `policy`; the
    /// field is `mode` so that reading it is not `policy.policy`.
    #[serde(default, rename = "policy")]
    pub mode: UpdateMode,
    /// Minutes between automatic metadata checks; `0` disables them. One
    /// cadence for both `check` and `auto` — [`UpdateMode`] decides what
    /// happens after a check, not how often one runs.
    #[serde(default = "default_check_interval")]
    pub check_interval_minutes: u64,
    /// What the automatic path does after an install. Read only under
    /// [`UpdateMode::Auto`], which is the only mode that installs.
    #[serde(default)]
    pub reboot_policy: RebootPolicy,
    #[serde(default)]
    pub source: SourcePolicy,
    #[serde(default)]
    pub network: NetworkPolicy,
    #[serde(default)]
    pub maintenance: MaintenancePolicy,
    #[serde(default)]
    pub reboot_gate: RebootGatePolicy,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        Self {
            mode: UpdateMode::default(),
            check_interval_minutes: default_check_interval(),
            reboot_policy: RebootPolicy::default(),
            source: SourcePolicy::default(),
            network: NetworkPolicy::default(),
            maintenance: MaintenancePolicy::default(),
            reboot_gate: RebootGatePolicy::default(),
        }
    }
}

/// Where updates come from and how much of the `/mos/updates` workspace
/// they may hold. The paths default to the deployment contract
/// `docs/design/updates.md` records; `url` has no default because there is
/// no fleet mirror to assume. Where bundles are staged is NOT a policy
/// knob: the client's workspace is `/mos/updates` and nothing else
/// (PLAN-061/063), so there is no key that could point it elsewhere.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SourcePolicy {
    /// Base URL of the published TUF repository. Absent = no online source:
    /// `check`/`fetch` are refused and the offline import path remains.
    pub url: Option<String>,
    /// Release channel to follow (`rauc-update check --channel`).
    #[serde(default = "default_channel")]
    pub channel: String,
    /// Local metadata mirror directory (`rauc-update --repo`).
    #[serde(default = "default_repo_dir")]
    pub repo_dir: String,
    /// Pinned trusted root (`rauc-update --root`).
    #[serde(default = "default_root_path")]
    pub root_path: String,
    /// Persistent rollback state (`rauc-update --state`).
    #[serde(default = "default_state_path")]
    pub state_path: String,
    /// Byte budget for the workspace — downloads/, verified/ and staging/
    /// together (`rauc-update --max-bytes`); readiness also requires the
    /// DATA pool to back what is unspent of it.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
}

fn default_channel() -> String {
    "stable".to_string()
}
fn default_repo_dir() -> String {
    "/var/lib/mos/update/tuf-mirror".to_string()
}
fn default_root_path() -> String {
    "/usr/share/mos/uptane/root.json".to_string()
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

impl Default for SourcePolicy {
    fn default() -> Self {
        Self {
            url: None,
            channel: default_channel(),
            repo_dir: default_repo_dir(),
            root_path: default_root_path(),
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

fn default_check_interval() -> u64 {
    // Daily. A check is a bounded metadata read, safe on the default online
    // mode; metered/offline modes refuse it wholesale regardless of cadence.
    1440
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MaintenancePolicy {
    /// When installs may run. An empty list means "any time" — maintenance
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
    /// — a reboot neither worsens nor is worsened by a full `/var`.
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
    /// built-in ceiling — a policy file cannot mint a week-long override.
    pub fn override_ceiling(&self) -> u64 {
        self.override_max_seconds.min(OVERRIDE_CEILING_SECONDS)
    }
}

/// One load of the policy file: the policy, or the reason it could not be
/// read. Both, never neither — a caller always has a policy object to
/// evaluate the gate with, and always knows whether it may trust it.
pub struct LoadedPolicy {
    pub policy: UpdatePolicy,
    /// `Some` when the file exists and does not parse or validate. The
    /// restricted actions are refused while this is set (fail closed); the
    /// reboot gate keeps evaluating with the defaults (fail open there would
    /// mean an unreadable file bricks the reboot button).
    pub error: Option<String>,
}

/// Reads the policy file fresh per decision. A handful of bytes per action
/// is cheaper than a watch, and an operator edit takes effect on the next
/// decision with no restart and no reload verb.
#[derive(Clone)]
pub struct PolicyStore {
    /// `None` = no file to read (dry-run daemons): defaults, always.
    path: Option<PathBuf>,
}

impl PolicyStore {
    /// Store reading `path`; a missing file is the default policy.
    pub fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Store with no file at all — the dry-run/test shape.
    pub fn defaults() -> Self {
        Self { path: None }
    }

    /// The path decisions are read from, for the recorded state.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// Load the current policy. Missing file = defaults; unreadable or
    /// invalid file = defaults plus the error that makes actions refuse.
    pub fn load(&self) -> LoadedPolicy {
        let Some(path) = &self.path else {
            return LoadedPolicy {
                policy: UpdatePolicy::default(),
                error: None,
            };
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return LoadedPolicy {
                    policy: UpdatePolicy::default(),
                    error: None,
                };
            }
            Err(err) => {
                return LoadedPolicy {
                    policy: UpdatePolicy::default(),
                    error: Some(format!("read {}: {err}", path.display())),
                };
            }
        };
        match toml::from_str::<UpdatePolicy>(&raw) {
            Ok(policy) => match validate(&policy) {
                Ok(()) => LoadedPolicy {
                    policy,
                    error: None,
                },
                Err(reason) => LoadedPolicy {
                    policy: UpdatePolicy::default(),
                    error: Some(format!("{}: {reason}", path.display())),
                },
            },
            Err(err) => LoadedPolicy {
                policy: UpdatePolicy::default(),
                error: Some(format!("parse {}: {err}", path.display())),
            },
        }
    }
}

/// Validate what serde cannot: window times parse, days are day names, and
/// `auto` names a window to install in.
///
/// Public because there is one rule set and it has two callers: this
/// module's reader, which fails closed on a document that reached the disk,
/// and the write route that must refuse the same document at the API with
/// the same sentence. Two spellings of one rule is how they drift.
pub fn validate(policy: &UpdatePolicy) -> Result<(), String> {
    for window in &policy.maintenance.windows {
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
    // Zero windows means "any time", which is right for a manual install — a
    // device with no operator-set window must still be updatable by a human
    // who is standing there — and wrong for an automatic one, where it would
    // mean "install the moment a bundle lands". Requiring the window is what
    // makes "automatic installation inside a time window" literally true,
    // and it forces the operator to name the hour rather than inherit one.
    if policy.mode == UpdateMode::Auto && policy.maintenance.windows.is_empty() {
        return Err(
            "policy `auto` requires at least one maintenance window: zero windows means \
             `any time`, which for an automatic install means `the moment a bundle lands`"
                .to_string(),
        );
    }
    Ok(())
}

/// `HH:MM` → minutes since midnight, or `None` when it is not that.
fn minutes_of_day(clock: &str) -> Option<u32> {
    let (hours, minutes) = clock.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: u32 = hours.parse().ok()?;
    let minutes: u32 = minutes.parse().ok()?;
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

/// `mon`..`sun` → 0..6, Monday first (chrono's `num_days_from_monday`).
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
            // Wrapping: 23:00–01:00 is two hours into the next day. Equal
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

/// Why `check` is refused right now, or `None` when it may run.
pub fn check_refusal(loaded: &LoadedPolicy) -> Option<String> {
    if let Some(error) = &loaded.error {
        return Some(format!("update policy file is invalid ({error})"));
    }
    match loaded.policy.network.mode {
        NetworkMode::Offline => {
            Some("network mode is offline: updates arrive by import only".to_string())
        }
        NetworkMode::Online | NetworkMode::Metered => {
            if loaded.policy.source.url.is_none() {
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
///   administrator overrides — the override is exactly the judgement call
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

    fn loaded(policy: UpdatePolicy) -> LoadedPolicy {
        LoadedPolicy {
            policy,
            error: None,
        }
    }

    #[test]
    fn a_missing_file_is_the_default_policy_and_no_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PolicyStore::at(dir.path().join("update-policy.toml"));
        let loaded = store.load();
        assert!(loaded.error.is_none());
        assert_eq!(loaded.policy.network.mode, NetworkMode::Online);
        assert_eq!(loaded.policy.check_interval_minutes, 1440);
        assert!(loaded.policy.maintenance.windows.is_empty());
    }

    #[test]
    fn a_parseable_file_is_read_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("update-policy.toml");
        std::fs::write(
            &path,
            "[network]\nmode = \"metered\"\n\n[source]\nurl = \"http://mirror/tuf\"\n",
        )
        .expect("seed");
        let store = PolicyStore::at(path.clone());
        assert_eq!(store.load().policy.network.mode, NetworkMode::Metered);
        // An edit takes effect on the next load, with no reload verb.
        std::fs::write(&path, "[network]\nmode = \"offline\"\n").expect("edit");
        assert_eq!(store.load().policy.network.mode, NetworkMode::Offline);
    }

    #[test]
    fn an_unparseable_file_fails_closed_not_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("update-policy.toml");
        std::fs::write(&path, "network = \"not a table\"").expect("seed");
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
        let path = dir.path().join("update-policy.toml");
        std::fs::write(&path, "[network]\nmoed = \"offline\"\n").expect("seed");
        assert!(PolicyStore::at(path).load().error.is_some());
    }

    #[test]
    fn a_malformed_window_is_a_load_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("update-policy.toml");
        for (body, needle) in [
            (
                "[[maintenance.windows]]\nstart = \"2:00\"\nend = \"04:00\"\n",
                "not HH:MM",
            ),
            (
                "[[maintenance.windows]]\nstart = \"02:00\"\nend = \"24:00\"\n",
                "not HH:MM",
            ),
            (
                "[[maintenance.windows]]\ndays = [\"monday\"]\nstart = \"02:00\"\nend = \"04:00\"\n",
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
        let mut policy = UpdatePolicy::default();
        policy.source.url = Some("http://mirror/tuf".to_string());
        policy.network.mode = NetworkMode::Offline;
        let loaded = loaded(policy);
        assert!(check_refusal(&loaded).expect("refused").contains("offline"));
        assert!(fetch_refusal(&loaded).expect("refused").contains("offline"));
    }

    #[test]
    fn metered_mode_allows_check_but_refuses_fetch_until_allowed() {
        let mut policy = UpdatePolicy::default();
        policy.source.url = Some("http://mirror/tuf".to_string());
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
        let loaded = loaded(UpdatePolicy::default());
        assert!(
            check_refusal(&loaded)
                .expect("refused")
                .contains("source.url")
        );
    }

    #[test]
    fn no_windows_means_installs_run_any_time() {
        let mut policy = UpdatePolicy::default();
        policy.source.url = Some("http://mirror/tuf".to_string());
        assert_eq!(install_refusal(&loaded(policy), Utc::now()), None);
    }

    #[test]
    fn a_window_admits_inside_and_refuses_outside() {
        let mut policy = UpdatePolicy::default();
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
        let mut policy = UpdatePolicy::default();
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
        let mut policy = UpdatePolicy::default();
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

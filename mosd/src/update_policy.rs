//! Update policy: the refusals, the maintenance window, the auto-check
//! cadence and the safe-to-reboot gate.
//!
//! **The documents and the precedence are not here.** They live in
//! [`mosd_settings::configuration`], because apid reports the effective
//! policy on `GET /api/v1/provisioning/status` and a status route computing
//! its own answer would eventually disagree with the subsystem it describes.
//! This module is the *semantics*: what the resolved document lets a device
//! do right now, and why it refuses when it refuses.
//!
//! What that leaves here is the one thing mosd needs and apid does not: a
//! policy object that **always exists**, even when the operator document did
//! not load. [`LoadedPolicy`] is the shape — the effective policy plus the
//! reason it may not be trusted — and the split it encodes is PLAN-070 §5.1's:
//!
//! - **Absent document** → the baked defaults. A device that has never been
//!   configured follows what it shipped with.
//! - **Malformed document** → fail closed on the *actions*, fail open on the
//!   *device*, and **never** silently adopt the baked channel. Every
//!   capability the document gates is refused with a message naming the file;
//!   the reboot gate keeps evaluating with the code defaults, because an
//!   unreadable file must not brick the reboot button. `EffectivePolicy` then
//!   carries no selection at all, so there is nothing for a later caller to
//!   read a channel out of.
//!
//! The file is read fresh on every policy decision — a handful of bytes per
//! action is cheaper than a watch, and an operator edit takes effect on the
//! next decision with no restart and no reload verb.
//!
//! What this module does not do: hold state. The reboot-gate override and the
//! lifecycle machine live in [`crate::update_lifecycle`]; everything here is
//! a pure function of the resolved document, the clock and the health tree.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde_json::Value;

use mosd_settings::configuration::{self, BakedUpdate};

use crate::update_codes::{self, CodedReason};

// Re-exported so the lifecycle, the automatic driver and `main.rs` name one
// module for the policy, not two: the types are the library's, the semantics
// below are this module's.
pub use mosd_settings::configuration::{
    DEFAULT_UPDATES_PATH as DEFAULT_POLICY_PATH, EffectivePolicy, NetworkMode, RebootGatePolicy,
    RebootPolicy, Selection, UpdateMode, Workspace,
};

/// One load of the operator document: the effective policy, or the reason it
/// could not be read. Both, never neither — a caller always has a policy
/// object to evaluate the gate with, and always knows whether it may trust
/// the selection inside it.
pub struct LoadedPolicy {
    pub policy: EffectivePolicy,
    /// `Some` when the file exists and does not read, parse or validate.
    /// The restricted actions are refused while this is set.
    pub error: Option<String>,
}

impl LoadedPolicy {
    /// Minutes between automatic checks, or `None` when this device
    /// initiates nothing: `policy = "off"`, an interval of `0`, or a document
    /// that did not load — a device whose configuration is unreadable does
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

    /// Store with no file at all — the dry-run/test shape.
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

    /// Load the current policy through the shared reader and the shared
    /// precedence, and turn the two outcomes into the shape a daemon needs.
    ///
    /// This is the *only* difference between mosd's view and apid's: the
    /// library returns `Err` for a document that did not load, and mosd needs
    /// a policy object anyway so the reboot gate keeps working. It does not
    /// invent one from the baked layer — [`EffectivePolicy::unknown_selection`]
    /// cannot see it — so both callers still answer the same question the
    /// same way.
    pub fn load(&self) -> LoadedPolicy {
        let Some(path) = &self.path else {
            return LoadedPolicy {
                policy: configuration::resolve(
                    &self.baked,
                    configuration::UpdatesDocument::default(),
                ),
                error: None,
            };
        };
        match configuration::load_updates(path) {
            Ok(document) => LoadedPolicy {
                policy: configuration::resolve(&self.baked, document),
                error: None,
            },
            Err(err) => LoadedPolicy {
                policy: EffectivePolicy::unknown_selection(),
                error: Some(err.to_string()),
            },
        }
    }
}

/// The refusal for a document that did not load. Reached only alongside
/// [`LoadedPolicy::error`], and written once so no caller has to decide what
/// an unknown selection means.
fn unknown_selection_refusal() -> CodedReason {
    CodedReason::new(
        update_codes::POLICY_NOT_LOADED,
        "the update policy document did not load, so this device's channel and \
         source are unknown",
    )
}

/// Why `check` is refused right now, or `None` when it may run.
///
/// The code travels with the sentence (PLAN-076 B4): this predicate is the one
/// place that knows WHICH rule refused, so it is the one place that can name
/// the rule without reading its own sentence back.
pub fn check_refusal(loaded: &LoadedPolicy) -> Option<CodedReason> {
    if let Some(error) = &loaded.error {
        return Some(CodedReason::new(
            update_codes::REFUSED_POLICY_INVALID,
            format!("update policy file is invalid ({error})"),
        ));
    }
    let Some(selection) = &loaded.policy.selection else {
        return Some(unknown_selection_refusal());
    };
    match loaded.policy.network.mode {
        NetworkMode::Offline => Some(CodedReason::new(
            update_codes::REFUSED_NETWORK_OFFLINE,
            "network mode is offline: updates arrive by import only",
        )),
        NetworkMode::Online | NetworkMode::Metered => {
            if selection.url.is_none() {
                Some(CodedReason::new(
                    update_codes::NO_SOURCE_CONFIGURED,
                    "no update source configured (source.url is unset)",
                ))
            } else {
                None
            }
        }
    }
}

/// Why `fetch` is refused right now, or `None` when it may run.
pub fn fetch_refusal(loaded: &LoadedPolicy) -> Option<CodedReason> {
    if let Some(refusal) = check_refusal(loaded) {
        return Some(refusal);
    }
    if loaded.policy.network.mode == NetworkMode::Metered
        && !loaded.policy.network.metered_allows_fetch
    {
        return Some(CodedReason::new(
            update_codes::REFUSED_NETWORK_METERED,
            "network mode is metered: bundle downloads are refused \
             (set network.meteredAllowsFetch to allow them)",
        ));
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
    if windows.is_empty() || windows.iter().any(|window| window.contains(now)) {
        return None;
    }
    Some("outside every configured maintenance window".to_string())
}

/// The safe-to-reboot verdict and every reason it is not safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateVerdict {
    pub safe: bool,
    pub reasons: Vec<String>,
    /// PLAN-076 B4's health-path vocabulary: one code per entry of `reasons`,
    /// in the same order, so a fleet groups blocks by class while an operator
    /// reads the sentence that names the component.
    ///
    /// Closed by construction rather than by a mapping with a fallback: this
    /// verdict has exactly two producers, both in [`evaluate_gate`], and
    /// neither takes a code from outside. There is no `unknown` here because
    /// there is no foreign text to classify.
    pub codes: Vec<&'static str>,
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
            "codes": self.codes,
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
    let mut codes = Vec::new();
    if installing {
        reasons
            .push("an update install is writing the other slot; wait for it to finish".to_string());
        codes.push(update_codes::GATE_INSTALL_IN_FLIGHT);
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
        // The two vectors are extended together, so `reasons[i]` and
        // `codes[i]` are the same block: a consumer may zip them.
        codes.extend(std::iter::repeat_n(
            update_codes::GATE_HEALTH_BLOCKING,
            health_blocks.len(),
        ));
        reasons.extend(health_blocks);
    }
    GateVerdict {
        safe: reasons.is_empty(),
        reasons,
        codes,
        overridden,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use mosd_settings::configuration::{
        MaintenanceWindow, OVERRIDE_CEILING_SECONDS, UpdatesDocument,
    };
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
        configuration::resolve(&BakedUpdate::code_defaults(), UpdatesDocument::default())
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
        let refusal = check_refusal(&loaded).expect("refused");
        assert!(refusal.text.contains("invalid"));
        assert_eq!(refusal.code, update_codes::REFUSED_POLICY_INVALID);
        assert!(fetch_refusal(&loaded).is_some());
        assert!(install_refusal(&loaded, Utc::now()).is_some());
        // And the selection is unknown rather than the baked default: this is
        // the whole rule, so it is asserted here and not only in the library.
        assert!(loaded.policy.selection.is_none());
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
        for refusal in [
            check_refusal(&loaded).expect("refused"),
            fetch_refusal(&loaded).expect("refused"),
        ] {
            assert!(refusal.text.contains("offline"));
            assert_eq!(refusal.code, update_codes::REFUSED_NETWORK_OFFLINE);
        }
    }

    #[test]
    fn metered_mode_allows_check_but_refuses_fetch_until_allowed() {
        let mut policy = with_url("http://mirror/tuf");
        policy.network.mode = NetworkMode::Metered;
        assert_eq!(check_refusal(&loaded(policy.clone())), None);
        let metered = fetch_refusal(&loaded(policy.clone())).expect("refused");
        assert!(metered.text.contains("metered"));
        assert_eq!(metered.code, update_codes::REFUSED_NETWORK_METERED);
        policy.network.metered_allows_fetch = true;
        assert_eq!(fetch_refusal(&loaded(policy)), None);
    }

    // `policy-not-loaded`'s refusal branch, driven directly at the predicate.
    //
    // NEITHER of its two production sites is reachable: `check_refusal` tests
    // `loaded.error` first, so a document that failed to load answers
    // `policy-invalid` before it gets here, and `selection_of` is reached only
    // after the same predicate admitted the operation. The branch is kept
    // because "the document did not load" must never resolve to the baked
    // channel (PLAN-070 §5.1), and the code is asserted here so that a caller
    // who does reach it gets a published word rather than an invented one.
    #[test]
    fn a_selection_that_is_absent_without_an_error_is_still_a_published_code() {
        let mut policy = effective();
        policy.selection = None;
        let refusal = check_refusal(&loaded(policy)).expect("refused");
        assert_eq!(refusal.code, update_codes::POLICY_NOT_LOADED);
        assert!(refusal.text.contains("did not load"));
    }

    #[test]
    fn no_configured_source_refuses_check_with_its_own_reason() {
        let loaded = loaded(effective());
        let refusal = check_refusal(&loaded).expect("refused");
        assert!(refusal.text.contains("source.url"));
        assert_eq!(refusal.code, update_codes::NO_SOURCE_CONFIGURED);
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
                codes: vec![],
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
        assert_eq!(verdict.codes, vec![update_codes::GATE_INSTALL_IN_FLIGHT]);
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
        // PLAN-076 B4's health path: the class is a fixed word and the
        // component that reported it stays in the sentence beside it, one code
        // per reason and in the same order.
        assert_eq!(closed.codes, vec![update_codes::GATE_HEALTH_BLOCKING]);
        // Both blocks at once, so "same index" is a property and not a
        // coincidence of a one-element vector.
        let both = evaluate_gate(&policy, &health, true, false);
        assert_eq!(both.reasons.len(), 2);
        assert_eq!(
            both.codes,
            vec![
                update_codes::GATE_INSTALL_IN_FLIGHT,
                update_codes::GATE_HEALTH_BLOCKING
            ]
        );
        assert!(both.reasons[0].contains("install"));
        assert!(both.reasons[1].contains("batch-writer"));
        let lifted = evaluate_gate(&policy, &health, false, true);
        assert!(lifted.safe);
        assert!(lifted.overridden);
        assert!(lifted.codes.is_empty(), "a lifted gate names no block");
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

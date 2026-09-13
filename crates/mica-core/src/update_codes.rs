//! Closed codes for native acquisition, operator policy and reboot gates.
//! Details remain separate from machine-readable failure and deferral codes.

/// Every failure this vocabulary does not name.
///
/// Deliberately information-losing: the alternative is the caller's text, and
/// the text is what this whole module exists to keep off the wire. The words
/// are logged where the mapping happens, so nothing a support case needs is
/// destroyed — it is only moved to the journal, where an unbounded string is
/// harmless.
pub const UNKNOWN: &str = "unknown";

/// A failure, its code, and the words that go with it.
///
/// The two travel together from the site that mints them to the site that
/// records them, so there is no window in which a caller holds a reason with
/// no code and has to invent one. `text` is for a human and is never the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodedReason {
    /// A word from this module. `&'static str` on purpose: a code that could
    /// be computed at runtime is a code that could be a message.
    pub code: &'static str,
    /// The same failure in words, for the operator reading one device.
    pub text: String,
}

impl CodedReason {
    pub fn new(code: &'static str, text: impl Into<String>) -> Self {
        Self {
            code,
            text: text.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// `update.lifecycle.code` for state `failed`: an operation this daemon ran
// did not produce its outcome. Minted at the six sites that construct one, so
// no classifier reads mos's own sentences back.
// ---------------------------------------------------------------------------

/// `mica-deploy` could not be run to completion: spawn failed, or the bound on
/// the subprocess expired. Nothing was learned about the update.
pub const CLIENT_SPAWN_FAILED: &str = "client-spawn-failed";

/// `mica-deploy` ran and exited with a failure status that is not one of its
/// contract's ("nothing compatible", "workspace not ready"). The reason beside
/// this code is the client's stderr tail.
pub const CLIENT_EXIT_FAILURE: &str = "client-exit-failure";

/// `mica-deploy` exited successfully but did not print what its contract says
/// it prints, so there is no outcome to record. A client/daemon version skew
/// or a client bug; never an update fault.
pub const CLIENT_OUTPUT_UNPARSEABLE: &str = "client-output-unparseable";

/// A fetch named a path that is not a verified bundle directly inside
/// `verified/`. Refused rather than recorded: `docs/design/updates.md` §1.1's
/// "never installable by filename alone" is enforced here, not trusted.
pub const UNVERIFIED_DEPLOYMENT_PATH: &str = "unverified-deployment-path";

/// There is no `source.url` to acquire from.
pub const NO_SOURCE_CONFIGURED: &str = "no-source-configured";

/// The operator policy document did not load, so this device has no channel.
/// Never resolved to the baked default (PLAN-070 §5.1): "unknown" and "unset"
/// are different answers and only one of them is safe to act on.
pub const POLICY_NOT_LOADED: &str = "policy-not-loaded";

// ---------------------------------------------------------------------------
// `update.lifecycle.code` for state `update-unavailable`, and
// `update.lifecycle.workspace.kind`: the `/mos/updates` workspace refused the
// acquisition before it started.
// ---------------------------------------------------------------------------

/// The native workspace probe refused acquisition; detail preserves the filesystem error.
pub const WORKSPACE_PROBE_FAILED: &str = "probe-failed";

// ---------------------------------------------------------------------------
// `update.lifecycle.last_refusal_code`: an operator action this device
// declined before it started. Minted by the policy predicates that decide it.
// ---------------------------------------------------------------------------

/// The policy file exists and does not parse.
pub const REFUSED_POLICY_INVALID: &str = "policy-invalid";
/// `network.mode` is `offline`: updates arrive by import only.
pub const REFUSED_NETWORK_OFFLINE: &str = "network-offline";
/// `network.mode` is `metered` and `meteredAllowsFetch` is not set, so a
/// bundle download is refused (a metadata check is not).
pub const REFUSED_NETWORK_METERED: &str = "network-metered";
/// The update client binary is not there to run.
pub const REFUSED_CLIENT_UNAVAILABLE: &str = "client-unavailable";

// `last_refusal` carries the newest notable lifecycle event, and two of them
// are not refusals at all. They are coded with the refusals because they share
// the member: a code member that is present for some values of its sibling and
// absent for others is one a client has to special-case.

/// The staged bundle was deleted because the current metadata no longer names
/// it (PLAN-071 §5).
pub const NOTE_DEPLOYMENT_DISCARDED: &str = "deployment-discarded";
// ---------------------------------------------------------------------------
// `update.lifecycle.deferred.reason`: why the last AUTOMATIC pass did not
// proceed (PLAN-071 §2). The fifteen the driver mints, and nothing else.
// ---------------------------------------------------------------------------

/// The automatic check was refused by the policy or the client.
pub const DEFER_CHECK_REFUSED: &str = "check-refused";
/// The channel publishes nothing newer than the running system.
pub const DEFER_NO_NEWER_RELEASE: &str = "no-newer-release";
/// The automatic fetch was refused by the policy or the client.
pub const DEFER_FETCH_REFUSED: &str = "fetch-refused";
/// The device does not believe its clock, and a maintenance window is UTC
/// wall-clock (PLAN-071 §7).
pub const DEFER_CLOCK_UNTRUSTED: &str = "clock-untrusted";
/// No configured maintenance window is open.
pub const DEFER_OUTSIDE_WINDOW: &str = "outside-window";
/// The native backend did not answer the deployment query, so "nothing is pending" is not a fact
/// this pass may assume.
pub const DEFER_DEPLOYMENT_STATUS_UNKNOWN: &str = "deployment-status-unknown";
/// A slot is already installed and waiting for its first boot.
pub const DEFER_REBOOT_PENDING: &str = "reboot-pending";
/// The `/mos/updates` workspace refused the pre-install re-check.
pub const DEFER_WORKSPACE_UNREADY: &str = "workspace-unready";
/// The pre-install re-check failed.
pub const DEFER_RECHECK_FAILED: &str = "recheck-failed";
/// The pre-install re-check was refused by the policy or the client.
pub const DEFER_RECHECK_REFUSED: &str = "recheck-refused";
/// The re-check names a different bundle than the staged one, which is
/// superseded rather than provably withdrawn (PLAN-071 §5).
pub const DEFER_SUPERSEDED: &str = "superseded";
/// `InstallUpdate` refused: the window, the path rule or the storage
/// reservation.
pub const DEFER_INSTALL_REFUSED: &str = "install-refused";
/// The safe-to-reboot gate is closed. Automation defers rather than arming the
/// override; there is no method on its route trait that could arm one.
pub const DEFER_REBOOT_GATE_CLOSED: &str = "reboot-gate-closed";

/// PLAN-071 §2's deferral vocabulary, the whole set.
///
/// Public, unlike [`WORKSPACE_KINDS`], because one caller has to read the
/// whole set rather than one word: this module's rule is that *a code that no
/// test can produce is a code nobody has seen*, and for these fifteen that was
/// unenforceable while `AutoDriver` could not be ticked in a test.
/// [`crate::update_auto::Cadence`] closed that, and `update_auto`'s
/// `every_deferral_reason_the_driver_can_mint_is_reachable` now drives a pass
/// for each word here and fails on a sixteenth nothing produces. A list
/// written out in the test would only assert what its author remembered.
pub const DEFERRALS: [&str; 13] = [
    DEFER_CHECK_REFUSED,
    DEFER_NO_NEWER_RELEASE,
    DEFER_FETCH_REFUSED,
    DEFER_CLOCK_UNTRUSTED,
    DEFER_OUTSIDE_WINDOW,
    DEFER_DEPLOYMENT_STATUS_UNKNOWN,
    DEFER_REBOOT_PENDING,
    DEFER_WORKSPACE_UNREADY,
    DEFER_RECHECK_FAILED,
    DEFER_RECHECK_REFUSED,
    DEFER_SUPERSEDED,
    DEFER_INSTALL_REFUSED,
    DEFER_REBOOT_GATE_CLOSED,
];

/// A deferral reason as a code.
///
/// The driver mints these from the constants above, so in production this
/// function is the identity. It exists for the seam rather than for the
/// driver: `AutoRoutes::defer` takes a `&str`, and a second driver — or a
/// misspelling in this one — must not be able to publish a sixteenth reason
/// into a set `openapi.json` says has fifteen. An unmatched word is recorded
/// as `unknown` and logged, which is a visible deferral with a lost name
/// rather than an invisible new vocabulary.
pub fn deferral_code(reason: &str) -> &'static str {
    DEFERRALS
        .into_iter()
        .find(|known| *known == reason)
        .unwrap_or(UNKNOWN)
}

// ---------------------------------------------------------------------------
// `update.lifecycle.reboot_gate.codes`: THE HEALTH PATH. Why the device may
// not reboot right now, one code per reason, same order.
// ---------------------------------------------------------------------------

/// The deployment installer is writing artifacts. No override lifts this one.
pub const GATE_INSTALL_IN_FLIGHT: &str = "install-in-flight";

/// A component reported a status the policy's `blockingStatuses` names.
///
/// One code for every component rather than one per component: a code is a
/// closed set and `ReportHealth`'s component names are an open one — any unit
/// on the device may report — so a per-component code would be the open
/// vocabulary again with extra steps. WHICH component and WHY is in the
/// `reasons` entry beside it, which is the free-text half this pair exists to
/// stop being the only half.
pub const GATE_HEALTH_BLOCKING: &str = "health-blocking";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mapping_answers_from_its_own_set() {
        for text in ["", "arbitrary message", "unexpected refusal", "😀"] {
            assert_eq!(deferral_code(text), UNKNOWN);
        }
        assert_eq!(deferral_code(UNKNOWN), UNKNOWN);
    }

    #[test]
    fn every_declared_code_maps_to_itself() {
        for reason in DEFERRALS {
            assert_eq!(deferral_code(reason), reason);
        }
    }

    // Every code is a distinct kebab-case word. `unknown` in particular is not
    // in any set: a producer that could mint it would make "this failure is
    // not in the vocabulary" indistinguishable from a failure that is.
    #[test]
    fn codes_are_distinct_kebab_case_words_and_none_is_unknown() {
        let mut all: Vec<&str> = Vec::new();
        all.push(WORKSPACE_PROBE_FAILED);
        all.extend(DEFERRALS);
        all.extend([
            CLIENT_SPAWN_FAILED,
            CLIENT_EXIT_FAILURE,
            CLIENT_OUTPUT_UNPARSEABLE,
            UNVERIFIED_DEPLOYMENT_PATH,
            NO_SOURCE_CONFIGURED,
            POLICY_NOT_LOADED,
            REFUSED_POLICY_INVALID,
            REFUSED_NETWORK_OFFLINE,
            REFUSED_NETWORK_METERED,
            REFUSED_CLIENT_UNAVAILABLE,
            NOTE_DEPLOYMENT_DISCARDED,
            GATE_INSTALL_IN_FLIGHT,
            GATE_HEALTH_BLOCKING,
        ]);
        for code in &all {
            assert_ne!(*code, UNKNOWN, "no producer may mint the fallback");
            assert!(
                !code.is_empty() && code.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'),
                "`{code}` is not a kebab-case word"
            );
        }
        let mut seen = all.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), all.len(), "two codes share a spelling: {all:?}");
    }

    // THE VOCABULARY IS API SURFACE, checked rather than asserted in prose.
    //
    // `apid` serves this document verbatim at `GET /api/v1/update`, and
    // `openapi.json` is what a generated client learns from. A code a client
    // cannot look up is a code that client will treat as an opaque string,
    // which is the defect this module exists to remove — so every word a
    // producer here can mint has to be IN the published description of the
    // route that serves it, not merely somewhere in the file.
    //
    // The committed document is read rather than the running generator: it is
    // the artifact clients are given, and `apid`'s own byte-equality test is
    // what keeps it equal to what the tree generates.
    #[test]
    fn every_code_is_published_in_the_route_that_serves_it() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../../mica-apid/openapi.json"))
                .expect("the committed openapi document is JSON");
        let described = |path: &str| -> String {
            document
                .pointer(&format!(
                    "/paths/{}/get/responses/200/description",
                    path.replace('/', "~1")
                ))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| panic!("{path} has no documented 200"))
                .to_string()
        };
        let update = described("/api/v1/update");
        let mut published: Vec<&str> = Vec::new();
        published.push(WORKSPACE_PROBE_FAILED);
        published.extend(DEFERRALS);
        published.extend([
            UNKNOWN,
            CLIENT_SPAWN_FAILED,
            CLIENT_EXIT_FAILURE,
            CLIENT_OUTPUT_UNPARSEABLE,
            UNVERIFIED_DEPLOYMENT_PATH,
            NO_SOURCE_CONFIGURED,
            POLICY_NOT_LOADED,
            REFUSED_POLICY_INVALID,
            REFUSED_NETWORK_OFFLINE,
            REFUSED_NETWORK_METERED,
            REFUSED_CLIENT_UNAVAILABLE,
            NOTE_DEPLOYMENT_DISCARDED,
            GATE_INSTALL_IN_FLIGHT,
            GATE_HEALTH_BLOCKING,
        ]);
        for code in published {
            assert!(
                update.contains(&format!("`{code}`")),
                "`{code}` is not published in GET /api/v1/update; regenerate \
                 apid/openapi.json after naming it in the route's rustdoc"
            );
        }
        // The health path's own three, on its own route.
        let health = described("/api/v1/health");
        for code in ["micad_unreachable", "micad_timeout", "micad_bad_answer"] {
            assert!(
                health.contains(&format!("`{code}`")),
                "`{code}` is not published in GET /api/v1/health"
            );
        }
    }
}

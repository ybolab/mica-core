//! The closed failure vocabulary the update and health paths report as
//! (PLAN-076 B4).
//!
//! WHY A VOCABULARY AT ALL. Every failure on this path used to reach the wire
//! as a sentence — the client's stderr tail, RAUC's `LastError`, a refusing
//! rule in words. A sentence is unusable to anything that is not a human
//! reading one device: a fleet dashboard that groups failures, an alert that
//! fires on one class, a support tool that looks the answer up all have to
//! match on the text, and the day the text changes upstream every one of them
//! goes quiet without going red. PLAN-037 calls that out as a 1.0 blocker in
//! exactly those terms.
//!
//! **The closed half is the load-bearing half.** A code set with a
//! "…otherwise pass the original string through" arm is not a vocabulary, it
//! is the same open set with a nicer name: a consumer that saw a code once has
//! no way to know whether the next value is a code or a sentence, so it goes
//! back to matching on text. So every mapping here is TOTAL and its fallback
//! is [`UNKNOWN`] — never the input. The input is not lost, it is *moved*: the
//! text goes to the journal at the mapping site, and the document keeps the
//! human-readable member beside the code (`reason`, `detail`, `error`) for the
//! operator reading one device. What no consumer gets is a *code* it has never
//! seen.
//!
//! **Codes are minted where the failure is constructed, not recognised later
//! from its words**, wherever mos owns the failure — matching mos's own
//! strings would only move the fragility inside the daemon. The two mappings
//! that do classify text ([`workspace_code`], [`rauc_error_code`]) are the two
//! places the text comes from a process this daemon does not own, and they are
//! where [`UNKNOWN`] is actually reachable.
//!
//! The vocabulary is API surface: `apid` serves this document verbatim at
//! `GET /api/v1/update`, so `openapi.json` states every code below and
//! `docs/design/updates.md` §1.2 is where a support engineer looks one up.
//! Adding a code is a published-contract change; a code that no test can
//! produce is a code nobody has seen.

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

/// `rauc-update` could not be run to completion: spawn failed, or the bound on
/// the subprocess expired. Nothing was learned about the update.
pub const CLIENT_SPAWN_FAILED: &str = "client-spawn-failed";

/// `rauc-update` ran and exited with a failure status that is not one of its
/// contract's ("nothing compatible", "workspace not ready"). The reason beside
/// this code is the client's stderr tail.
pub const CLIENT_EXIT_FAILURE: &str = "client-exit-failure";

/// `rauc-update` exited successfully but did not print what its contract says
/// it prints, so there is no outcome to record. A client/daemon version skew
/// or a client bug; never an update fault.
pub const CLIENT_OUTPUT_UNPARSEABLE: &str = "client-output-unparseable";

/// A fetch named a path that is not a verified bundle directly inside
/// `verified/`. Refused rather than recorded: `docs/design/updates.md` §1.1's
/// "never installable by filename alone" is enforced here, not trusted.
pub const UNVERIFIED_BUNDLE_PATH: &str = "unverified-bundle-path";

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

/// `/mos`, `/mnt/data` or a workspace directory is absent.
pub const WORKSPACE_MOUNT_MISSING: &str = "mount-missing";
/// What is at `/mos` is not the DATA pool.
pub const WORKSPACE_NOT_DATA: &str = "not-data";
/// The pool is there and mounted read-only.
pub const WORKSPACE_READ_ONLY: &str = "read-only";
/// The declared `maxBytes` budget is spent, or free space is below what the
/// acquisition needs.
pub const WORKSPACE_EXHAUSTED: &str = "exhausted";
/// The probe itself could not complete.
pub const WORKSPACE_PROBE_FAILED: &str = "probe-failed";

/// PLAN-061's workspace verdicts, the whole set.
const WORKSPACE_KINDS: [&str; 5] = [
    WORKSPACE_MOUNT_MISSING,
    WORKSPACE_NOT_DATA,
    WORKSPACE_READ_ONLY,
    WORKSPACE_EXHAUSTED,
    WORKSPACE_PROBE_FAILED,
];

/// The workspace verdict `rauc-update` named, as a code.
///
/// **One of the two places [`UNKNOWN`] is genuinely reachable.** The kind is a
/// word a separate binary chose and printed on its exit-3 line, and the parser
/// that reads that line cannot enumerate what a future client might print. A
/// kind outside the set above is therefore reported as `unknown` rather than
/// forwarded: the state (`update-unavailable`) and the `status`
/// (`unavailable`/`degraded`) are still right, which is what refuses the
/// acquisition, and only the word is lost — to the journal, which
/// [`crate::update_lifecycle::parse_unready`] writes it to.
pub fn workspace_code(kind: &str) -> &'static str {
    WORKSPACE_KINDS
        .into_iter()
        .find(|known| *known == kind)
        .unwrap_or(UNKNOWN)
}

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
pub const NOTE_BUNDLE_DISCARDED: &str = "bundle-discarded";
/// An operator lifted a version suppression.
pub const NOTE_SUPPRESSION_CLEARED: &str = "suppression-cleared";

// ---------------------------------------------------------------------------
// `update.lifecycle.deferred.reason`: why the last AUTOMATIC pass did not
// proceed (PLAN-071 §2). The fifteen the driver mints, and nothing else.
// ---------------------------------------------------------------------------

/// The automatic check was refused by the policy or the client.
pub const DEFER_CHECK_REFUSED: &str = "check-refused";
/// The channel publishes nothing newer than the running system.
pub const DEFER_NO_NEWER_RELEASE: &str = "no-newer-release";
/// The candidate's version is one this device rolled back from (PLAN-071 §6).
pub const DEFER_VERSION_SUPPRESSED: &str = "version-suppressed";
/// The suppression store exists and could not be read — which is never read as
/// "nothing is suppressed", because that reading is the reboot loop §6 exists
/// to break.
pub const DEFER_SUPPRESSION_UNREADABLE: &str = "suppression-unreadable";
/// The automatic fetch was refused by the policy or the client.
pub const DEFER_FETCH_REFUSED: &str = "fetch-refused";
/// The device does not believe its clock, and a maintenance window is UTC
/// wall-clock (PLAN-071 §7).
pub const DEFER_CLOCK_UNTRUSTED: &str = "clock-untrusted";
/// No configured maintenance window is open.
pub const DEFER_OUTSIDE_WINDOW: &str = "outside-window";
/// RAUC did not answer the slot query, so "nothing is pending" is not a fact
/// this pass may assume.
pub const DEFER_SLOT_STATUS_UNKNOWN: &str = "slot-status-unknown";
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
pub const DEFERRALS: [&str; 15] = [
    DEFER_CHECK_REFUSED,
    DEFER_NO_NEWER_RELEASE,
    DEFER_VERSION_SUPPRESSED,
    DEFER_SUPPRESSION_UNREADABLE,
    DEFER_FETCH_REFUSED,
    DEFER_CLOCK_UNTRUSTED,
    DEFER_OUTSIDE_WINDOW,
    DEFER_SLOT_STATUS_UNKNOWN,
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

/// RAUC is mid-write on the other slot. No override lifts this one.
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

// ---------------------------------------------------------------------------
// `update.last_error_code` and `update.install.error_code`: RAUC's own words
// for a failed install, classified.
// ---------------------------------------------------------------------------

/// RAUC refused the bundle's signature.
///
/// The needle is RAUC's, measured rather than assumed: PLAN-078 §5 records it
/// from five separate refusals (expired signer, not-yet-valid signer, foreign
/// CA, path-length violation, missing intermediate) and
/// `tests/rauc-trust-negative-test.sh` asserts on the same prefix, so this
/// classification is pinned by a test that does not belong to this module.
pub const RAUC_SIGNATURE_INVALID: &str = "signature-invalid";

/// RAUC's message for the signature refusals, verbatim and measured — see
/// [`RAUC_SIGNATURE_INVALID`]. The rest of the sentence names which check
/// failed and is deliberately not matched on: the class is what a fleet
/// groups by, and PLAN-078 §5 is explicit that the tail cannot distinguish a
/// real expiry from a wrong clock anyway.
const RAUC_SIGNATURE_NEEDLE: &str = "signature verification failed";

/// RAUC's error as a code.
///
/// **The second place [`UNKNOWN`] is genuinely reachable, and the honest one.**
/// RAUC's `LastError` is RAUC's vocabulary, not this project's: mos cannot
/// enumerate it and must not pretend to. So exactly one class is claimed here
/// — the one this repository has measured and holds a test on — and every
/// other install failure is reported as `unknown` with RAUC's sentence beside
/// it in `error` and in the journal.
///
/// That is the correct shape rather than a gap to fill later. The set grows
/// when a failure is *measured*, one code per measurement, and a set grown any
/// other way is a set that claims to have classified a failure nobody has
/// seen.
pub fn rauc_error_code(error: &str) -> &'static str {
    if error.contains(RAUC_SIGNATURE_NEEDLE) {
        return RAUC_SIGNATURE_INVALID;
    }
    UNKNOWN
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both classifiers are total and neither can answer with its input. The
    // property, not three examples of it: an arbitrary sentence must come back
    // as one of the fixed words, and `unknown` in particular must not be
    // spellable by a caller.
    #[test]
    fn every_mapping_answers_from_its_own_set() {
        let foreign = [
            "",
            "read only",
            "rauc-update: degraded read-only: ro",
            "signature verification FAILED",
            "no-newer-releases",
            "\u{1f600}",
        ];
        for text in foreign {
            let workspace = workspace_code(text);
            assert!(
                workspace == UNKNOWN || WORKSPACE_KINDS.contains(&workspace),
                "workspace_code({text:?}) answered {workspace}"
            );
            assert_ne!(
                workspace, text,
                "workspace_code must never answer with its input"
            );
            let deferral = deferral_code(text);
            assert!(
                deferral == UNKNOWN || DEFERRALS.contains(&deferral),
                "deferral_code({text:?}) answered {deferral}"
            );
            assert_ne!(
                deferral, text,
                "deferral_code must never answer with its input"
            );
            let rauc = rauc_error_code(text);
            assert!(
                rauc == UNKNOWN || rauc == RAUC_SIGNATURE_INVALID,
                "rauc_error_code({text:?}) answered {rauc}"
            );
        }
        // The literal `unknown` is the one input whose answer equals it, and
        // that is the fallback firing rather than the input being forwarded --
        // which is exactly why no producer is allowed to mint the word (see
        // `codes_are_distinct_kebab_case_words_and_none_is_unknown`), so a
        // reader can never tell the two apart and never has to.
        assert_eq!(workspace_code(UNKNOWN), UNKNOWN);
        assert_eq!(deferral_code(UNKNOWN), UNKNOWN);
    }

    // The identity half: every word the producers mint survives its mapping.
    // Without this the fallback above would be satisfied by a function that
    // answers `unknown` to everything.
    #[test]
    fn every_declared_code_maps_to_itself() {
        for kind in WORKSPACE_KINDS {
            assert_eq!(workspace_code(kind), kind);
        }
        for reason in DEFERRALS {
            assert_eq!(deferral_code(reason), reason);
        }
    }

    // PLAN-071 §2 says fifteen and `docs/design/updates.md` §3.2 lists
    // fifteen; a sixteenth added to the array without a plan amendment fails
    // here rather than in a reader's client.
    #[test]
    fn the_sets_are_the_sizes_the_documents_state() {
        assert_eq!(DEFERRALS.len(), 15);
        assert_eq!(WORKSPACE_KINDS.len(), 5);
    }

    // Every code is a distinct kebab-case word. `unknown` in particular is not
    // in any set: a producer that could mint it would make "this failure is
    // not in the vocabulary" indistinguishable from a failure that is.
    #[test]
    fn codes_are_distinct_kebab_case_words_and_none_is_unknown() {
        let mut all: Vec<&str> = Vec::new();
        all.extend(WORKSPACE_KINDS);
        all.extend(DEFERRALS);
        all.extend([
            CLIENT_SPAWN_FAILED,
            CLIENT_EXIT_FAILURE,
            CLIENT_OUTPUT_UNPARSEABLE,
            UNVERIFIED_BUNDLE_PATH,
            NO_SOURCE_CONFIGURED,
            POLICY_NOT_LOADED,
            REFUSED_POLICY_INVALID,
            REFUSED_NETWORK_OFFLINE,
            REFUSED_NETWORK_METERED,
            REFUSED_CLIENT_UNAVAILABLE,
            NOTE_BUNDLE_DISCARDED,
            NOTE_SUPPRESSION_CLEARED,
            GATE_INSTALL_IN_FLIGHT,
            GATE_HEALTH_BLOCKING,
            RAUC_SIGNATURE_INVALID,
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
            serde_json::from_str(include_str!("../../apid/openapi.json"))
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
        published.extend(WORKSPACE_KINDS);
        published.extend(DEFERRALS);
        published.extend([
            UNKNOWN,
            CLIENT_SPAWN_FAILED,
            CLIENT_EXIT_FAILURE,
            CLIENT_OUTPUT_UNPARSEABLE,
            UNVERIFIED_BUNDLE_PATH,
            NO_SOURCE_CONFIGURED,
            POLICY_NOT_LOADED,
            REFUSED_POLICY_INVALID,
            REFUSED_NETWORK_OFFLINE,
            REFUSED_NETWORK_METERED,
            REFUSED_CLIENT_UNAVAILABLE,
            NOTE_BUNDLE_DISCARDED,
            NOTE_SUPPRESSION_CLEARED,
            GATE_INSTALL_IN_FLIGHT,
            GATE_HEALTH_BLOCKING,
            RAUC_SIGNATURE_INVALID,
        ]);
        for code in published {
            assert!(
                update.contains(&format!("`{code}`")),
                "`{code}` is not published in GET /api/v1/update; regenerate \
                 pkgs/mosd/apid/openapi.json after naming it in the route's rustdoc"
            );
        }
        // The health path's own three, on its own route.
        let health = described("/api/v1/health");
        for code in ["mosd_unreachable", "mosd_timeout", "mosd_bad_answer"] {
            assert!(
                health.contains(&format!("`{code}`")),
                "`{code}` is not published in GET /api/v1/health"
            );
        }
    }

    // The one classification `rauc_error_code` claims, against RAUC's own
    // sentence as PLAN-078 §5 recorded it.
    #[test]
    fn a_measured_rauc_refusal_is_the_code_it_was_measured_as() {
        for measured in [
            "rauc install failed: signature verification failed: Verify error: certificate has expired",
            "signature verification failed: Verify error: certificate is not yet valid",
            "signature verification failed: Verify error: path length constraint exceeded",
            "signature verification failed: Verify error: unable to get local issuer certificate",
        ] {
            assert_eq!(
                rauc_error_code(measured),
                RAUC_SIGNATURE_INVALID,
                "{measured}"
            );
        }
        // An unmeasured failure is `unknown`, not a guess and not its text.
        assert_eq!(
            rauc_error_code("rauc install failed: Compatible mismatch"),
            UNKNOWN
        );
    }
}

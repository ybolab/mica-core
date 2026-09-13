//! Start-up bundle discovery and §6.1's compatibility re-check.
//!
//! `docs/design/api.md` §6.1's detection half, and §8.2 phase 4's second
//! sanctioned local install path — *"an operator places a tree on the device
//! and calls an activate operation, or apid picks up a staged directory."*
//! There is no upload route here and no archive dependency; that is phase 5.
//!
//! Nothing in this module returns an error, and nothing in it may `?`,
//! `unwrap`, `expect` or panic its way out. §6.1 requires discovery to run
//! after the listeners bind and after `APID_LISTENING` is printed, and every
//! outcome — an unreadable disk, a garbage manifest, an absent `/mica/ui` — to
//! be a [`BundleState`] the daemon holds. `main` propagates every earlier
//! start-up step with `?` and the unit is `Restart=on-failure`
//! (`dist/apid.service`), so an error out of here is a crash loop with
//! no listener bound.
//!
//! Two of §6.1's five classes are detected here, and both are detected at
//! start-up rather than per request:
//!
//! - **Class 3**, a malformed or half-written bundle: the digest recorded at
//!   activation, re-checked. §6.1 states the cost — *"a corruption introduced
//!   mid-life is detected at the next restart, not immediately."*
//! - **Class 5**, a UI that renders and cannot talk to any API version apid
//!   serves. The relation is set intersection and the trigger is an empty
//!   intersection and nothing else — never equality with the served set's
//!   `current` member. A bundle that matches only the outgoing major
//!   stays active, which is what §2.1's dual-major recommendation exists for.
//!
//! Classes 1, 2 and 4 belong to the asset router, which sees them per request
//! at the cost of a syscall; class 1 in particular is *"the shipped state of
//! every device and it must not be logged as one"* — so the no-bundle outcome
//! here is logged at `INFO` as normal operation.

use std::fmt;

use crate::bundle::{CompatCheck, Store};

/// §2.1's **served set**: the `versions` array of `GET /api/versions`.
///
/// §2.1 fixes the shape — an array, because the answer can legitimately have
/// more than one member, with [`CURRENT_API_VERSION`] always among them. This
/// constant is that array today.
///
/// `GET /api/versions` is declared (`routes::api_router`) and serves this
/// constant rather than a second copy of it; it is unauthenticated by §2.1.
/// Every path under `/api/` that is not a declared route still 404s.
pub const SERVED_API_VERSIONS: &[&str] = &["v1"];

/// §2.1's `current`: the member a client with no preference should use.
///
/// Always a member of [`SERVED_API_VERSIONS`] — §2.1 requires it and a test
/// asserts it. §6.1's check does not compare against this value; it is logged
/// so that a deactivation can be read back, and read by nothing else.
pub const CURRENT_API_VERSION: &str = "v1";

/// Why start-up removed the active pointer. Both are §6.1 deactivations and
/// both can hold at once, so the state carries a list rather than one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// §6.1 class 3: the tree no longer hashes to the digest recorded at
    /// activation. A tree that cannot be hashed at all counts as a mismatch,
    /// because it is one.
    DigestMismatch,
    /// §6.1 class 5: the declared range and the served set have no member in
    /// common. Both sets are carried so the log line can name both — that is
    /// what makes a correct deactivation distinguishable from an incorrect one
    /// after the fact.
    Incompatible {
        /// The manifest's declared API versions.
        declared: Vec<String>,
        /// The served set they were compared against.
        served: Vec<String>,
    },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DigestMismatch => f.write_str(
                "the tree no longer matches the digest recorded at activation (§6.1 class 3)",
            ),
            Self::Incompatible { declared, served } => write!(
                f,
                "declared API versions {declared:?} have no member in common with the served set {served:?} (§6.1 class 5)"
            ),
        }
    }
}

/// What start-up discovery concluded — a state the daemon holds, never an
/// error it returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleState {
    /// No custom bundle is active. §6.1 class 1: the shipped state of every
    /// device, and not an error.
    BuiltIn,
    /// A bundle is active and survived both re-checks.
    Active {
        /// The generation `current` resolves to.
        generation: u64,
        /// What the compatibility check decided, or that it could not run.
        compat: CompatCheck,
    },
    /// A bundle was active and start-up took the pointer down.
    Deactivated {
        /// The generation that was active.
        generation: u64,
        /// Every §6.1 trigger that fired, in class order.
        reasons: Vec<Reason>,
        /// Whether the pointer actually came down. `false` means the reasons
        /// hold but the removal failed, which is a worse state than either and
        /// is therefore not collapsed into the same word.
        removed: bool,
    },
    /// The store could not be evaluated at all. The asset router reads the
    /// same store per request and answers with the built-in UI when it cannot
    /// read it, so this is a degraded state and not a fatal one.
    Unavailable {
        /// What went wrong, for the operator reading the log.
        why: String,
    },
}

impl fmt::Display for BundleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuiltIn => f.write_str(crate::bundle::NO_CUSTOM_BUNDLE),
            Self::Active { generation, compat } => {
                write!(f, "generation {generation} is active: {}", describe(compat))
            }
            Self::Deactivated {
                generation,
                reasons,
                removed,
            } => {
                let verb = if *removed {
                    "was deactivated at start-up"
                } else {
                    "should have been deactivated at start-up and the pointer is still in place"
                };
                let why = reasons
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ");
                write!(f, "generation {generation} {verb}: {why}")
            }
            Self::Unavailable { why } => {
                write!(f, "the bundle store could not be evaluated: {why}")
            }
        }
    }
}

/// `main`'s entry point, and the whole of §6.1's start-up half.
///
/// Called after the listeners bind and after `APID_LISTENING` is printed.
/// It takes no `Result` out and it takes no `Result` back in: the return type
/// has no error variant, so `main` cannot propagate one by accident.
///
/// The evaluation runs on the blocking pool for two reasons and both matter.
/// It re-hashes a whole tree, which does not belong on an async worker; and a
/// panic raised anywhere beneath it is delivered as a `JoinError` rather than
/// unwinding `main`, so the "a bundle cannot stop apid from listening"
/// property survives a bug in code this module only calls.
pub async fn discover(store: Store, audit: std::sync::Arc<crate::audit::Audit>) -> BundleState {
    join(tokio::task::spawn_blocking(move || run(&store, &audit)).await)
}

/// Turn the blocking task's join result into a state. A `JoinError` here means
/// something below this module panicked; §6.1 permits exactly one response to
/// that and it is not re-panicking.
fn join(joined: Result<BundleState, tokio::task::JoinError>) -> BundleState {
    match joined {
        Ok(state) => state,
        Err(err) => {
            let why = format!("bundle discovery did not complete: {err}");
            tracing::error!(error = %err, "bundle discovery did not complete; the built-in UI is what the asset router will serve");
            BundleState::Unavailable { why }
        }
    }
}

/// The synchronous body, against the served set this binary actually serves.
fn run(store: &Store, audit: &crate::audit::Audit) -> BundleState {
    evaluate(store, audit, SERVED_API_VERSIONS)
}

/// The body against an arbitrary served set.
///
/// The set is a parameter so that the A/B case §6.1 is written for — a bundle
/// activated against one image's API and re-checked against the next image's —
/// is reachable in a test. `run` is the only caller that chooses it, and it
/// chooses [`SERVED_API_VERSIONS`].
fn evaluate(store: &Store, audit: &crate::audit::Audit, served: &[&str]) -> BundleState {
    pick_up_staged(store, audit, served);
    recheck(store, served)
}

/// §8.2 phase 4's second local install path. Failure is logged and start-up
/// continues: a staged tree that cannot be activated must not prevent the
/// already-active one from being evaluated.
///
/// A successful pick-up goes to the audit trail as well as the journal: it is
/// the one path that changes which UI the appliance serves without any HTTP
/// request, so access.md §6's "custom-UI activate" event is recorded here.
/// The source is `local` — the trigger is a directory staged on the disk, not
/// a network peer.
fn pick_up_staged(store: &Store, audit: &crate::audit::Audit, served: &[&str]) {
    match store.pick_up_staged(served) {
        Ok(None) => {}
        Ok(Some(activation)) => {
            tracing::info!(
                generation = activation.generation,
                digest = %activation.digest,
                compat = %describe(&activation.compat),
                "picked up a staged UI bundle at start-up"
            );
            // The device acted on what it found on the disk at start-up.
            // Recording it as an operator's would be the exact lie U8's actor
            // field exists to prevent: nobody asked for this one.
            audit.record_as(
                "custom-ui",
                "activated",
                "local",
                micad_settings::ACTOR_DEVICE,
            );
        }
        Err(err) => tracing::warn!(
            error = %format!("{err:#}"),
            "a staged UI bundle could not be activated at start-up; nothing else changed"
        ),
    }
}

/// §6.1 classes 3 and 5 against the active bundle.
fn recheck(store: &Store, served: &[&str]) -> BundleState {
    let recheck = match store.recheck_active(served) {
        Ok(Some(recheck)) => recheck,
        Ok(None) => {
            // §6.1 class 1. INFO, never WARN and never ERROR: this is the
            // shipped state of every device and "it must not be logged as one".
            tracing::info!(
                root = %store.root().display(),
                served = ?served,
                current = CURRENT_API_VERSION,
                "{}",
                crate::bundle::NO_CUSTOM_BUNDLE
            );
            return BundleState::BuiltIn;
        }
        Err(err) => {
            let why = format!("{err:#}");
            tracing::warn!(
                root = %store.root().display(),
                error = %why,
                "the UI bundle store could not be evaluated at start-up; the built-in UI is what the asset router will serve"
            );
            return BundleState::Unavailable { why };
        }
    };
    let generation = recheck.generation;

    let mut reasons = Vec::new();
    if recheck.corrupt() {
        // §6.1 class 3: "deactivate and serve the built-in UI, logging the
        // mismatch".
        tracing::warn!(
            generation,
            "the UI bundle no longer matches the digest recorded at activation; deactivating"
        );
        reasons.push(Reason::DigestMismatch);
    }
    if let CompatCheck::Ran {
        declared,
        served,
        compatible: false,
    } = &recheck.compat
    {
        // §6.1 class 5. Both sets go in the line, because "recording the
        // served set in the log line is what makes the two distinguishable
        // after the fact".
        tracing::warn!(
            generation,
            declared = ?declared,
            served = ?served,
            current = CURRENT_API_VERSION,
            "the UI bundle's declared API versions have no member in common with the served set; deactivating"
        );
        reasons.push(Reason::Incompatible {
            declared: declared.clone(),
            served: served.clone(),
        });
    }

    if reasons.is_empty() {
        tracing::info!(
            generation,
            compat = %describe(&recheck.compat),
            "a custom UI bundle is active"
        );
        return BundleState::Active {
            generation,
            compat: recheck.compat,
        };
    }

    let removed = match store.deactivate() {
        Ok(removed) => removed,
        Err(err) => {
            tracing::error!(
                generation,
                error = %format!("{err:#}"),
                "the UI bundle could not be deactivated; the pointer is still in place"
            );
            false
        }
    };
    BundleState::Deactivated {
        generation,
        reasons,
        removed,
    }
}

/// One line for a compatibility result, including the "could not be checked"
/// case §6.1 requires to be a degradation rather than a rejection.
fn describe(compat: &CompatCheck) -> String {
    match compat {
        CompatCheck::NotRun => {
            "unchecked — the bundle carries no manifest, so nothing could be compared against the served set".to_string()
        }
        CompatCheck::Ran {
            declared,
            served,
            compatible,
        } => format!(
            "declared {declared:?} against the served set {served:?} — {}",
            if *compatible {
                "compatible"
            } else {
                "no member in common"
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fmt::Write as _;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;
    use std::panic::AssertUnwindSafe;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use crate::bundle::Installed;

    /// §2.1's dual-major recommendation, as a served set: two members, and
    /// `current` is the *second* one. Used wherever a test needs the outgoing
    /// major to be distinguishable from `current`.
    const DUAL: [&str; 2] = ["v1", "v2"];
    /// `current` for [`DUAL`]. The outgoing major is `v1`.
    const DUAL_CURRENT: &str = "v2";

    // Fixtures.

    fn store() -> (tempfile::TempDir, Store) {
        fresh()
    }

    /// A journal-only audit sink, for the tests that drive `discover` itself.
    fn journal_audit() -> Arc<crate::audit::Audit> {
        Arc::new(crate::audit::Audit::journal_only())
    }

    /// The same fixture under a second name, for the tests that need a second
    /// store while the first is still bound to `store`.
    fn fresh() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path().join("ui"));
        (dir, store)
    }

    /// Stage a minimal valid tree: one `index.html` and one asset.
    fn stage(store: &Store, generation: u64) -> PathBuf {
        let staging = store.staging_dir(generation);
        fs::create_dir_all(staging.join("assets")).expect("create staging");
        fs::write(staging.join("index.html"), b"<!doctype html>").expect("write index");
        fs::write(staging.join("assets/app.js"), b"console.log(1)").expect("write asset");
        staging
    }

    fn write_manifest(staging: &Path, api_versions: &[&str]) {
        let manifest = serde_json::json!({
            "name": "demo",
            "version": "1.2.3",
            "immutableDir": "assets",
            "apiVersions": api_versions,
        });
        fs::write(
            staging.join("mos-ui.json"),
            serde_json::to_vec(&manifest).expect("serialise manifest"),
        )
        .expect("write manifest");
    }

    /// Activate a bundle declaring `declared`, against `served`. `served` must
    /// intersect `declared` or activation refuses — which is the activation
    /// half of class 5 and is `bundle.rs`'s test, not this module's.
    fn activate(store: &Store, generation: u64, declared: &[&str], served: &[&str]) {
        let staging = stage(store, generation);
        write_manifest(&staging, declared);
        store
            .activate(generation, served)
            .expect("activation must succeed");
    }

    // Log capture.

    /// A subscriber that is interested in everything and does nothing with it,
    /// registered once for the life of the test binary.
    ///
    /// `tracing` caches each callsite's `Interest` the first time that
    /// callsite is reached, computing it from the dispatchers registered at
    /// that instant, and a cached `never` is never reconsidered. With only
    /// scoped subscribers in the process there are instants with none: a
    /// thread that reaches a callsite while holding no dispatcher of its own
    /// gets `Interest::never()` cached for it, and every later event at that
    /// callsite is dropped before any subscriber sees it -- including a
    /// [`capture`] in flight on another thread, which then reads an empty log
    /// about code that logged correctly. Keeping one always-interested
    /// dispatcher registered for good makes `never` unreachable. It changes
    /// nothing about where events go: `with_default` still routes them to the
    /// capturing subscriber on the capturing thread, and they reach this one
    /// only on threads that are not capturing, which drop them.
    struct Interested;

    impl tracing::Subscriber for Interested {
        fn register_callsite(
            &self,
            _: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::always()
        }
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `f` with every `tracing` event going into a string.
    fn capture<T>(f: impl FnOnce() -> T) -> (T, String) {
        // Before the scoped subscriber and before `f`, so that no callsite
        // reached from here on can be cached as uninteresting.
        static FLOOR: std::sync::Once = std::sync::Once::new();
        FLOOR.call_once(|| {
            let _ = tracing::subscriber::set_global_default(Interested);
        });

        let sink = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let out = tracing::subscriber::with_default(subscriber, f);
        let bytes = sink.0.lock().expect("log buffer").clone();
        (out, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The named-set assertion, applied to a log line: declare the fragments
    /// expected by identity, observe which are present, and name what is
    /// missing. Never a count.
    fn names(log: &str, expected: &[&str], what: &str) {
        let missing: Vec<&str> = expected
            .iter()
            .copied()
            .filter(|fragment| !log.contains(fragment))
            .collect();
        assert!(
            missing.is_empty(),
            "{what}: the log does not name {missing:?}\n--- log ---\n{log}"
        );
    }

    fn active(store: &Store) -> Option<u64> {
        store.active_generation().expect("active generation")
    }

    // The served-set constant.

    /// §2.1 fixes the shape of the served set: an array, with `current` always
    /// a member. Declared by identity and diffed, never counted.
    #[test]
    fn the_served_set_is_the_array_2_1_specifies() {
        let expected: BTreeSet<&str> = ["v1"].into_iter().collect();
        let observed: BTreeSet<&str> = SERVED_API_VERSIONS.iter().copied().collect();
        let missing: Vec<&&str> = expected.difference(&observed).collect();
        let unexpected: Vec<&&str> = observed.difference(&expected).collect();
        assert!(
            missing.is_empty() && unexpected.is_empty(),
            "served set: missing {missing:?}, unexpected {unexpected:?}"
        );
        assert_eq!(
            observed.len(),
            SERVED_API_VERSIONS.len(),
            "the served set carries a duplicate: {SERVED_API_VERSIONS:?}"
        );
        assert!(
            SERVED_API_VERSIONS.contains(&CURRENT_API_VERSION),
            "§2.1: `current` is always a member of `versions`, but {CURRENT_API_VERSION:?} is not in {SERVED_API_VERSIONS:?}"
        );
    }

    /// The constant is not merely defined: `run` feeds it to the class-5
    /// check. A bundle activated against a served set this binary does **not**
    /// serve is deactivated by `run`, and the log names the constant's members
    /// as the set it was compared against.
    #[test]
    fn run_reads_the_served_set_constant() {
        let (_dir, store) = store();
        // Activated against a served set of ["v0"] — an earlier image. This
        // binary serves SERVED_API_VERSIONS, which does not contain it.
        activate(&store, 1, &["v0"], &["v0"]);
        assert_eq!(active(&store), Some(1));

        let (state, log) = capture(|| run(&store, &crate::audit::Audit::journal_only()));
        assert!(
            matches!(state, BundleState::Deactivated { generation: 1, .. }),
            "expected a deactivation, got {state}"
        );
        names(
            &log,
            &[r#"declared=["v0"]"#, r#"served=["v1"]"#],
            "the class-5 line must name the constant it compared against",
        );
        assert_eq!(active(&store), None);

        // And the other direction: a bundle declaring the constant's own
        // member survives `run`, so the constant is not being ignored.
        let (_dir2, keeper) = fresh();
        activate(&keeper, 1, &["v1"], SERVED_API_VERSIONS);
        assert!(
            matches!(
                run(&keeper, &crate::audit::Audit::journal_only()),
                BundleState::Active { generation: 1, .. }
            ),
            "a bundle declaring the served member must stay active"
        );
        assert_eq!(active(&keeper), Some(1));
    }

    // §6.1 class 5.

    /// A non-empty intersection is not a deactivation trigger, however partial.
    #[test]
    fn an_intersecting_range_stays_active() {
        let (_dir, store) = store();
        activate(&store, 1, &["v1", "v2", "v3"], &DUAL);
        let (state, log) =
            capture(|| evaluate(&store, &crate::audit::Audit::journal_only(), &DUAL));

        let BundleState::Active { generation, compat } = state else {
            panic!("expected the bundle to stay active, got {state}");
        };
        assert_eq!(generation, 1);
        let CompatCheck::Ran {
            declared,
            served,
            compatible,
        } = compat
        else {
            panic!("expected the check to have run");
        };
        assert!(compatible);
        // Named set, not a count: *which* members intersected.
        let intersection: BTreeSet<&str> = declared
            .iter()
            .map(String::as_str)
            .filter(|v| served.iter().any(|s| s == v))
            .collect();
        let expected: BTreeSet<&str> = ["v1", "v2"].into_iter().collect();
        assert_eq!(
            intersection, expected,
            "declared {declared:?} against served {served:?}"
        );
        assert_eq!(active(&store), Some(1));
        assert!(
            !log.contains("deactivat"),
            "nothing may be deactivated\n{log}"
        );
    }

    /// §2.1 recommends serving the outgoing major alongside the new one for
    /// one image generation, and §6.1 says an escape hatch that fires on the
    /// wrong condition is worse than one that does not exist. A bundle that matches
    /// only the outgoing major must stay active — equality against `current`
    /// would remove exactly the bundles the recommendation exists to protect.
    #[test]
    fn a_bundle_matching_only_the_outgoing_major_stays_active() {
        let (_dir, store) = store();
        activate(&store, 1, &["v1"], &DUAL);

        let state = evaluate(&store, &crate::audit::Audit::journal_only(), &DUAL);
        let BundleState::Active { generation, compat } = state else {
            panic!("a bundle matching the outgoing major must stay active, got {state}");
        };
        assert_eq!(generation, 1);
        let CompatCheck::Ran {
            declared, served, ..
        } = &compat
        else {
            panic!("expected the check to have run");
        };

        // The intersection is exactly {v1} -- the outgoing major, by name.
        let intersection: BTreeSet<&str> = declared
            .iter()
            .map(String::as_str)
            .filter(|v| served.iter().any(|s| s == v))
            .collect();
        assert_eq!(intersection, ["v1"].into_iter().collect::<BTreeSet<&str>>());

        // And the control that makes the test mean what it claims: `current`
        // is *not* in the declared range, so an implementation written as
        // equality-with-`current` would have deactivated this bundle.
        assert!(
            !declared.iter().any(|v| v == DUAL_CURRENT),
            "the fixture is wrong: {declared:?} contains `current` {DUAL_CURRENT:?}, so this test could pass under an equality check"
        );
        assert!(served.iter().any(|v| v == DUAL_CURRENT));
        assert_eq!(active(&store), Some(1));
    }

    /// §6.1's sole class-5 trigger: an empty intersection. This is the A/B
    /// case — activated against the old image's API, re-checked against the
    /// new image's, which is *"the only moment at which anything on the device
    /// is in a position to notice."*
    #[test]
    fn an_empty_intersection_deactivates_and_the_log_names_both_sets() {
        let (_dir, store) = store();
        activate(&store, 1, &["v1"], &["v1"]);
        assert_eq!(active(&store), Some(1));

        // The A/B update: this slot serves v2 only.
        let (state, log) =
            capture(|| evaluate(&store, &crate::audit::Audit::journal_only(), &["v2"]));

        let BundleState::Deactivated {
            generation,
            reasons,
            removed,
        } = state
        else {
            panic!("an empty intersection must deactivate, got {state}");
        };
        assert_eq!(generation, 1);
        assert!(removed);
        assert_eq!(
            reasons,
            vec![Reason::Incompatible {
                declared: vec!["v1".to_string()],
                served: vec!["v2".to_string()],
            }]
        );
        names(
            &log,
            &[r#"declared=["v1"]"#, r#"served=["v2"]"#],
            "§6.1 requires both the declared range and the served set in the line",
        );
        assert_eq!(active(&store), None, "the pointer must be gone");
        assert!(
            store.bundle_dir(1).is_dir(),
            "deactivate removes the pointer, never the tree"
        );
    }

    /// §6.1: "a bundle with no manifest cannot be checked, so the correct
    /// behaviour is to activate it and record that it was activated
    /// unchecked." Degradation, not rejection.
    #[test]
    fn a_bundle_with_no_manifest_stays_active_and_is_recorded_unchecked() {
        let (_dir, store) = store();
        stage(&store, 1);
        store.activate(1, SERVED_API_VERSIONS).expect("activate");

        let (state, log) = capture(|| run(&store, &crate::audit::Audit::journal_only()));
        assert_eq!(
            state,
            BundleState::Active {
                generation: 1,
                compat: CompatCheck::NotRun
            }
        );
        names(&log, &["unchecked"], "the unchecked state must be named");
        assert_eq!(active(&store), Some(1));

        // And it is recorded on disk as unchecked, not merely reported.
        let Installed::Custom(ui) = store.status().expect("status") else {
            panic!("expected an active bundle");
        };
        assert_eq!(
            ui.recorded.expect("an activation record").compat,
            CompatCheck::NotRun
        );
    }

    // §6.1 class 3.

    /// Class 3's second reachable cause: "an operator writing into `/mica/ui`
    /// over a root shell". Detected at the next restart -- which is this
    /// module -- and not per request.
    #[test]
    fn a_digest_mismatch_deactivates_and_logs() {
        let (_dir, store) = store();
        activate(&store, 1, &["v1"], SERVED_API_VERSIONS);
        fs::write(store.bundle_dir(1).join("planted.js"), b"root shell").expect("plant a file");

        let (state, log) = capture(|| run(&store, &crate::audit::Audit::journal_only()));
        let BundleState::Deactivated {
            generation,
            reasons,
            removed,
        } = state
        else {
            panic!("a digest mismatch must deactivate, got {state}");
        };
        assert_eq!(generation, 1);
        assert!(removed);
        assert_eq!(reasons, vec![Reason::DigestMismatch]);
        names(
            &log,
            &["digest recorded at activation", "deactivating"],
            "class 3 requires the mismatch to be logged",
        );
        assert_eq!(active(&store), None);
        assert!(store.bundle_dir(1).is_dir());
    }

    // §6.1 class 1.

    /// "This is not an error -- it is the shipped state of every device, and
    /// it must not be logged as one." Asserted in both directions: the state
    /// is named, and the log carries no WARN and no ERROR.
    #[test]
    fn an_absent_srv_ui_is_normal_operation_and_not_an_error() {
        let (_dir, store) = store();
        assert!(!store.root().exists());

        let (state, log) = capture(|| run(&store, &crate::audit::Audit::journal_only()));
        assert_eq!(state, BundleState::BuiltIn);
        names(
            &log,
            &[crate::bundle::NO_CUSTOM_BUNDLE],
            "the no-bundle state must be a named answer",
        );
        assert!(
            !log.contains("WARN") && !log.contains("ERROR"),
            "class 1 must not be logged as an error\n--- log ---\n{log}"
        );
        assert!(
            !store.root().exists(),
            "§5.2: the root is created by the install path, never at start-up"
        );
    }

    // §8.2 phase 4's local install path.

    /// "an operator places a tree on the device and calls an activate
    /// operation, **or apid picks up a staged directory**."
    #[test]
    fn a_staged_directory_is_picked_up_at_start_up() {
        let (_dir, store) = store();
        let staging = stage(&store, 7);
        write_manifest(&staging, &["v1"]);
        assert_eq!(active(&store), None);

        let (state, log) = capture(|| run(&store, &crate::audit::Audit::journal_only()));
        assert!(
            matches!(state, BundleState::Active { generation: 7, .. }),
            "the staged tree must become the active bundle, got {state}"
        );
        assert_eq!(active(&store), Some(7));
        names(
            &log,
            &["picked up a staged UI bundle"],
            "the pick-up is logged",
        );
        assert!(!store.staging_dir(7).exists());
    }

    /// A staged tree that cannot be activated is logged and start-up carries
    /// on: the already-active bundle is still evaluated and still active.
    #[test]
    fn an_unactivatable_staged_tree_does_not_stop_the_recheck() {
        let (_dir, store) = store();
        activate(&store, 1, &["v1"], SERVED_API_VERSIONS);
        // Staged generation 9 has no index.html -- §6.1 class 2, refused.
        fs::create_dir_all(store.staging_dir(9)).expect("create staging");

        let (state, log) = capture(|| run(&store, &crate::audit::Audit::journal_only()));
        assert!(
            matches!(state, BundleState::Active { generation: 1, .. }),
            "the active bundle must be unaffected, got {state}"
        );
        names(
            &log,
            &["could not be activated", "nothing else changed"],
            "the refusal is logged",
        );
        assert_eq!(active(&store), Some(1));
    }

    // The constraint.

    /// Build one hostile store per name. Every one of these is a state the
    /// daemon must hold; none of them may be an error it returns.
    fn hostile(base: &Path) -> Vec<(&'static str, Store)> {
        let mut cases: Vec<(&'static str, Store)> = Vec::new();
        let mut root = |name: &'static str| -> PathBuf {
            let path = base.join(name);
            cases.push((name, Store::new(&path)));
            path
        };

        root("absent-root");

        let path = root("root-is-a-regular-file");
        fs::write(&path, b"not a directory").expect("write file");

        let path = root("root-is-a-dangling-symlink");
        symlink(base.join("nowhere-at-all"), &path).expect("symlink");

        let path = root("root-is-unreadable");
        fs::create_dir_all(&path).expect("create root");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod 000");

        let path = root("bundles-is-a-regular-file");
        fs::create_dir_all(&path).expect("create root");
        fs::write(path.join("bundles"), b"x").expect("write file");
        symlink("bundles/1", path.join("current")).expect("symlink");

        let path = root("current-dangles-into-nowhere");
        fs::create_dir_all(path.join("bundles")).expect("create bundles");
        symlink("bundles/7", path.join("current")).expect("symlink");

        let path = root("current-points-outside-the-store");
        fs::create_dir_all(&path).expect("create root");
        symlink("/proc/self/root/nonexistent/1", path.join("current")).expect("symlink");

        let path = root("current-is-a-regular-file");
        fs::create_dir_all(&path).expect("create root");
        fs::write(path.join("current"), b"not a symlink").expect("write file");

        let path = root("current-is-a-directory");
        fs::create_dir_all(path.join("current")).expect("create dir");

        let path = root("current-target-is-not-numeric");
        fs::create_dir_all(path.join("bundles/latest")).expect("create bundles");
        symlink("bundles/latest", path.join("current")).expect("symlink");

        let path = root("generation-is-a-regular-file");
        fs::create_dir_all(path.join("bundles")).expect("create bundles");
        fs::write(path.join("bundles/1"), b"x").expect("write file");
        symlink("bundles/1", path.join("current")).expect("symlink");

        let path = root("tree-contains-a-socket");
        fs::create_dir_all(path.join("bundles/1")).expect("create bundles");
        fs::write(path.join("bundles/1/index.html"), b"<!doctype html>").expect("write index");
        UnixListener::bind(path.join("bundles/1/sock")).expect("bind socket");
        symlink("bundles/1", path.join("current")).expect("symlink");

        let path = root("record-is-garbage");
        fs::create_dir_all(path.join("bundles/1")).expect("create bundles");
        fs::create_dir_all(path.join("records")).expect("create records");
        fs::write(path.join("bundles/1/index.html"), b"<!doctype html>").expect("write index");
        fs::write(path.join("records/1.json"), b"}{ not json").expect("write record");
        symlink("bundles/1", path.join("current")).expect("symlink");

        let path = root("record-is-a-directory");
        fs::create_dir_all(path.join("bundles/1")).expect("create bundles");
        fs::create_dir_all(path.join("records/1.json")).expect("create records");
        fs::write(path.join("bundles/1/index.html"), b"<!doctype html>").expect("write index");
        symlink("bundles/1", path.join("current")).expect("symlink");

        let path = root("manifest-is-garbage");
        fs::create_dir_all(path.join("bundles/1")).expect("create bundles");
        fs::write(path.join("bundles/1/index.html"), b"<!doctype html>").expect("write index");
        fs::write(path.join("bundles/1/mos-ui.json"), b"\x00\xff not json")
            .expect("write manifest");
        symlink("bundles/1", path.join("current")).expect("symlink");

        let path = root("manifest-declares-no-versions");
        fs::create_dir_all(path.join("bundles/1")).expect("create bundles");
        fs::write(path.join("bundles/1/index.html"), b"<!doctype html>").expect("write index");
        fs::write(
            path.join("bundles/1/mos-ui.json"),
            br#"{"name":"n","version":"1","immutableDir":"a","apiVersions":[]}"#,
        )
        .expect("write manifest");
        symlink("bundles/1", path.join("current")).expect("symlink");

        let path = root("staged-tree-contains-a-symlink");
        fs::create_dir_all(path.join(".staging-3")).expect("create staging");
        fs::write(path.join(".staging-3/index.html"), b"<!doctype html>").expect("write index");
        symlink("/etc/passwd", path.join(".staging-3/leak")).expect("symlink");

        let path = root("staged-generation-overflows-u64");
        fs::create_dir_all(path.join(".staging-99999999999999999999")).expect("create staging");

        let path = root("staging-is-a-regular-file");
        fs::create_dir_all(&path).expect("create root");
        fs::write(path.join(".staging-1"), b"x").expect("write file");

        cases
    }

    /// §6.3's safety claim: a bad bundle cannot take the listener down.
    ///
    /// Every hostile store below is run through the real entry point. The
    /// assertion is a named set — which roots returned normally, diffed
    /// against which roots were built — so a regression names the root that
    /// broke rather than reporting a count. `catch_unwind` is what makes the
    /// diff possible: without it the first panic would abort the test and the
    /// remaining roots would never be tried.
    #[test]
    fn no_hostile_store_can_stop_start_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cases = hostile(dir.path());
        let expected: BTreeSet<&str> = cases.iter().map(|(name, _)| *name).collect();
        assert!(!expected.is_empty());

        let mut returned = BTreeSet::new();
        let mut states = Vec::new();
        for (name, store) in &cases {
            // The panic hook is left in place on purpose: a failure here must
            // print where it panicked, not only that it did.
            if let Ok(state) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                run(store, &crate::audit::Audit::journal_only())
            })) {
                returned.insert(*name);
                states.push((*name, state));
            }
        }

        let missing: Vec<&&str> = expected.difference(&returned).collect();
        assert!(
            missing.is_empty(),
            "start-up did not return for {missing:?}; a bundle that can do this is a crash loop with no listener bound"
        );

        // Every outcome is one of the four defined states -- guaranteed by the
        // return type, and printed here so a regression is readable.
        let mut report = String::new();
        for (name, state) in &states {
            writeln!(report, "  {name}: {state}").expect("format");
        }
        assert_eq!(states.len(), cases.len(), "states:\n{report}");

        // Clean-up: the 0o000 root would otherwise defeat TempDir's own
        // recursive remove on a non-root test runner.
        let unreadable = dir.path().join("root-is-unreadable");
        if unreadable.exists() {
            let _ = fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755));
        }
    }

    /// The same property for the async entry point `main` actually calls, and
    /// one layer deeper: a panic raised *below* this module -- in code it only
    /// calls -- arrives as a `JoinError` and becomes a state.
    #[tokio::test]
    async fn a_panic_beneath_discovery_becomes_a_state_and_not_an_unwind() {
        let joined = tokio::task::spawn_blocking(|| -> BundleState {
            panic!("deliberate: stands in for a panic in code `run` calls")
        })
        .await;
        assert!(joined.is_err(), "the fixture must actually panic");

        let (state, log) = capture(|| join(joined));
        assert!(
            matches!(state, BundleState::Unavailable { .. }),
            "a panic must become a state, got {state}"
        );
        names(&log, &["did not complete"], "the panic is logged");
    }

    /// `discover` is what `main` calls, so it is exercised end to end at least
    /// once rather than only through its synchronous body.
    #[tokio::test]
    async fn discover_holds_the_state_for_a_real_store() {
        let (_dir, store) = store();
        activate(&store, 1, &["v1"], SERVED_API_VERSIONS);
        let state = discover(store.clone(), journal_audit()).await;
        assert!(
            matches!(state, BundleState::Active { generation: 1, .. }),
            "got {state}"
        );
        assert!(!state.to_string().is_empty());

        let (_dir2, empty) = fresh();
        assert_eq!(discover(empty, journal_audit()).await, BundleState::BuiltIn);
    }
}

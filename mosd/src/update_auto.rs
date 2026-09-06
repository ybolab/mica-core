//! The automatic update driver: what a device does on its own.
//!
//! One task, one tick, and a policy re-read every turn. Under
//! [`UpdateMode::Check`] it is the check cadence that has always been here.
//! Under [`UpdateMode::Auto`] it is PLAN-071 §2's four steps — check, fetch,
//! re-check, install — followed by a reboot under [`RebootPolicy`].
//!
//! **Every step calls the function the manual route calls.** The check and
//! the fetch are [`UpdateLifecycle::check_now`] and
//! [`UpdateLifecycle::fetch_now`], which admit exactly as `CheckUpdate` and
//! `FetchUpdate` do; the install and the reboot are the `InstallUpdate` and
//! `Reboot` routes themselves, reached through [`AutoRoutes`]. So a gate an
//! operator meets is a gate this driver meets: the policy refusals, the
//! workspace readiness probe, the maintenance window, the storage
//! reservation and the safe-to-reboot gate. There is no automatic bypass of
//! anything, because there is no second implementation to bypass it in.
//!
//! Three things the automatic path adds, and one it refuses to add:
//!
//! - **The re-check before an install** (§5). A verified bundle sitting in
//!   `verified/` may have been withdrawn since it was fetched, and TUF
//!   offers no revocation signal beyond the target's absence from the
//!   current metadata. So automation installs only what the current metadata
//!   still names and deletes what it does not. A human is not stopped: a
//!   manual install of a staged path stays available, because the human may
//!   be installing it deliberately.
//! - **The pending-slot guard.** A device whose other slot is installed and
//!   waiting for its first boot has already been updated; installing again
//!   would write over the slot the fallback needs.
//! - **The suppression and the clock predicate** (§6, §7). A version whose
//!   slot rolled back is not selected again — without that, `auto` is a
//!   reboot loop, and PLAN-071 calls it the single most important safety
//!   property in the plan — and an automatic install requires a clock the
//!   device believes, because a maintenance window is UTC wall-clock and a
//!   window verdict computed from a clock nobody vouches for is not a
//!   verdict. Both refuse where a human is not refused: a manual install of
//!   a suppressed version stays available, on the same reasoning as the
//!   re-check above.
//! - **It never arms the reboot-gate override** (§2 step 4). That is a
//!   human's judgement that this reboot outranks what an application
//!   declared it must not be interrupted for, and a machine cannot make it.
//!   Enforced by construction: [`AutoRoutes`] has no method that arms it.
//!
//! **Every refusal is recorded, not logged and forgotten** (§2). Each
//! `defer` call names a reason from the plan's vocabulary and lands in
//! `update.lifecycle.deferred` with the refusing rule, when the reason first
//! applied and how many attempts it has refused since. A permanently
//! blocking application permanently defers the reboot, which is correct and
//! is also indistinguishable from a stuck update unless the device says so.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;

use mosd_settings::{UPDATE_CHECK_EVENT, UPDATE_FETCH_EVENT, UPDATE_INSTALL_EVENT};

use crate::time_status::ClockTrust;
use crate::update_lifecycle::{Available, Refusal, Settled};
use crate::update_policy::{self, LoadedPolicy, RebootPolicy, UpdateMode};
use crate::update_suppress::Suppression;

/// Who the driver's actions are attributed to, wherever an operator's bus
/// name would be. One name, so an audit reading `requested_by` can tell a
/// machine's install from a human's without having to infer it.
pub const SENDER: &str = "auto-update";

/// How often the driver looks at the world.
///
/// A maintenance window is `HH:MM`-precise and may be a single minute long,
/// so a driver that slept longer could not honestly claim to act *inside*
/// one. A tick with nothing to do costs one policy-file read.
const TICK: Duration = Duration::from_secs(60);

/// The daemon facts the driver reads that are not the lifecycle's own. One
/// value because they come from one RAUC query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateFacts {
    /// A slot is installed and activated and has not booted yet.
    pub reboot_pending: bool,
    /// `update.install.status` as the install route records it: `running`,
    /// `done` or `failed`. `None` when nothing has installed anything.
    pub install_status: Option<String>,
}

/// Everything the automatic driver can do, and nothing else.
///
/// The driver is written against this trait rather than against the daemon
/// so that PLAN-071 §2 step 4's invariant is structural rather than
/// remembered: **`SetRebootOverride` is not on this surface**, so no
/// automatic path can arm the reboot-gate override. Not "does not today" —
/// cannot, from here. The same seam is what a test drives the whole driver
/// through without a bus.
#[async_trait::async_trait]
pub trait AutoRoutes: Send + Sync {
    /// The policy document, loaded fresh. Every decision re-reads it, so an
    /// operator's edit takes effect on the next tick with no restart.
    fn policy(&self) -> LoadedPolicy;

    /// `CheckUpdate`, awaited to its outcome.
    async fn check(&self, sender: &str) -> Result<Settled<Available>, Refusal>;

    /// `FetchUpdate`, awaited to its outcome.
    async fn fetch(&self, sender: &str) -> Result<Settled<String>, Refusal>;

    /// The candidate the last check selected.
    async fn available(&self) -> Option<Available>;

    /// The verified bundle the last fetch staged.
    async fn staged(&self) -> Option<String>;

    /// Delete a staged bundle the current metadata no longer names, and
    /// forget it.
    async fn discard_staged(&self, why: &str);

    /// The pending-slot and last-install facts, from one RAUC query.
    /// `None` when the query did not answer — which is not "nothing is
    /// pending", so the driver defers rather than proceeding on a guess.
    async fn facts(&self) -> Option<UpdateFacts>;

    /// `InstallUpdate`, with its own gates; the error is what it answers an
    /// operator.
    async fn install(&self, sender: &str, bundle: &str) -> Result<(), String>;

    /// `Reboot`, honouring the safe-to-reboot gate; the error is the gate's
    /// refusal, verbatim.
    async fn reboot(&self, sender: &str) -> Result<(), String>;

    /// PLAN-071 §6's record for `version`, or the store's own error.
    ///
    /// The error is carried rather than swallowed: a suppression store that
    /// exists and does not parse must not read as "nothing is suppressed",
    /// because that reading is exactly the loop the store exists to break.
    async fn suppression(&self, version: &str) -> Result<Option<Suppression>, String>;

    /// The two signals PLAN-071 §7's clock predicate reads.
    async fn clock(&self) -> ClockTrust;

    /// Record one automatic action in the device's audit ring (U8).
    ///
    /// **The same event names the manual routes record**, under the `policy`
    /// actor rather than an operator's: PLAN-071 §3 requires the trail to be
    /// able to answer *did a human do this*, and an event set that cannot is
    /// a support tool that lies during exactly the incident it exists for.
    /// The names are `mosd_settings`'s constants on both sides, so the two
    /// halves of one trail cannot drift into two.
    async fn audit(&self, event: &str);

    /// Record why this pass did not proceed (§2, U5).
    async fn defer(&self, reason: &str, detail: &str);

    /// Forget a recorded deferral; `only` clears just that reason.
    async fn resume(&self, only: Option<&str>);
}

/// Where the driver is between ticks.
///
/// Nothing here is persisted. A restart re-derives what it can from the
/// daemon's own facts, and the one thing it cannot re-derive — that *this*
/// driver asked for the install now pending — is exactly the thing that must
/// not outlive the process: an automatic reboot is owed for an automatic
/// install, not for whatever a human left staged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Nothing of the driver's is in flight.
    Idle,
    /// The driver asked for an install and is waiting for RAUC to finish.
    Installing,
    /// The driver's install finished; a reboot is owed under
    /// [`RebootPolicy::Window`] as soon as the window and the gate allow.
    RebootPending,
}

/// The driver's cadence clock, and the ONE thing it is allowed to answer.
///
/// The driver measures a single quantity with a clock of its own: how long
/// since it last *attempted* a check. That is a monotonic duration, and in
/// production it is [`Instant::now`]. The seam exists because `Instant` has
/// none of its own — the type is opaque, `tokio::time::pause` does not move
/// it, and a driver whose cadence cannot be advanced cannot be ticked in a
/// test at all. Every gate below the cadence was unreachable until this
/// existed.
///
/// **It is not a second way to read the time of day, and it cannot become
/// one.** The two places the automatic path needs a WALL clock are
/// [`AutoRoutes::clock`] — PLAN-071 §7's predicate over
/// `docs/design/time.md`'s trusted-clock floor — and the `Utc::now()` the
/// maintenance-window verdict is computed at. Neither is routed through here,
/// and the return type is what keeps it that way: an [`Instant`] names no
/// date, so nothing downstream can turn one into a window verdict or into a
/// claim that the clock is believed. A seam that answered a wall-clock time
/// would let a caller hand the driver a clock this device does not vouch for
/// and have it install outside its window on the strength of it, which is the
/// exact refusal §7 exists to make.
pub trait Cadence: Send + Sync {
    /// The monotonic now.
    fn now(&self) -> Instant;
}

/// The production cadence: the machine's own monotonic clock.
pub struct SystemCadence;

impl Cadence for SystemCadence {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// The driver's own state between ticks.
pub struct AutoDriver {
    routes: Arc<dyn AutoRoutes>,
    cadence: Arc<dyn Cadence>,
    /// When a check was last attempted. Attempt-based rather than
    /// success-based: a check refused by policy must not retry every tick,
    /// and the cadence an operator set is a cadence of attempts.
    last_check: Instant,
    stage: Stage,
}

/// Run the driver forever. One task, spawned by `main` on a production
/// daemon only.
pub async fn run(routes: Arc<dyn AutoRoutes>) {
    let mut driver = AutoDriver::new(routes, Arc::new(SystemCadence));
    loop {
        tokio::time::sleep(TICK).await;
        driver.tick().await;
    }
}

impl AutoDriver {
    pub fn new(routes: Arc<dyn AutoRoutes>, cadence: Arc<dyn Cadence>) -> Self {
        Self {
            routes,
            // The first check falls one interval after start, which is what
            // the cadence this replaces did by sleeping before its first
            // check: a device that reboots hourly must not check hourly.
            last_check: cadence.now(),
            cadence,
            stage: Stage::Idle,
        }
    }

    /// One turn of the driver.
    pub async fn tick(&mut self) {
        let loaded = self.routes.policy();
        let Some(selection) = loaded.policy.selection.as_ref() else {
            // A document that exists and does not parse refuses every
            // restricted action already, and the load answers with NO
            // selection beside the error — so there is no mode to read, which
            // is stronger than declining to read one: PLAN-070 §5.1 forbids
            // falling back to the baked channel here, and there is nothing
            // here to fall back to. The device initiates nothing until the
            // file is fixed; the operator's manual routes still refuse with
            // the error naming the file.
            return;
        };
        match selection.mode {
            UpdateMode::Off => {
                // No timer arms, and an owed automatic reboot is dropped
                // rather than carried: the operator has just said the device
                // initiates nothing. A pending slot stays pending and a human
                // reboots into it.
                self.stage = Stage::Idle;
            }
            UpdateMode::Check => {
                self.check_if_due(&loaded).await;
            }
            UpdateMode::Auto => {
                if let Some(reason) = loaded.policy.auto_window_refusal() {
                    // `auto` inherited from the baked default over a document
                    // that names no window. The document-local case is a load
                    // error (`configuration::validate`); this is the case
                    // precedence creates, and it refuses the automatic
                    // install only — the check cadence and every manual route
                    // keep working.
                    tracing::warn!(reason, "automatic install refused");
                    self.check_if_due(&loaded).await;
                    return;
                }
                self.drive(&loaded).await;
            }
        }
    }

    /// Step 1: the check cadence, shared by `check` and `auto`.
    async fn check_if_due(&mut self, loaded: &LoadedPolicy) {
        // `auto_check_minutes` is the one reading of "does this device check
        // on its own": `off`, a zero interval and a document that did not
        // load all answer `None`, so no caller re-derives the three.
        let Some(interval) = loaded.auto_check_minutes() else {
            return;
        };
        let now = self.cadence.now();
        if now.duration_since(self.last_check) < Duration::from_secs(interval.saturating_mul(60)) {
            return;
        }
        self.last_check = now;
        self.routes.audit(UPDATE_CHECK_EVENT).await;
        match self.routes.check(SENDER).await {
            // The channel is up to date. Recorded rather than passed over:
            // the lifecycle renders this as plain `idle`, which is also what
            // a device that has never checked renders as, and an operator
            // watching a release they expect needs to be told that the
            // device looked and the channel does not carry it.
            Ok(Settled::NoneCompatible) => {
                // Named through the resolved selection, which is the only
                // place the channel exists after PLAN-070 §5.1's precedence.
                // The unnamed arm is unreachable from here — a device with no
                // selection has no cadence either, and `auto_check_minutes`
                // returned above — and is written rather than unwrapped so
                // that a later caller cannot make it panic.
                let detail = match loaded.policy.selection.as_ref() {
                    Some(selection) => format!(
                        "channel `{}` publishes nothing newer than the running system",
                        selection.channel
                    ),
                    None => "the configured channel publishes nothing newer than the \
                             running system"
                        .to_string(),
                };
                self.defer("no-newer-release", &detail).await;
            }
            // It found one: that supersedes the fact above and nothing else.
            // A window that was shut a minute ago is still shut.
            Ok(_) => self.routes.resume(Some("no-newer-release")).await,
            Err(refusal) => {
                tracing::debug!(reason = refusal.message(), "automatic check skipped");
                self.defer("check-refused", refusal.message()).await;
            }
        }
    }

    /// The `auto` pass: check, fetch, re-check, install, reboot.
    async fn drive(&mut self, loaded: &LoadedPolicy) {
        match self.stage {
            Stage::Installing => return self.await_install(loaded).await,
            Stage::RebootPending => return self.reboot_if_allowed(loaded).await,
            Stage::Idle => {}
        }
        self.check_if_due(loaded).await;
        // Step 2 — fetch what the check named, unless it is already staged.
        // Comparing the candidate against the staged file (rather than
        // fetching only when nothing is staged) is what lets the pass move
        // on when the publisher released something newer between the fetch
        // and the window: the newer target is fetched, and the older staged
        // file is left where a human can still install it.
        if let Some(candidate) = self.routes.available().await {
            // §6 again, one step earlier than the refusal that closes the
            // loop: a version this device has already rolled back is not
            // downloaded again either, which is what stops a metered link
            // from paying for the same bad bundle once per window. The pass
            // stops here rather than falling through to an install step that
            // would refuse it anyway, so the recorded reason names the
            // suppression instead of whatever happens to be staged.
            match self.routes.suppression(&candidate.version).await {
                Ok(Some(record)) => {
                    self.defer("version-suppressed", &record.detail).await;
                    return;
                }
                Err(error) => {
                    self.defer("suppression-unreadable", &error).await;
                    return;
                }
                Ok(None) => {}
            }
            let staged = self.routes.staged().await;
            if staged.as_deref().and_then(bundle_name) != Some(candidate.name.as_str()) {
                self.routes.audit(UPDATE_FETCH_EVENT).await;
                if let Err(refusal) = self.routes.fetch(SENDER).await {
                    tracing::debug!(reason = refusal.message(), "automatic fetch skipped");
                    self.defer("fetch-refused", refusal.message()).await;
                    return;
                }
            }
        }
        // Step 3 — install, in the window, only what the metadata still names.
        if let Some(bundle) = self.routes.staged().await {
            self.install_if_allowed(loaded, &bundle).await;
        }
    }

    /// Step 3: the clock, the window, the pending-slot guard, the re-check,
    /// the suppression, the install.
    async fn install_if_allowed(&mut self, loaded: &LoadedPolicy, bundle: &str) {
        // PLAN-071 §7, and FIRST in the list on purpose. Every other
        // precondition below is judged against a wall clock: the maintenance
        // window is UTC `HH:MM`, so a window verdict computed from a clock
        // nobody vouches for is not a verdict, and refusing on the window
        // afterwards would report the wrong reason for the same refusal.
        // `believes` is `docs/design/time.md` §3's floor and §5's
        // `synchronized`, not a notion invented here. Checks and fetches are
        // deliberately unaffected — neither is time-keyed, and refusing them
        // would make a clockless device stop even discovering updates.
        if let Some(reason) = self.routes.clock().await.untrusted_reason() {
            self.defer("clock-untrusted", &reason).await;
            return;
        }
        // The same refusal `InstallUpdate` answers an operator with: the
        // maintenance window, which `auto` requires the document to name.
        if let Some(reason) = update_policy::install_refusal(loaded, Utc::now()) {
            self.defer("outside-window", &reason).await;
            return;
        }
        let Some(facts) = self.routes.facts().await else {
            self.defer("slot-status-unknown", "RAUC did not answer the slot query")
                .await;
            return;
        };
        if facts.reboot_pending {
            self.defer(
                "reboot-pending",
                "a slot is already installed and waiting for its first boot",
            )
            .await;
            return;
        }
        // The re-check of §5, and the version it answers with is what §6's
        // suppression is consulted on immediately below. Audited like the
        // cadence check above, because it IS a check: the trail records what
        // the device did, not a summary of what a pass was for.
        self.routes.audit(UPDATE_CHECK_EVENT).await;
        let named = match self.routes.check(SENDER).await {
            Ok(Settled::Done(candidate)) => Some(candidate),
            Ok(Settled::NoneCompatible) => None,
            Ok(Settled::Unready(unready)) => {
                self.defer("workspace-unready", &unready.reason()).await;
                return;
            }
            Ok(Settled::Failed(reason)) => {
                self.defer("recheck-failed", &reason).await;
                return;
            }
            Err(refusal) => {
                self.defer("recheck-refused", refusal.message()).await;
                return;
            }
        };
        let version = match named {
            // The metadata still names it: this is the version to install.
            Some(candidate) if Some(candidate.name.as_str()) == bundle_name(bundle) => {
                candidate.version
            }
            // It names something else. The staged bundle is superseded rather
            // than provably withdrawn — a check reports the selection, not the
            // whole target list — so it is not deleted, and the next pass
            // fetches what was named.
            Some(candidate) => {
                self.defer(
                    "superseded",
                    &format!("the check now names {}", candidate.name),
                )
                .await;
                return;
            }
            // Nothing compatible is published at all, so the current metadata
            // does not name the staged bundle: it was withdrawn.
            None => {
                self.routes
                    .discard_staged("the current metadata no longer names it")
                    .await;
                return;
            }
        };
        // PLAN-071 §6, HERE: between the re-check and the install, because
        // this is the first point at which the version about to be written
        // is known rather than guessed at. Without this refusal `auto` is a
        // reboot loop — the fallback leaves the device on the older system,
        // which makes the failed version newer again, and the next window
        // installs it again. The refusal binds the AUTOMATIC path only: a
        // manual install of this same bundle is still permitted, because the
        // operator reading the record has been told and is choosing.
        match self.routes.suppression(&version).await {
            Ok(Some(record)) => {
                self.defer("version-suppressed", &record.detail).await;
                return;
            }
            // A store that exists and cannot be read is not an empty store.
            // Reading it as one is precisely how the loop restarts, so the
            // closed side here is refusing the install.
            Err(error) => {
                self.defer("suppression-unreadable", &error).await;
                return;
            }
            Ok(None) => {}
        }
        self.routes.audit(UPDATE_INSTALL_EVENT).await;
        match self.routes.install(SENDER, bundle).await {
            Ok(()) => {
                tracing::warn!(bundle, version, "automatic install started");
                self.stage = Stage::Installing;
                // The pass ran to its end; every reason it was refused for
                // before now is stale.
                self.routes.resume(None).await;
            }
            Err(refusal) => self.defer("install-refused", &refusal).await,
        }
    }

    /// Wait out the install this driver started, then owe a reboot or not.
    async fn await_install(&mut self, loaded: &LoadedPolicy) {
        // An unanswered query leaves the stage where it is: the install is
        // still whatever it was, and the next tick asks again.
        let Some(facts) = self.routes.facts().await else {
            return;
        };
        match facts.install_status.as_deref() {
            Some("running") => {}
            Some("done") => {
                self.stage = Stage::RebootPending;
                self.reboot_if_allowed(loaded).await;
            }
            // A failed install, or a status nobody wrote: either way nothing
            // is owed a reboot, and the failure is already recorded under
            // `update.install` where an operator reads it.
            other => {
                if let Some(status) = other {
                    tracing::warn!(status, "automatic install did not finish cleanly");
                }
                self.stage = Stage::Idle;
            }
        }
    }

    /// Step 4: reboot under `rebootPolicy`, in the same window, gate-honoured.
    async fn reboot_if_allowed(&mut self, loaded: &LoadedPolicy) {
        if loaded.policy.reboot_policy != RebootPolicy::Window {
            // `manual`: the install happened, the reboot is a human's. The
            // lifecycle reports `reboot-required` until one arrives.
            self.stage = Stage::Idle;
            return;
        }
        if let Some(reason) = update_policy::install_refusal(loaded, Utc::now()) {
            self.defer("outside-window", &reason).await;
            return;
        }
        if let Err(refusal) = self.routes.reboot(SENDER).await {
            // The gate is closed: an install in flight, or a component
            // reporting a blocking health status. Automation defers and the
            // next window re-attempts. It does not arm the override — there
            // is no method on `AutoRoutes` to arm it with.
            self.defer("reboot-gate-closed", &refusal).await;
            return;
        }
        // The machine is going down; the stage it leaves behind is moot, and
        // so is every reason the pass was refused for on the way here.
        self.routes.resume(None).await;
        self.stage = Stage::Idle;
    }

    /// Say why an automatic step did not proceed, in the log AND in the
    /// state (PLAN-071 §2, U5).
    ///
    /// The log line is for whoever is reading the journal at the time; the
    /// recorded fact is for the operator who opens the update page a week
    /// later, which is the reader §2 is written for. The lifecycle keeps the
    /// instant the reason first applied and the number of attempts it has
    /// refused, so "a permanently blocking application" and "a stuck update"
    /// stop looking the same from outside.
    async fn defer(&self, reason: &str, detail: &str) {
        tracing::info!(reason, detail, "automatic update deferred");
        self.routes.defer(reason, detail).await;
    }
}

/// The target name a staged bundle path carries. `rauc-update` stages a
/// verified bundle under its target name and nothing else, so the file name
/// is what a check's selection is compared against.
fn bundle_name(bundle: &str) -> Option<&str> {
    Path::new(bundle).file_name().and_then(|name| name.to_str())
}

/// A cadence a test moves by hand, and the reason [`Cadence`] exists.
///
/// [`Instant`] cannot be constructed at a chosen point, but it can be offset
/// from one, so a driver holding this advances exactly as far as a test says
/// and never by sleeping. Monotonic like the production clock and for the
/// same reason: nothing here answers a time of day.
#[cfg(test)]
pub struct TestCadence {
    base: Instant,
    offset: std::sync::Mutex<Duration>,
}

#[cfg(test)]
impl TestCadence {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            base: Instant::now(),
            offset: std::sync::Mutex::new(Duration::ZERO),
        })
    }

    /// Move the driver's notion of now forward by `by`.
    pub fn advance(&self, by: Duration) {
        *self.offset.lock().expect("cadence offset") += by;
    }

    /// Past any cadence these tests configure, so the next tick checks.
    pub fn advance_past_the_check_interval(&self) {
        self.advance(Duration::from_secs(61 * 60));
    }
}

#[cfg(test)]
impl Cadence for TestCadence {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock().expect("cadence offset")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, VecDeque};
    use std::sync::Mutex as StdMutex;

    use chrono::Duration as Wall;

    use super::*;
    use crate::time_status::SyncStatus;
    use crate::update_lifecycle::Unready;
    use crate::update_policy::PolicyStore;
    use crate::update_suppress::SuppressionStore;

    /// One thing the driver did, in the order it did it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Check,
        Fetch,
        Discard(String),
        Install(String),
        Reboot,
        Audit(String),
        Defer(String, String),
        Resume(Option<String>),
    }

    /// The daemon the driver talks to, scripted.
    ///
    /// Everything the driver may learn is a field a test sets; everything it
    /// does lands in `log`. Three behaviours are *mirrored* from
    /// [`crate::update_lifecycle::UpdateLifecycle`] rather than invented,
    /// because the driver's next step depends on them: a check that selects a
    /// candidate records it as `available`, a fetch that stages a bundle
    /// records it as `staged`, and the suppression route is the real store
    /// read through the same two lines `suppression_for` uses — so the refusal
    /// that closes PLAN-071 §6's loop is tested against a file on disk and not
    /// against a boolean this test set.
    struct FakeDaemon {
        policy: PolicyStore,
        suppression: SuppressionStore,
        check: StdMutex<VecDeque<Result<Settled<Available>, Refusal>>>,
        fetch: StdMutex<VecDeque<Result<Settled<String>, Refusal>>>,
        available: StdMutex<Option<Available>>,
        staged: StdMutex<Option<String>>,
        facts: StdMutex<Option<UpdateFacts>>,
        install: StdMutex<Result<(), String>>,
        reboot: StdMutex<Result<(), String>>,
        clock: StdMutex<ClockTrust>,
        log: StdMutex<Vec<Call>>,
    }

    impl FakeDaemon {
        fn new(policy: PolicyStore, suppression: SuppressionStore) -> Arc<Self> {
            Arc::new(Self {
                policy,
                suppression,
                check: StdMutex::new(VecDeque::new()),
                fetch: StdMutex::new(VecDeque::new()),
                available: StdMutex::new(None),
                staged: StdMutex::new(None),
                facts: StdMutex::new(Some(UpdateFacts::default())),
                install: StdMutex::new(Ok(())),
                reboot: StdMutex::new(Ok(())),
                clock: StdMutex::new(trusted_clock()),
                log: StdMutex::new(Vec::new()),
            })
        }

        fn will_check(&self, answer: Result<Settled<Available>, Refusal>) {
            self.check.lock().expect("check").push_back(answer);
        }

        fn will_fetch(&self, answer: Result<Settled<String>, Refusal>) {
            self.fetch.lock().expect("fetch").push_back(answer);
        }

        fn set<T>(slot: &StdMutex<T>, value: T) {
            *slot.lock().expect("slot") = value;
        }

        fn calls(&self) -> Vec<Call> {
            self.log.lock().expect("log").clone()
        }

        fn installs(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Install(bundle) => Some(bundle),
                    _ => None,
                })
                .collect()
        }

        /// Every deferral recorded so far, in order.
        fn deferrals(&self) -> Vec<(String, String)> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Defer(reason, detail) => Some((reason, detail)),
                    _ => None,
                })
                .collect()
        }

        /// The one deferral this pass recorded. Panics on none or several: a
        /// test that meant one refusal and got two has learned something and
        /// should say so rather than pick the one it hoped for.
        fn the_deferral(&self) -> (String, String) {
            let deferrals = self.deferrals();
            assert_eq!(
                deferrals.len(),
                1,
                "expected exactly one deferral, got {deferrals:?}"
            );
            deferrals.into_iter().next().expect("one deferral")
        }

        fn log(&self, call: Call) {
            self.log.lock().expect("log").push(call);
        }
    }

    #[async_trait::async_trait]
    impl AutoRoutes for FakeDaemon {
        fn policy(&self) -> LoadedPolicy {
            self.policy.load()
        }

        async fn check(&self, _sender: &str) -> Result<Settled<Available>, Refusal> {
            self.log(Call::Check);
            let answer = self
                .check
                .lock()
                .expect("check")
                .pop_front()
                .expect("the driver checked more times than this test scripted");
            // What `settle_check` records, so the step after this one reads
            // what a real daemon would have left behind.
            match &answer {
                Ok(Settled::Done(selected)) => Self::set(&self.available, Some(selected.clone())),
                Ok(Settled::NoneCompatible) => Self::set(&self.available, None),
                _ => {}
            }
            answer
        }

        async fn fetch(&self, _sender: &str) -> Result<Settled<String>, Refusal> {
            self.log(Call::Fetch);
            let answer = self
                .fetch
                .lock()
                .expect("fetch")
                .pop_front()
                .expect("the driver fetched more times than this test scripted");
            // What `settle_fetch` records.
            if let Ok(Settled::Done(path)) = &answer {
                Self::set(&self.staged, Some(path.clone()));
            }
            answer
        }

        async fn available(&self) -> Option<Available> {
            self.available.lock().expect("available").clone()
        }

        async fn staged(&self) -> Option<String> {
            self.staged.lock().expect("staged").clone()
        }

        async fn discard_staged(&self, why: &str) {
            self.log(Call::Discard(why.to_string()));
            Self::set(&self.staged, None);
        }

        async fn facts(&self) -> Option<UpdateFacts> {
            self.facts.lock().expect("facts").clone()
        }

        async fn install(&self, _sender: &str, bundle: &str) -> Result<(), String> {
            self.log(Call::Install(bundle.to_string()));
            self.install.lock().expect("install").clone()
        }

        async fn reboot(&self, _sender: &str) -> Result<(), String> {
            self.log(Call::Reboot);
            self.reboot.lock().expect("reboot").clone()
        }

        async fn suppression(&self, version: &str) -> Result<Option<Suppression>, String> {
            // `UpdateLifecycle::suppression_for`, verbatim: an unreadable
            // store is an error and never an empty one.
            let loaded = self.suppression.load();
            match loaded.error {
                Some(error) => Err(error),
                None => Ok(loaded.get(version).cloned()),
            }
        }

        async fn clock(&self) -> ClockTrust {
            *self.clock.lock().expect("clock")
        }

        async fn audit(&self, event: &str) {
            self.log(Call::Audit(event.to_string()));
        }

        async fn defer(&self, reason: &str, detail: &str) {
            self.log(Call::Defer(reason.to_string(), detail.to_string()));
        }

        async fn resume(&self, only: Option<&str>) {
            self.log(Call::Resume(only.map(str::to_string)));
        }
    }

    /// A clock the device believes, by the first of PLAN-071 §7's two limbs.
    fn trusted_clock() -> ClockTrust {
        ClockTrust {
            status: Some(SyncStatus::Synchronized),
            floor_advanced: false,
        }
    }

    /// The case §7 is written for: `offline-degraded` with no advance.
    fn untrusted_clock() -> ClockTrust {
        ClockTrust {
            status: Some(SyncStatus::OfflineDegraded),
            floor_advanced: false,
        }
    }

    /// `HH:MM` UTC, `offset` from now.
    fn clock_face(offset: Wall) -> String {
        (Utc::now() + offset).format("%H:%M").to_string()
    }

    /// A window open right now: two hours either side of it, so the verdict
    /// does not depend on the minute the suite happens to run in, and the
    /// wrap-past-midnight arithmetic is exercised whenever it does run near
    /// one.
    fn open_window() -> String {
        format!(
            r#"{{"start": "{}", "end": "{}"}}"#,
            clock_face(-Wall::hours(2)),
            clock_face(Wall::hours(2))
        )
    }

    /// A window shut right now: it opens in two hours.
    fn shut_window() -> String {
        format!(
            r#"{{"start": "{}", "end": "{}"}}"#,
            clock_face(Wall::hours(2)),
            clock_face(Wall::hours(4))
        )
    }

    /// The operator document an `auto` device carries.
    fn auto_document(window: &str, reboot_policy: &str) -> String {
        format!(
            r#"{{"policy": "auto", "checkIntervalMinutes": 60,
                 "rebootPolicy": "{reboot_policy}",
                 "source": {{"url": "http://mirror/tuf", "channel": "stable"}},
                 "maintenance": {{"windows": [{window}]}}}}"#
        )
    }

    const BUNDLE: &str = "/mos/updates/verified/mos-1.5.0.raucb";

    fn candidate(name: &str, version: &str) -> Available {
        Available {
            name: name.to_string(),
            version: version.to_string(),
            channel: "stable".to_string(),
        }
    }

    /// The release the staged [`BUNDLE`] carries.
    fn the_candidate() -> Available {
        candidate("mos-1.5.0.raucb", "1.5.0")
    }

    /// What the state refresh writes when a slot rolls back (PLAN-071 §6).
    fn rollback_record(version: &str) -> Suppression {
        Suppression {
            version: version.to_string(),
            slot: "rootfs.1".to_string(),
            at: "2026-09-05T02:11:00Z".to_string(),
            boot_status: Some("bad".to_string()),
            detail: format!("version {version} was installed into rootfs.1, which rolled back"),
        }
    }

    /// One device: its documents on disk, its daemon, its cadence and the
    /// driver ticking against all three.
    struct Scene {
        dir: tempfile::TempDir,
        daemon: Arc<FakeDaemon>,
        cadence: Arc<TestCadence>,
        driver: AutoDriver,
    }

    impl Scene {
        /// An `auto` device with the window and reboot policy named, a
        /// believed clock, an answering slot query and nothing staged.
        fn auto(window: &str, reboot_policy: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("updates.json");
            std::fs::write(&path, auto_document(window, reboot_policy))
                .expect("seed the policy document");
            let daemon = FakeDaemon::new(
                PolicyStore::at(path),
                SuppressionStore::at(dir.path().join("suppressed-versions.json")),
            );
            let cadence = TestCadence::new();
            let routes: Arc<dyn AutoRoutes> = daemon.clone();
            let clock: Arc<dyn Cadence> = cadence.clone();
            let driver = AutoDriver::new(routes, clock);
            Self {
                dir,
                daemon,
                cadence,
                driver,
            }
        }

        /// Rewrite the document the store already points at. Every decision
        /// re-reads it, so this is how an operator's edit lands mid-run.
        fn rewrite(&self, window: &str, reboot_policy: &str) {
            std::fs::write(
                self.dir.path().join("updates.json"),
                auto_document(window, reboot_policy),
            )
            .expect("rewrite the policy document");
        }

        fn write_document(&self, body: &str) {
            std::fs::write(self.dir.path().join("updates.json"), body)
                .expect("rewrite the policy document");
        }

        /// Suppress `version` through the real store's real write path.
        fn suppress(&self, version: &str) {
            assert!(
                self.daemon
                    .suppression
                    .record(&rollback_record(version))
                    .expect("record the rollback"),
                "the first record of a version is a new one"
            );
        }

        fn corrupt_the_suppression_store(&self) {
            std::fs::write(
                self.dir.path().join("suppressed-versions.json"),
                "{not json",
            )
            .expect("corrupt the store");
        }

        async fn tick(&mut self) {
            self.driver.tick().await;
        }
    }

    /// The seam itself, and the property every test below rests on: a driver
    /// reaches its next pass because a test moved its clock, not because a
    /// test waited.
    ///
    /// Before [`Cadence`] the cadence was `Instant::elapsed`, which answers
    /// real elapsed time that `tokio::time::pause` does not move — so the
    /// second assertion here was reachable only by sleeping for an hour, and
    /// with it every gate that lives past the first check.
    #[tokio::test]
    async fn the_cadence_seam_advances_the_driver_without_sleeping() {
        let mut scene = Scene::auto(&open_window(), "manual");
        scene.daemon.will_check(Ok(Settled::NoneCompatible));

        // The first check falls one interval after start, not on the first
        // tick: a device that reboots hourly must not check hourly.
        scene.tick().await;
        scene.cadence.advance(Duration::from_secs(59 * 60));
        scene.tick().await;
        assert!(
            !scene.daemon.calls().contains(&Call::Check),
            "the interval has not elapsed: {:?}",
            scene.daemon.calls()
        );

        scene.cadence.advance(Duration::from_secs(2 * 60));
        scene.tick().await;
        assert_eq!(
            scene
                .daemon
                .calls()
                .iter()
                .filter(|call| **call == Call::Check)
                .count(),
            1,
            "the interval elapsed and the driver checked once"
        );

        // Attempt-based, not success-based: the check above found nothing and
        // the next one is still a whole interval away.
        scene.tick().await;
        assert_eq!(
            scene
                .daemon
                .calls()
                .iter()
                .filter(|call| **call == Call::Check)
                .count(),
            1,
            "a checked cadence must not retry on the next tick"
        );
    }

    /// Every reason the driver can mint, read out of its own source.
    ///
    /// The needle is assembled rather than written so that this scan cannot
    /// find itself. Reading the source is what turns PLAN-071 U5's gate
    /// (*each deferral reason reachable in a test*) into a criterion that
    /// keeps holding: a new `defer` call site with a new reason fails the
    /// comparison below until a case covers it.
    fn reasons_the_driver_can_mint() -> BTreeSet<String> {
        let needle = format!("self.{}(", "defer");
        let source = include_str!("update_auto.rs");
        let mut reasons = BTreeSet::new();
        for (index, _) in source.match_indices(&needle) {
            let rest = &source[index + needle.len()..];
            let opening = rest.find('"').expect("a defer call names its reason");
            let rest = &rest[opening + 1..];
            let closing = rest.find('"').expect("an unterminated reason literal");
            reasons.insert(rest[..closing].to_string());
        }
        reasons
    }

    /// PLAN-071 U5: each deferral reason reachable in a test.
    ///
    /// Fifteen reasons over eighteen call sites, and the set is compared
    /// against [`reasons_the_driver_can_mint`] at the end rather than against
    /// a list written here — a list would only assert what its author
    /// remembered on the day.
    #[tokio::test]
    async fn every_deferral_reason_the_driver_can_mint_is_reachable() {
        let mut observed: BTreeSet<String> = BTreeSet::new();
        let mut record = |scene: &Scene, expected: &str, detail_contains: &str| {
            let (reason, detail) = scene.daemon.the_deferral();
            assert_eq!(reason, expected, "detail was: {detail}");
            assert!(
                detail.contains(detail_contains),
                "`{expected}` must carry the refusing rule; got: {detail}"
            );
            observed.insert(reason);
        };

        // 1. The channel publishes nothing newer. Recorded rather than passed
        //    over: `idle` is also what a device that never checked renders as.
        let mut scene = Scene::auto(&open_window(), "window");
        scene.daemon.will_check(Ok(Settled::NoneCompatible));
        scene.cadence.advance_past_the_check_interval();
        scene.tick().await;
        record(&scene, "no-newer-release", "channel `stable`");

        // 2. The cadence check, refused by the policy an operator would meet.
        let mut scene = Scene::auto(&open_window(), "window");
        scene.daemon.will_check(Err(Refusal::Policy(
            "network mode is offline: updates arrive by import only".to_string(),
        )));
        scene.cadence.advance_past_the_check_interval();
        scene.tick().await;
        record(&scene, "check-refused", "network mode is offline");

        // 3. §6 one step before the install: a rolled-back version is not
        //    downloaded again either.
        let mut scene = Scene::auto(&open_window(), "window");
        scene.suppress("1.5.0");
        FakeDaemon::set(&scene.daemon.available, Some(the_candidate()));
        scene.tick().await;
        record(&scene, "version-suppressed", "rootfs.1");

        // 4. A store that exists and does not parse is not an empty store.
        let mut scene = Scene::auto(&open_window(), "window");
        scene.corrupt_the_suppression_store();
        FakeDaemon::set(&scene.daemon.available, Some(the_candidate()));
        scene.tick().await;
        record(&scene, "suppression-unreadable", "parse ");

        // 5. The fetch, refused by the policy an operator would meet.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.available, Some(the_candidate()));
        scene.daemon.will_fetch(Err(Refusal::Policy(
            "network mode is metered: bundle downloads are refused".to_string(),
        )));
        scene.tick().await;
        record(&scene, "fetch-refused", "network mode is metered");

        // 6. §7's predicate, first in the list on purpose.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        FakeDaemon::set(&scene.daemon.clock, untrusted_clock());
        scene.tick().await;
        record(&scene, "clock-untrusted", "offline-degraded");

        // 7. The maintenance window, which `auto` requires the document to
        //    name and which gates the install and only the install.
        let mut scene = Scene::auto(&shut_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.tick().await;
        record(
            &scene,
            "outside-window",
            "outside every configured maintenance window",
        );

        // 8. An unanswered slot query is not "nothing is pending".
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        FakeDaemon::set(&scene.daemon.facts, None);
        scene.tick().await;
        record(&scene, "slot-status-unknown", "RAUC did not answer");

        // 9. A slot already installed and waiting for its first boot.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        FakeDaemon::set(
            &scene.daemon.facts,
            Some(UpdateFacts {
                reboot_pending: true,
                install_status: None,
            }),
        );
        scene.tick().await;
        record(&scene, "reboot-pending", "waiting for its first boot");

        // 10. The re-check, refused by the workspace probe.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.daemon.will_check(Ok(Settled::Unready(Unready {
            status: "degraded".to_string(),
            kind: "read-only".to_string(),
            detail: "/mos is mounted read-only".to_string(),
        })));
        scene.tick().await;
        record(&scene, "workspace-unready", "degraded read-only");

        // 11. The re-check ran and failed.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene
            .daemon
            .will_check(Ok(Settled::Failed("state file corrupt".to_string())));
        scene.tick().await;
        record(&scene, "recheck-failed", "state file corrupt");

        // 12. The re-check was not admitted at all.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.daemon.will_check(Err(Refusal::Unavailable(
            "/usr/bin/rauc-update is not present on this image".to_string(),
        )));
        scene.tick().await;
        record(&scene, "recheck-refused", "rauc-update is not present");

        // 13. §5: the metadata names something else. The staged bundle is
        //     superseded rather than provably withdrawn, so it is not deleted.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene
            .daemon
            .will_check(Ok(Settled::Done(candidate("mos-1.6.0.raucb", "1.6.0"))));
        scene.tick().await;
        record(&scene, "superseded", "the check now names mos-1.6.0.raucb");
        assert_eq!(
            scene.daemon.staged.lock().expect("staged").as_deref(),
            Some(BUNDLE),
            "a superseded bundle is left where a human can still install it"
        );

        // 14. §6 at its load-bearing site: between the re-check, which is the
        //     first point the version about to be written is known, and the
        //     install.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.suppress("1.5.0");
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.tick().await;
        record(&scene, "version-suppressed", "rootfs.1");
        assert!(
            scene.daemon.installs().is_empty(),
            "the refusal that closes the loop must stop the install"
        );

        // 15. The same store error at the same site, which is the one whose
        //     closed side is refusing the install.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.corrupt_the_suppression_store();
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.tick().await;
        record(&scene, "suppression-unreadable", "parse ");

        // 16. The install route's own refusal, verbatim.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        FakeDaemon::set(
            &scene.daemon.install,
            Err("an update install is already running; query GetUpdateState and retry".to_string()),
        );
        scene.tick().await;
        record(&scene, "install-refused", "already running");

        // 17. The safe-to-reboot gate, reached the only way the driver can
        //     reach it: an install of its own that finished.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.tick().await;
        FakeDaemon::set(
            &scene.daemon.facts,
            Some(UpdateFacts {
                reboot_pending: false,
                install_status: Some("done".to_string()),
            }),
        );
        FakeDaemon::set(
            &scene.daemon.reboot,
            Err(
                "reboot refused by the safe-to-reboot gate: mos-vision reports blocking"
                    .to_string(),
            ),
        );
        scene.tick().await;
        record(&scene, "reboot-gate-closed", "safe-to-reboot gate");

        // 18. The window again, on the reboot rather than on the install: an
        //     operator who shut it between the two is obeyed by both.
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.tick().await;
        scene.rewrite(&shut_window(), "window");
        FakeDaemon::set(
            &scene.daemon.facts,
            Some(UpdateFacts {
                reboot_pending: false,
                install_status: Some("done".to_string()),
            }),
        );
        scene.tick().await;
        record(
            &scene,
            "outside-window",
            "outside every configured maintenance window",
        );
        assert!(
            !scene.daemon.calls().contains(&Call::Reboot),
            "a shut window is asked before the gate is"
        );

        assert_eq!(
            observed,
            reasons_the_driver_can_mint(),
            "every reason the driver can record must be reachable from a test"
        );
        assert_eq!(observed.len(), 15, "the vocabulary is fifteen reasons");
    }

    /// PLAN-071 U4, and the plan's own acceptance for the slice: a bad bundle
    /// installs **once**.
    ///
    /// The loop this refuses is the whole reason §6 exists. The fallback
    /// leaves the device on the older system, which makes the failed version
    /// strictly newer again, so the next check selects it and the next window
    /// installs it — forever, once per window, unless the second pass selects
    /// nothing.
    #[tokio::test]
    async fn a_bad_bundle_cycle_ends_with_the_second_pass_selecting_nothing() {
        let mut scene = Scene::auto(&open_window(), "window");

        // Pass one: check, fetch, re-check, install.
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene
            .daemon
            .will_fetch(Ok(Settled::Done(BUNDLE.to_string())));
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.cadence.advance_past_the_check_interval();
        scene.tick().await;
        assert_eq!(
            scene.daemon.installs(),
            vec![BUNDLE.to_string()],
            "the pass installs what the current metadata still names"
        );
        assert_eq!(
            scene.daemon.calls().last(),
            Some(&Call::Resume(None)),
            "a pass that ran to its end clears every reason it was refused for"
        );

        // The install finishes and the reboot is owed and taken.
        FakeDaemon::set(
            &scene.daemon.facts,
            Some(UpdateFacts {
                reboot_pending: false,
                install_status: Some("done".to_string()),
            }),
        );
        scene.tick().await;
        assert!(
            scene.daemon.calls().contains(&Call::Reboot),
            "`rebootPolicy = window` reboots inside the same window"
        );

        // The device boots the new slot, the slot fails to confirm, the
        // bootloader spends its credits and falls back. What the state
        // refresh writes when it sees that is this record, through the real
        // store.
        scene.suppress("1.5.0");

        // Pass two. The failed version is newer than the running system
        // again, so the check selects it again — and this is where the loop
        // either closes or does not.
        let before = scene.daemon.installs().len();
        let mark = scene.daemon.calls().len();
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.cadence.advance_past_the_check_interval();
        scene.tick().await;

        assert_eq!(
            scene.daemon.installs().len(),
            before,
            "the second automatic pass must install nothing"
        );
        assert!(
            !scene.daemon.calls()[mark..].contains(&Call::Fetch),
            "nor pay a metered link for the same bad bundle again"
        );
        let (reason, detail) = scene
            .daemon
            .deferrals()
            .pop()
            .expect("the refusal is recorded, not silent");
        assert_eq!(reason, "version-suppressed");
        assert!(detail.contains("rolled back"), "got: {detail}");

        // And it stays closed: an operator who has not cleared the record
        // gets the same answer on the pass after that.
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.cadence.advance_past_the_check_interval();
        scene.tick().await;
        assert_eq!(scene.daemon.installs().len(), before);

        // And closed at its load-bearing site too. With no check due, the
        // candidate the fetch step consults is stale, so the only
        // consultation between the current metadata and the slot is the one
        // immediately before the install — the first point at which the
        // version about to be written is known rather than guessed at.
        FakeDaemon::set(&scene.daemon.available, None);
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.tick().await;
        assert_eq!(
            scene.daemon.installs().len(),
            before,
            "the refusal immediately before the install must hold on its own"
        );
        assert_eq!(
            scene.daemon.deferrals().pop().expect("recorded").0.as_str(),
            "version-suppressed"
        );

        // Cleared explicitly by an operator — the one thing that lifts it —
        // and the device installs it again, because the operator has been
        // told and is choosing.
        scene
            .daemon
            .suppression
            .clear("1.5.0")
            .expect("clear the suppression")
            .expect("the version was suppressed");
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.cadence.advance_past_the_check_interval();
        scene.tick().await;
        assert_eq!(
            scene.daemon.installs().len(),
            before + 1,
            "a cleared suppression is a refusal an operator lifted"
        );
    }

    /// PLAN-071 §7: an automatic install requires a clock the device
    /// believes, and **checks and fetches are unaffected** — asserted here as
    /// well as the refusal, because a test of the refusal alone would pass
    /// against a stricter product than the one designed, one where a
    /// clockless device stops even discovering updates.
    #[tokio::test]
    async fn an_untrusted_clock_defers_the_install_and_leaves_the_check_and_fetch_alone() {
        let mut scene = Scene::auto(&open_window(), "manual");
        FakeDaemon::set(&scene.daemon.clock, untrusted_clock());
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene
            .daemon
            .will_fetch(Ok(Settled::Done(BUNDLE.to_string())));
        scene.cadence.advance_past_the_check_interval();
        scene.tick().await;

        let calls = scene.daemon.calls();
        assert!(calls.contains(&Call::Check), "the check is not time-keyed");
        assert!(calls.contains(&Call::Fetch), "nor is the fetch");
        assert!(
            scene.daemon.installs().is_empty(),
            "the install is: a maintenance window is UTC wall-clock"
        );
        let (reason, detail) = scene.daemon.the_deferral();
        assert_eq!(reason, "clock-untrusted");
        assert!(
            detail.contains("offline-degraded") && detail.contains("has not advanced since boot"),
            "the deferral names both limbs so an operator knows which to fix: {detail}"
        );

        // The second limb: a saved floor advancing since boot is a clock this
        // device believes even while timesyncd still reports `offline-degraded`.
        FakeDaemon::set(
            &scene.daemon.clock,
            ClockTrust {
                status: Some(SyncStatus::OfflineDegraded),
                floor_advanced: true,
            },
        );
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.tick().await;
        assert_eq!(
            scene.daemon.installs(),
            vec![BUNDLE.to_string()],
            "either limb of §7's predicate admits the install"
        );
    }

    /// PLAN-071 §5: a release withdrawn between the fetch and the window is
    /// deleted rather than installed, and the automatic path is the only one
    /// that is stricter here.
    #[tokio::test]
    async fn a_withdrawn_bundle_is_discarded_rather_than_installed() {
        let mut scene = Scene::auto(&open_window(), "manual");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.daemon.will_check(Ok(Settled::NoneCompatible));

        scene.tick().await;

        assert!(scene.daemon.installs().is_empty());
        assert_eq!(
            scene.daemon.calls().last(),
            Some(&Call::Discard(
                "the current metadata no longer names it".to_string()
            ))
        );
        assert!(
            scene.daemon.deferrals().is_empty(),
            "a withdrawal is not a refusal to record; the state says the bundle is gone"
        );
    }

    /// PLAN-071 §2 step 4: `manual` installs and stops at `reboot-required`.
    /// "Install automatically" and "reboot automatically" are not the same
    /// promise, and `manual` is the default.
    #[tokio::test]
    async fn a_manual_reboot_policy_installs_and_stops() {
        let mut scene = Scene::auto(&open_window(), "manual");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.tick().await;
        assert_eq!(scene.daemon.installs(), vec![BUNDLE.to_string()]);

        FakeDaemon::set(
            &scene.daemon.facts,
            Some(UpdateFacts {
                reboot_pending: false,
                install_status: Some("done".to_string()),
            }),
        );
        scene.tick().await;
        assert!(
            !scene.daemon.calls().contains(&Call::Reboot),
            "the install happened; the reboot is a human's"
        );
    }

    /// PLAN-071 §2 step 4, the driver's half of the never-arms-the-override
    /// invariant: against a closed gate the automatic path defers, repeats,
    /// and reports the escape hatch it cannot itself take.
    ///
    /// The structural half is the type: [`AutoRoutes`] carries no method that
    /// arms the override, so the driver holds no capability to reach one.
    /// What a test adds is that the driver does not instead find some other
    /// way through — it reboots only when the gate itself opens. The same
    /// invariant against the real reboot gate, where an armed override would
    /// be visible in the recorded state, is
    /// `crate::bus::tests::the_automatic_path_against_a_closed_gate_arms_no_override`.
    #[tokio::test]
    async fn a_closed_reboot_gate_defers_and_the_driver_takes_no_way_around_it() {
        const CLOSED: &str = "reboot refused by the safe-to-reboot gate: mos-vision reports \
                              blocking: recording. An administrator can lift a health block \
                              with SetRebootOverride (POST /api/v1/update/reboot-override).";

        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.tick().await;
        FakeDaemon::set(
            &scene.daemon.facts,
            Some(UpdateFacts {
                reboot_pending: false,
                install_status: Some("done".to_string()),
            }),
        );
        FakeDaemon::set(&scene.daemon.reboot, Err(CLOSED.to_string()));

        for _ in 0..3 {
            scene.tick().await;
        }

        let deferrals = scene.daemon.deferrals();
        assert_eq!(
            deferrals.len(),
            3,
            "a permanently blocking application permanently defers, visibly: {deferrals:?}"
        );
        for (reason, detail) in &deferrals {
            assert_eq!(reason, "reboot-gate-closed");
            assert_eq!(
                detail, CLOSED,
                "the gate's refusal is recorded verbatim, override sentence and all"
            );
        }

        // The reboot is owed and stays owed: the gate opening is the only
        // thing that discharges it, and the driver did not quietly drop it
        // over three refused attempts.
        FakeDaemon::set(&scene.daemon.reboot, Ok(()));
        scene.tick().await;
        assert_eq!(
            scene
                .daemon
                .calls()
                .iter()
                .filter(|call| **call == Call::Reboot)
                .count(),
            4,
            "the next window re-attempts, and the gate is what decides"
        );
        assert_eq!(
            scene.daemon.calls().last(),
            Some(&Call::Resume(None)),
            "a reboot that went through clears every reason it was refused for"
        );
    }

    /// `policy = "off"` initiates nothing, and an owed automatic reboot is
    /// dropped rather than carried: the operator has just said the device
    /// initiates nothing, so a pending slot stays pending and a human reboots
    /// into it.
    #[tokio::test]
    async fn switching_to_off_drops_the_reboot_the_driver_owed() {
        let mut scene = Scene::auto(&open_window(), "window");
        FakeDaemon::set(&scene.daemon.staged, Some(BUNDLE.to_string()));
        scene.daemon.will_check(Ok(Settled::Done(the_candidate())));
        scene.tick().await;
        FakeDaemon::set(
            &scene.daemon.facts,
            Some(UpdateFacts {
                reboot_pending: false,
                install_status: Some("done".to_string()),
            }),
        );

        scene.write_document(r#"{"policy": "off", "source": {"url": "http://mirror/tuf"}}"#);
        scene.cadence.advance_past_the_check_interval();
        scene.tick().await;
        assert!(
            !scene.daemon.calls().contains(&Call::Reboot),
            "`off` initiates nothing, including the reboot it owed a moment ago"
        );

        // And the owed reboot is gone rather than parked: turning `auto` back
        // on does not reboot on the strength of an install the operator has
        // since disowned.
        FakeDaemon::set(&scene.daemon.staged, None);
        scene.rewrite(&open_window(), "window");
        scene.daemon.will_check(Ok(Settled::NoneCompatible));
        scene.cadence.advance_past_the_check_interval();
        scene.tick().await;
        assert!(!scene.daemon.calls().contains(&Call::Reboot));
    }

    /// A document that exists and does not parse: the device initiates
    /// nothing at all — no check, no fetch, no install — because there is no
    /// selection to read a mode out of, and PLAN-070 §5.1 forbids falling
    /// back to the baked channel here.
    #[tokio::test]
    async fn an_unparseable_document_initiates_nothing() {
        let mut scene = Scene::auto(&open_window(), "window");
        scene.write_document("{not json");
        scene.cadence.advance_past_the_check_interval();

        scene.tick().await;

        assert!(
            scene.daemon.calls().is_empty(),
            "an unreadable document is not a device that goes and checks: {:?}",
            scene.daemon.calls()
        );
    }
}

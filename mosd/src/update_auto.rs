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
//! Two things the automatic path adds, and one it refuses to add:
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
//! - **It never arms the reboot-gate override** (§2 step 4). That is a
//!   human's judgement that this reboot outranks what an application
//!   declared it must not be interrupted for, and a machine cannot make it.
//!   Enforced by construction: [`AutoRoutes`] has no method that arms it.
//!
//! **What is not here yet, and it matters.** PLAN-071 §6's version
//! suppression is U4 and is not built. Without it, a bundle that installs
//! and fails to confirm is selected again by the next check, installed
//! again, and fails again — the reboot loop §6 exists to prevent. The one
//! place U4 belongs is marked below.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::update_lifecycle::{Available, Refusal, Settled};
use crate::update_policy::{self, LoadedPolicy, RebootPolicy, UpdateMode};

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

/// The driver's own state between ticks.
pub struct AutoDriver {
    routes: Arc<dyn AutoRoutes>,
    /// When a check was last attempted. Attempt-based rather than
    /// success-based: a check refused by policy must not retry every tick,
    /// and the cadence an operator set is a cadence of attempts.
    last_check: Instant,
    stage: Stage,
}

/// Run the driver forever. One task, spawned by `main` on a production
/// daemon only.
pub async fn run(routes: Arc<dyn AutoRoutes>) {
    let mut driver = AutoDriver::new(routes);
    loop {
        tokio::time::sleep(TICK).await;
        driver.tick().await;
    }
}

impl AutoDriver {
    pub fn new(routes: Arc<dyn AutoRoutes>) -> Self {
        Self {
            routes,
            // The first check falls one interval after start, which is what
            // the cadence this replaces did by sleeping before its first
            // check: a device that reboots hourly must not check hourly.
            last_check: Instant::now(),
            stage: Stage::Idle,
        }
    }

    /// One turn of the driver.
    pub async fn tick(&mut self) {
        let loaded = self.routes.policy();
        if loaded.error.is_some() {
            // A document that exists and does not parse refuses every
            // restricted action already, and `load` answers the DEFAULTS
            // beside the error — so reading a mode out of it would be
            // reading a mode nobody wrote. The device initiates nothing
            // until the file is fixed; the operator's manual routes still
            // refuse with the error naming the file.
            return;
        }
        match loaded.policy.mode {
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
            UpdateMode::Auto => self.drive(&loaded).await,
        }
    }

    /// Step 1: the check cadence, shared by `check` and `auto`.
    async fn check_if_due(&mut self, loaded: &LoadedPolicy) {
        let interval = loaded.policy.check_interval_minutes;
        if interval == 0 {
            return;
        }
        if self.last_check.elapsed() < Duration::from_secs(interval.saturating_mul(60)) {
            return;
        }
        self.last_check = Instant::now();
        if let Err(refusal) = self.routes.check(SENDER).await {
            tracing::debug!(reason = refusal.message(), "automatic check skipped");
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
            let staged = self.routes.staged().await;
            if staged.as_deref().and_then(bundle_name) != Some(candidate.name.as_str())
                && let Err(refusal) = self.routes.fetch(SENDER).await
            {
                tracing::debug!(reason = refusal.message(), "automatic fetch skipped");
                return;
            }
        }
        // Step 3 — install, in the window, only what the metadata still names.
        if let Some(bundle) = self.routes.staged().await {
            self.install_if_allowed(loaded, &bundle).await;
        }
    }

    /// Step 3: the window, the pending-slot guard, the re-check, the install.
    async fn install_if_allowed(&mut self, loaded: &LoadedPolicy, bundle: &str) {
        // The same refusal `InstallUpdate` answers an operator with: the
        // maintenance window, which `auto` requires the document to name.
        if let Some(reason) = update_policy::install_refusal(loaded, Utc::now()) {
            self.defer("outside-window", &reason);
            return;
        }
        let Some(facts) = self.routes.facts().await else {
            self.defer("slot-status-unknown", "RAUC did not answer the slot query");
            return;
        };
        if facts.reboot_pending {
            self.defer(
                "reboot-pending",
                "a slot is already installed and waiting for its first boot",
            );
            return;
        }
        // The re-check of §5. Its answer is also where PLAN-071 §6's version
        // suppression goes when U4 lands: the candidate below carries the
        // version, and a suppressed one is refused HERE, between the re-check
        // and the install, with `version-suppressed` as its deferral reason.
        // Until it does, a bundle that installs and fails to confirm is
        // selected again by the next check and installed again — the loop §6
        // exists to close.
        let named = match self.routes.check(SENDER).await {
            Ok(Settled::Done(candidate)) => Some(candidate),
            Ok(Settled::NoneCompatible) => None,
            Ok(Settled::Unready(unready)) => {
                self.defer("workspace-unready", &unready.reason());
                return;
            }
            Ok(Settled::Failed(reason)) => {
                self.defer("recheck-failed", &reason);
                return;
            }
            Err(refusal) => {
                self.defer("recheck-refused", refusal.message());
                return;
            }
        };
        match named {
            // The metadata still names it: install.
            Some(candidate) if Some(candidate.name.as_str()) == bundle_name(bundle) => {}
            // It names something else. The staged bundle is superseded rather
            // than provably withdrawn — a check reports the selection, not the
            // whole target list — so it is not deleted, and the next pass
            // fetches what was named.
            Some(candidate) => {
                self.defer(
                    "superseded",
                    &format!("the check now names {}", candidate.name),
                );
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
        }
        match self.routes.install(SENDER, bundle).await {
            Ok(()) => {
                tracing::warn!(bundle, "automatic install started");
                self.stage = Stage::Installing;
            }
            Err(refusal) => self.defer("install-refused", &refusal),
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
            self.defer("outside-window", &reason);
            return;
        }
        if let Err(refusal) = self.routes.reboot(SENDER).await {
            // The gate is closed: an install in flight, or a component
            // reporting a blocking health status. Automation defers and the
            // next window re-attempts. It does not arm the override — there
            // is no method on `AutoRoutes` to arm it with.
            self.defer("reboot-gate-closed", &refusal);
            return;
        }
        // The machine is going down; the stage it leaves behind is moot.
        self.stage = Stage::Idle;
    }

    /// Say why an automatic step did not proceed.
    ///
    /// A log line today. PLAN-071 §2 wants it visible in the lifecycle as a
    /// `deferred` fact naming the reason and how long the pending slot has
    /// waited — that is U5, and every reason it names is already a call
    /// site here, so U5 replaces this body rather than hunting for them.
    fn defer(&self, reason: &str, detail: &str) {
        tracing::info!(reason, detail, "automatic update deferred");
    }
}

/// The target name a staged bundle path carries. `rauc-update` stages a
/// verified bundle under its target name and nothing else, so the file name
/// is what a check's selection is compared against.
fn bundle_name(bundle: &str) -> Option<&str> {
    Path::new(bundle).file_name().and_then(|name| name.to_str())
}

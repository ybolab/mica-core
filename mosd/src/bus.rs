//! D-Bus service implementation for `com.mos.mosd`.
//!
//! Exposes the settings tree and the live-state tree on the bus as the
//! `com.mos.mosd1` interface at [`OBJECT_PATH`], owned under [`BUS_NAME`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mosd_settings::{Settings, SettingsError, Store, json_path_get};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use zbus::fdo;
use zbus::message::Header;
use zbus::object_server::{InterfaceRef, SignalEmitter};

use crate::apply_queue::{ApplyJob, ApplyQueue, TaskRecord};
use crate::diagnostics::{FailureEvidenceSource, UnavailableFailureEvidence};
use crate::network_state::{NetworkState, UnavailableNetworkState};
use crate::power::PowerControl;
use crate::rauc::{self, RaucClient};
use crate::reconciler::Reconciler;
use crate::reconciler::network::WireguardRotate;
use crate::scan::Registry;
use crate::storage_status::{self, PressureTracker, StorageStatusSource, UnavailableStorageStatus};
use crate::system_info::{self, SystemInfoSource, UnavailableSystemInfo};
use crate::telemetry::{self, TelemetrySource, UnavailableTelemetry};
use crate::time_status::{TimeStatusSource, UnavailableTimeStatus, status_json};
use crate::transient;
use crate::update_auto::{AutoRoutes, UpdateFacts};
use crate::update_lifecycle::{
    Available, DEFAULT_WORKSPACE_ROOT, LifecycleHost, NoClient, Refusal, Settled, UpdateClient,
    UpdateLifecycle,
};
use crate::update_policy::{LoadedPolicy, PolicyStore};

/// Well-known bus name owned by the daemon.
pub const BUS_NAME: &str = "com.mos.mosd";
/// Object path the service is registered at.
pub const OBJECT_PATH: &str = "/com/mos/mosd";

/// True when `a` and `b` overlap by dot segments in either direction:
/// one path is a segment-wise prefix of the other. The root path (`""` or
/// `"."`) matches everything.
pub fn paths_overlap(a: &str, b: &str) -> bool {
    let a = if a == "." { "" } else { a };
    let b = if b == "." { "" } else { b };
    if a.is_empty() || b.is_empty() {
        return true;
    }
    let mut a = a.split('.');
    let mut b = b.split('.');
    loop {
        match (a.next(), b.next()) {
            (Some(x), Some(y)) if x == y => {}
            (Some(_), Some(_)) => return false,
            _ => return true,
        }
    }
}

/// Mutable trees guarded by one lock so settings writes and live-state
/// updates stay consistent.
struct Inner {
    settings: Settings,
    state: Value,
}

/// The `com.mos.mosd1` service: settings tree, live-state tree, store,
/// reconcilers, the power control, the update installer client and the shadow
/// file a transient root password is written into.
pub struct MosdService {
    store: Store,
    reconcilers: Arc<Vec<Box<dyn Reconciler>>>,
    power: Box<dyn PowerControl>,
    /// The update installer (RAUC) client. `Arc` rather than `Box` because a
    /// running install outlives the bus call that started it: the background
    /// task holds its own handle. Defaults to [`rauc::DryRunRauc`]; production
    /// swaps in the real client via [`Self::with_rauc`].
    rauc: Arc<dyn RaucClient>,
    /// True while a bundle install is in flight. `InstallUpdate` refuses a
    /// second install rather than queueing it: RAUC itself answers
    /// `AlreadyInstalling` to a concurrent request, and refusing here keeps
    /// the recorded `update.install` entry describing exactly one operation.
    installing: Arc<AtomicBool>,
    shadow_path: PathBuf,
    /// `Arc` so the install background task can record its outcome into the
    /// live-state tree after the bus call that spawned it has returned.
    inner: Arc<RwLock<Inner>>,
    /// Serializes every settings reconcile and the secret/key mutations whose
    /// read-modify-write cycles must not interleave. Data readers never take
    /// this lock.
    apply_lock: Arc<Mutex<()>>,
    /// Work queue and bounded lifecycle history for settings applies.
    apply_queue: Arc<ApplyQueue>,
    /// The service registry the scan task fills ([`crate::scan`]), shared so
    /// that `ForgetService` drops an entry from the same table the scan
    /// publishes from — one table, so the bus surface and the live-state tree
    /// cannot disagree about which services exist.
    ///
    /// `None` when no scan was constructed (dry run), which is the one state
    /// in which `ForgetService` has nothing to act on.
    registry: Option<Arc<Registry>>,
    /// The WireGuard key rotation `RotateWireguardKey` calls through.
    ///
    /// Defaults to [`NoRotation`], which draws no key at all: a daemon that
    /// was never handed a rotation is one running against no STATE partition,
    /// and the honest answer there is that there is nowhere to put a key.
    wireguard: Arc<dyn WireguardRotate>,
    /// Read-only live network observation. The default is unavailable so
    /// tests and dry-run instances never inspect the host network.
    network_state: Arc<dyn NetworkState>,
    /// Read-only time-synchronization observation, on the same default for
    /// the same reason.
    time_status: Arc<dyn TimeStatusSource>,
    /// Read-only storage observation, on the same default again.
    storage_status: Arc<dyn StorageStatusSource>,
    /// The low-space hysteresis state the storage surface classifies against.
    /// Lives with the service rather than with the observer because it is the
    /// REPORTED state, and it has to survive an observer being reattached.
    storage_pressure: Arc<PressureTracker>,
    /// Read-only system-information observation (PLAN-052), on the same
    /// unavailable default for the same reason.
    system_info: Arc<dyn SystemInfoSource>,
    /// Read-only board telemetry (PLAN-052), on the same default.
    telemetry: Arc<dyn TelemetrySource>,
    /// Read-only failure evidence for the diagnostic snapshot (PLAN-052):
    /// failed units and a bounded journal excerpt. Same default.
    failure_evidence: Arc<dyn FailureEvidenceSource>,
    /// The update lifecycle (check/fetch machine, policy, reboot gate).
    /// Constructed with [`NoClient`] and a fileless policy store, so a
    /// dry-run daemon can neither spawn the update client nor read a host
    /// policy file; production attaches both via [`Self::with_update`].
    update: Arc<UpdateLifecycle>,
}

/// The lifecycle's window onto this service: it records under
/// `update.lifecycle` and reads the `health` subtree, and nothing else.
struct InnerLifecycleHost(Arc<RwLock<Inner>>);

#[async_trait::async_trait]
impl LifecycleHost for InnerLifecycleHost {
    async fn record(&self, lifecycle: Value) {
        let mut inner = self.0.write().await;
        rauc::update_entry(&mut inner.state).insert("lifecycle".into(), lifecycle);
        drop(inner);
    }

    async fn health(&self) -> Value {
        let inner = self.0.read().await;
        inner
            .state
            .get("health")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
    }
}

/// The rotation a daemon with no key store has: none.
///
/// The dry-run shape [`rauc::DryRunRauc`] and [`crate::power::DryRunPower`]
/// both take — a default that cannot touch the host, so only `main.rs`, which
/// alone knows the daemon is running on a device, can attach one that can.
struct NoRotation;

#[async_trait::async_trait]
impl WireguardRotate for NoRotation {
    async fn rotate_key(&self, _iface: &str) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("this daemon has no WireGuard key store"))
    }
}

impl MosdService {
    /// Build the service around loaded `settings` and an initial live-state
    /// root (an empty object, or `{"dry_run": true}` in dry-run mode).
    ///
    /// `shadow_path` is a parameter for the same reason every reconciler path
    /// is: a test points it at a temporary file and can then drive the real
    /// transient-password method without touching the host's `/etc/shadow`.
    pub fn new(
        store: Store,
        settings: Settings,
        reconcilers: Vec<Box<dyn Reconciler>>,
        power: Box<dyn PowerControl>,
        shadow_path: PathBuf,
        state: Value,
    ) -> Self {
        let installing = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(RwLock::new(Inner { settings, state }));
        let update = Arc::new(UpdateLifecycle::new(
            Arc::new(NoClient),
            PolicyStore::defaults(),
            Arc::new(InnerLifecycleHost(Arc::clone(&inner))),
            Arc::clone(&installing),
            PathBuf::from(DEFAULT_WORKSPACE_ROOT),
        ));
        Self {
            store,
            reconcilers: Arc::new(reconcilers),
            power,
            rauc: Arc::new(rauc::DryRunRauc),
            installing,
            shadow_path,
            inner,
            apply_lock: Arc::new(Mutex::new(())),
            apply_queue: Arc::new(ApplyQueue::new()),
            registry: None,
            wireguard: Arc::new(NoRotation),
            network_state: Arc::new(UnavailableNetworkState),
            time_status: Arc::new(UnavailableTimeStatus),
            storage_status: Arc::new(UnavailableStorageStatus),
            system_info: Arc::new(UnavailableSystemInfo),
            telemetry: Arc::new(UnavailableTelemetry),
            failure_evidence: Arc::new(UnavailableFailureEvidence),
            storage_pressure: Arc::new(PressureTracker::default()),
            update,
        }
    }

    /// Attach the update client and policy store, rebuilding the lifecycle
    /// around them. A builder step for the reason [`Self::with_rauc`] is: the
    /// defaults touch nothing on the host, and only `main.rs` knows the
    /// daemon runs on a device with a client binary and a policy file.
    #[must_use]
    pub fn with_update(mut self, client: Arc<dyn UpdateClient>, policy: PolicyStore) -> Self {
        self.update = Arc::new(UpdateLifecycle::new(
            client,
            policy,
            Arc::new(InnerLifecycleHost(Arc::clone(&self.inner))),
            Arc::clone(&self.installing),
            self.update.workspace_root().to_path_buf(),
        ));
        self
    }

    /// Relocate the `/mos/updates` workspace the lifecycle records `ready`
    /// paths from and admits installs from. Tests only (the client's
    /// `RAUC_UPDATE_ROOT`, which `main.rs` forwards); the default is the
    /// contract.
    #[must_use]
    pub fn with_update_workspace(mut self, root: PathBuf) -> Self {
        self.update = Arc::new(UpdateLifecycle::new(
            self.update.client(),
            self.update.policy(),
            Arc::new(InnerLifecycleHost(Arc::clone(&self.inner))),
            Arc::clone(&self.installing),
            root,
        ));
        self
    }

    /// The lifecycle handle `main.rs` gives the auto-check task.
    pub fn update_handle(&self) -> Arc<UpdateLifecycle> {
        Arc::clone(&self.update)
    }

    /// Attach the WireGuard key rotation.
    ///
    /// A builder step for the same reason [`Self::with_rauc`] is: the default
    /// touches nothing, and a test that has no state directory must not be
    /// able to draw a key into one.
    #[must_use]
    pub fn with_wireguard(mut self, wireguard: Arc<dyn WireguardRotate>) -> Self {
        self.wireguard = wireguard;
        self
    }

    /// Attach the production network observer.
    #[must_use]
    pub fn with_network_state(mut self, network_state: Arc<dyn NetworkState>) -> Self {
        self.network_state = network_state;
        self
    }

    /// Attach the production time-synchronization observer.
    #[must_use]
    pub fn with_time_status(mut self, time_status: Arc<dyn TimeStatusSource>) -> Self {
        self.time_status = time_status;
        self
    }

    /// Attach the production storage observer.
    ///
    /// The default observes nothing, so a dry-run daemon or a test never
    /// reads the host's sysfs, mount table or media — and never refuses an
    /// install over free space it cannot see.
    #[must_use]
    pub fn with_storage_status(mut self, storage_status: Arc<dyn StorageStatusSource>) -> Self {
        self.storage_status = storage_status;
        self
    }

    /// Attach the system-information observer (`GetSystemInfo`).
    ///
    /// Same shape and same reason as [`Self::with_storage_status`]: only
    /// `main.rs` knows the daemon runs on a device.
    #[must_use]
    pub fn with_system_info(mut self, system_info: Arc<dyn SystemInfoSource>) -> Self {
        self.system_info = system_info;
        self
    }

    /// Attach the board telemetry adapter (`GetTelemetry`).
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Arc<dyn TelemetrySource>) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// Attach the failure-evidence source (`GetFailureEvidence`).
    #[must_use]
    pub fn with_failure_evidence(
        mut self,
        failure_evidence: Arc<dyn FailureEvidenceSource>,
    ) -> Self {
        self.failure_evidence = failure_evidence;
        self
    }

    /// Attach the update installer client.
    ///
    /// A builder step rather than a [`Self::new`] parameter for the same
    /// reason as [`Self::with_service_registry`]: the default —
    /// [`rauc::DryRunRauc`], which never touches the host — is the correct
    /// client for every test, and only `main.rs` ever has a production
    /// [`rauc::Rauc`] to hand over.
    #[must_use]
    pub fn with_rauc(mut self, rauc: Arc<dyn RaucClient>) -> Self {
        self.rauc = rauc;
        self
    }

    /// Attach the service registry this daemon's scan task fills.
    ///
    /// A separate step rather than a [`Self::new`] parameter because the
    /// registry only exists when a scan does: the daemon is built the same way
    /// either way, and a dry-run daemon — which constructs no scan at all —
    /// does not have to name a registry it will never have.
    #[must_use]
    pub fn with_service_registry(mut self, registry: Arc<Registry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Write the service registry into the live-state tree under
    /// [`crate::scan::STATE_KEY`], replacing it wholesale.
    ///
    /// The registry is rendered as one value rather than patched key by key so
    /// that `instance_collision`, which is a property of the whole table
    /// rather than of one entry, is never observable half-applied.
    pub async fn publish_services(&self, services: Value) {
        let mut inner = self.inner.write().await;
        if let Some(root) = inner.state.as_object_mut() {
            root.insert(crate::scan::STATE_KEY.to_string(), services);
        }
        drop(inner);
    }

    /// Clones of settings and live state for unit-test assertions.
    #[cfg(test)]
    pub async fn trees(&self) -> (Value, Value) {
        let inner = self.inner.read().await;
        let settings = inner.settings.get("").unwrap_or(Value::Null);
        (settings, inner.state.clone())
    }

    /// Log a power request from `sender` and record it in the live-state tree
    /// under `power`, with `update_warning` — an unconfirmed-slot warning, when
    /// there is one — recorded beside it (absent key when there is none, the
    /// same convention the settings tree uses for optional values).
    ///
    /// Always called BEFORE the action: once systemd starts tearing the
    /// machine down there may be no system left to log on.
    async fn note_power_request(&self, action: &str, sender: &str, update_warning: Option<String>) {
        tracing::warn!(action, sender, "power action requested");
        let mut entry = serde_json::Map::new();
        entry.insert("last_action".into(), Value::String(action.to_string()));
        entry.insert("requested_by".into(), Value::String(sender.to_string()));
        if let Some(warning) = update_warning {
            entry.insert("update_warning".into(), Value::String(warning));
        }
        let mut inner = self.inner.write().await;
        if let Some(root) = inner.state.as_object_mut() {
            root.insert("power".to_string(), Value::Object(entry));
        }
        drop(inner);
    }

    /// The booted slot for the system-information surface, or `None`.
    ///
    /// Bounded and non-fatal, [`Self::reboot_update_warning`]'s reasoning:
    /// an installer that is absent, wedged or slow makes the `slot` member
    /// absent with a reason, never the whole answer. Failures are logged.
    async fn slot_evidence(&self) -> Option<system_info::SlotEvidence> {
        const SLOT_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
        let query = async {
            let slots = self.rauc.slot_status().await?;
            let primary = self.rauc.primary().await?;
            anyhow::Ok((slots, primary))
        };
        match tokio::time::timeout(SLOT_QUERY_TIMEOUT, query).await {
            Ok(Ok((slots, primary))) => Some(system_info::SlotEvidence {
                booted: rauc::booted_slot(&slots).cloned(),
                primary,
            }),
            Ok(Err(err)) => {
                tracing::debug!(error = %err, "slot status unavailable for system info");
                None
            }
            Err(_) => {
                tracing::warn!(
                    timeout = ?SLOT_QUERY_TIMEOUT,
                    "rauc did not answer the system-info slot query in time"
                );
                None
            }
        }
    }

    /// The unconfirmed-slot warning a reboot should carry, or `None`.
    ///
    /// Read fresh from RAUC rather than from the live-state tree: the recorded
    /// `update` entry is only as new as the last query, and the whole point of
    /// warning is the install that just happened.
    ///
    /// Bounded and non-fatal BY DESIGN: a reboot must go through even when
    /// RAUC is absent (v1 image, container, dry-run), wedged, or slow — a
    /// power action that hangs on an installer is strictly worse than one that
    /// misses a warning. Failures are logged and answered with `None`.
    async fn reboot_update_warning(&self) -> Option<String> {
        const SLOT_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
        let query = async {
            let slots = self.rauc.slot_status().await?;
            let primary = self.rauc.primary().await?;
            anyhow::Ok((slots, primary))
        };
        match tokio::time::timeout(SLOT_QUERY_TIMEOUT, query).await {
            Ok(Ok((slots, primary))) => rauc::unconfirmed_slot_warning(&slots, primary.as_deref()),
            Ok(Err(err)) => {
                tracing::debug!(error = %err, "slot status unavailable before reboot; proceeding");
                None
            }
            Err(_) => {
                tracing::warn!(
                    timeout = ?SLOT_QUERY_TIMEOUT,
                    "rauc did not answer the pre-reboot slot query in time; proceeding"
                );
                None
            }
        }
    }

    /// Reboot the machine on behalf of `sender`.
    ///
    /// Update-aware: the slot status is read first, and a slot that is
    /// installed-but-not-confirmed — a reboot into it burns one of its
    /// boot attempts — is logged and recorded in the `power` live-state entry
    /// BEFORE the reboot fires. A warning, not a refusal: booting the new slot
    /// is exactly what the operator installing an update wants, and the
    /// attempt-burning edge case (rebooting *again* before the health gate
    /// confirms) is one the operator must be able to drive through anyway.
    ///
    /// Split out from the D-Bus method so unit tests can drive it without
    /// forging a message header.
    pub async fn request_reboot(&self, sender: &str) -> fdo::Result<()> {
        // The safe-to-reboot interlock, and the one REFUSAL on this path.
        // Distinct from the unconfirmed-slot warning below, which stays a
        // warning: booting a fresh slot is what an updating operator wants,
        // while rebooting through an application's declared blocking work —
        // or through an install mid-write — is what nobody wants. The gate
        // opens by the reporter clearing its status, the install finishing,
        // or a bounded audited override (`SetRebootOverride`).
        if let Some(refusal) = self.update.reboot_refusal().await {
            tracing::warn!(sender, refusal, "reboot refused by the safe-to-reboot gate");
            return Err(fdo::Error::AccessDenied(refusal));
        }
        let warning = self.reboot_update_warning().await;
        if let Some(warning) = &warning {
            tracing::warn!(warning, "rebooting with an unconfirmed update slot");
        }
        self.note_power_request("reboot", sender, warning).await;
        self.power
            .reboot()
            .await
            .map_err(|err| fdo::Error::Failed(format!("reboot: {err}")))
    }

    /// Power the machine off on behalf of `sender`.
    ///
    /// No unconfirmed-slot warning here, deliberately: a power-off does not
    /// boot anything, so it spends no boot attempt. The attempt is spent by
    /// whatever powers the machine back ON, which is not an event mosd can
    /// see, let alone warn about.
    pub async fn request_power_off(&self, sender: &str) -> fdo::Result<()> {
        self.note_power_request("power_off", sender, None).await;
        self.power
            .power_off()
            .await
            .map_err(|err| fdo::Error::Failed(format!("power off: {err}")))
    }

    /// Install the update bundle at absolute path `bundle_path`, on behalf of
    /// `sender`. Validates the path, refuses a concurrent install, records
    /// `update.install` as `running`, and returns — the install itself runs on
    /// a background task that records `done`/`failed` (plus a fresh status
    /// query) when RAUC reports completion. Neither the service lock nor the
    /// bus dispatcher is held across the install.
    ///
    /// Split out from the D-Bus method so unit tests can drive it without
    /// forging a message header.
    pub async fn request_install(&self, sender: &str, bundle_path: &str) -> fdo::Result<()> {
        // The maintenance-window policy: installs run only when the window
        // (when one is configured) is open. Before the path validation so a
        // refused operator learns the real reason first.
        if let Some(refusal) = self.update.install_refusal().await {
            tracing::warn!(sender, refusal, "install refused by update policy");
            return Err(fdo::Error::AccessDenied(refusal));
        }
        let bundle = rauc::validate_bundle_path(bundle_path).map_err(fdo::Error::InvalidArgs)?;
        // Only a verified bundle is handed to RAUC: a regular file inside
        // /mos/updates/verified, never a `.part`, never a file anywhere else
        // — the same rule for the staged path and an operator's explicit one.
        self.update
            .installable(&bundle)
            .map_err(fdo::Error::InvalidArgs)?;
        // PLAN-049's reserved update workspace, enforced at the one seam
        // where mos consumes DATA space for an update: the bundle staged
        // there before this call names it. An install admitted onto a DATA
        // tier with no workspace left is the failure the reservation exists
        // to prevent, and it is cheaper to refuse here than halfway through
        // writing a slot.
        //
        // A daemon whose observer sees nothing refuses nothing; see
        // `storage_status::install_refusal`.
        if let Ok(evidence) = self.storage_status.observe().await {
            let bundle_bytes = std::fs::metadata(&bundle)
                .map(|meta| meta.len())
                .unwrap_or(0);
            if let Some(refusal) =
                storage_status::install_refusal(evidence.data_tier(), &bundle, bundle_bytes)
            {
                return Err(fdo::Error::Failed(refusal));
            }
        }
        // The in-flight flag is taken BEFORE anything is recorded, in one
        // compare-exchange, so two racing calls cannot both proceed. It is
        // released only by the background task — including on install failure —
        // so an early return below must not happen after this point without
        // clearing it (there is none: the spawn is infallible).
        if self
            .installing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(fdo::Error::Failed(
                "an update install is already running; query GetUpdateState and retry".into(),
            ));
        }
        tracing::warn!(bundle = %bundle.display(), sender, "update install requested");
        let started = serde_json::json!({
            "status": "running",
            "bundle": bundle.to_string_lossy(),
            "requested_by": sender,
        });
        let mut inner = self.inner.write().await;
        rauc::update_entry(&mut inner.state).insert("install".into(), started);
        drop(inner);

        let rauc_client = Arc::clone(&self.rauc);
        let inner = Arc::clone(&self.inner);
        let installing = Arc::clone(&self.installing);
        let time_status = Arc::clone(&self.time_status);
        let sender = sender.to_string();
        tokio::spawn(async move {
            let result = rauc_client.install_bundle(&bundle).await;
            let outcome = match &result {
                Ok(()) => {
                    tracing::info!(bundle = %bundle.display(), "update install finished");
                    serde_json::json!({
                        "status": "done",
                        "bundle": bundle.to_string_lossy(),
                        "requested_by": sender,
                    })
                }
                Err(err) => {
                    tracing::error!(bundle = %bundle.display(), error = %err, "update install failed");
                    // PLAN-078 §5's diagnostic mitigation, and it is only a
                    // diagnostic: there is no cryptographic answer to a wrong
                    // clock. RAUC verifies a signer's window against the clock
                    // of the process doing the verifying, so a device with a
                    // grossly wrong RTC refuses every valid signer with the
                    // same sentence a real expiry produces. The facts are
                    // gathered here, while the failure is fresh, because a
                    // clock read minutes later by a separate query is a
                    // different clock.
                    //
                    // Read SOFTLY: a status source that does not answer leaves
                    // the member absent rather than turning a failed install
                    // into a failed record. Absent evidence is not evidence of
                    // a good clock, and install_failure_time_facts treats it
                    // as such.
                    let observed = time_status
                        .observe()
                        .await
                        .ok()
                        .map(|evidence| status_json(&evidence));
                    let error = format!("{err:#}");
                    let facts = rauc::install_failure_time_facts(
                        &error,
                        &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        observed,
                    );
                    serde_json::json!({
                        "status": "failed",
                        "bundle": bundle.to_string_lossy(),
                        "requested_by": sender,
                        "error": error,
                        "time": facts,
                    })
                }
            };
            // Refresh the whole update entry while the outcome is fresh, so
            // the recorded slots show what the install just changed. Best
            // effort: the install outcome above is recorded either way.
            let refreshed = rauc::query(rauc_client.as_ref()).await.ok();
            let mut inner = inner.write().await;
            let entry = rauc::update_entry(&mut inner.state);
            entry.insert("install".into(), outcome);
            if let Some(refreshed) = &refreshed {
                refreshed.merge_into(entry);
            }
            drop(inner);
            // Release the flag only after the outcome is recorded: a caller
            // admitted at this point sees `done`/`failed`, never a stale
            // `running` beside an idle flag.
            installing.store(false, Ordering::Release);
        });
        Ok(())
    }

    /// Query RAUC's operation, last error, progress, slot statuses and primary
    /// slot; record them under `update` in the live-state tree; return the
    /// recorded entry as JSON.
    ///
    /// The queries run WITHOUT the service lock — a wedged installer must not
    /// stall every other bus method — and the lock is taken only for the
    /// merge. On a query failure nothing is recorded (the last known entry
    /// stays) and the error goes to the caller, who is the one polling and can
    /// tell staleness from absence.
    pub async fn refresh_update_state(&self) -> fdo::Result<String> {
        let query = rauc::query(self.rauc.as_ref())
            .await
            .map_err(|err| fdo::Error::Failed(format!("query rauc: {err:#}")))?;
        // Re-derive the lifecycle entry from the same fresh slots, so the
        // answered `lifecycle` (recorded through the host before the merge
        // below) is exactly as new as the slot facts beside it.
        self.update
            .refresh(&query.slots, query.primary.as_deref())
            .await;
        let mut inner = self.inner.write().await;
        let entry = rauc::update_entry(&mut inner.state);
        query.merge_into(entry);
        let rendered = Value::Object(entry.clone()).to_string();
        drop(inner);
        Ok(rendered)
    }

    /// Manually mark a slot `good` or `bad`, on behalf of `sender`; answers
    /// RAUC's `(slot_name, message)`.
    ///
    /// The operator escape hatch over the boot health gate (`mos-health`),
    /// which owns the automatic confirm — see `crate::rauc`'s module docs for
    /// why mosd never marks anything on its own. `state` is validated down to
    /// `good`/`bad` and `slot` to `booted`/`other` BEFORE RAUC is asked;
    /// activation (`active`) is the installer's job and is not offered.
    pub async fn request_mark(
        &self,
        sender: &str,
        state: &str,
        slot: &str,
    ) -> fdo::Result<(String, String)> {
        rauc::validate_mark(state, slot).map_err(fdo::Error::InvalidArgs)?;
        tracing::warn!(state, slot, sender, "manual slot mark requested");
        let (slot_name, message) = self
            .rauc
            .mark(state, slot)
            .await
            .map_err(|err| fdo::Error::Failed(format!("rauc mark: {err:#}")))?;
        let mut inner = self.inner.write().await;
        rauc::update_entry(&mut inner.state).insert(
            "last_mark".into(),
            serde_json::json!({
                "state": state,
                "slot": slot,
                "slot_name": slot_name,
                "message": message,
                "requested_by": sender,
            }),
        );
        drop(inner);
        Ok((slot_name, message))
    }

    /// Validate and atomically persist `value` without waiting for a
    /// reconcile. The D-Bus method enqueues the apply after this returns.
    ///
    /// # Errors
    ///
    /// Whatever [`Settings::set`](mosd_settings::Settings::set) or
    /// [`Store::save`] rejected the write with.
    async fn persist_setting(&self, path: &str, value: Value) -> Result<(), SettingsError> {
        let mut inner = self.inner.write().await;
        let mut candidate = inner.settings.clone();
        candidate.set(path, value)?;
        self.store.save(&candidate)?;
        inner.settings = candidate;
        Ok(())
    }

    /// Synchronous test hook for assertions whose subject is lock behaviour,
    /// not the queue. Production settings writes call [`Self::persist_setting`]
    /// and [`Self::enqueue_apply`] from `SetSettings`.
    #[cfg(test)]
    pub async fn write_setting(&self, path: &str, value: Value) -> Result<(), SettingsError> {
        let _apply = self.apply_lock.lock().await;
        self.persist_setting(path, value).await?;
        self.apply_subtree(path).await;
        Ok(())
    }

    /// Apply exactly the reconcilers whose declared subtree overlaps `path`.
    ///
    /// The caller holds [`Self::apply_lock`]. Settings are cloned under a read
    /// lock and each live-state result is recorded under a short write lock;
    /// no data lock is held while a reconciler waits on another process.
    async fn apply_subtree(&self, path: &str) -> Vec<String> {
        reconcile_subtree(&self.reconcilers, &self.inner, path).await
    }

    /// Run every reconciler against the current settings, recording each
    /// result in the live-state tree. Errors are recorded, never propagated.
    pub async fn apply_all(&self) {
        let _apply = self.apply_lock.lock().await;
        self.apply_subtree("").await;
    }

    /// Queue one reconcile, publish its current record and ensure the one
    /// worker exists. The caller emits `SettingsChanged` separately because
    /// that signal describes persistence, while this lifecycle describes
    /// application.
    async fn enqueue_apply(
        &self,
        emitter: &SignalEmitter<'_>,
        operation: &str,
        dot_path: &str,
        source: &str,
    ) -> TaskRecord {
        self.apply_queue.remember_emitter(emitter);
        self.ensure_apply_worker();
        let enqueued = self.apply_queue.enqueue(operation, dot_path, source).await;
        publish_task_transition(&self.apply_queue, &self.inner, &enqueued.record).await;
        if enqueued.created {
            self.apply_queue.wake();
        }
        enqueued.record
    }

    fn ensure_apply_worker(&self) {
        if !self.apply_queue.claim_worker() {
            return;
        }
        tokio::spawn(run_apply_worker(
            Arc::clone(&self.apply_queue),
            Arc::clone(&self.inner),
            Arc::clone(&self.apply_lock),
            Arc::clone(&self.reconcilers),
        ));
    }

    /// Direct form retained only for unit tests whose subject is scoping or
    /// serialization. The production D-Bus member queues the apply and is
    /// exercised over a real bus by `mosd/tests/bus.rs`.
    #[cfg(test)]
    async fn set_transient_root_password(&self, password: &str) -> fdo::Result<()> {
        let _apply = self.apply_lock.lock().await;
        let shadow_path = self.shadow_path.clone();
        let password = password.to_string();
        tokio::task::spawn_blocking(move || {
            transient::set_transient_root_password(&shadow_path, &password)
        })
        .await
        .map_err(|err| fdo::Error::Failed(format!("transient password task: {err}")))?
        .map_err(transient_to_fdo)?;
        self.apply_subtree("access.ssh").await;
        Ok(())
    }
}

/// Apply `path` against one immutable settings snapshot, recording each
/// reconciler result under a short data write lock. Returns failure messages
/// for the task outcome; one failing reconciler does not stop the rest.
async fn reconcile_subtree(
    reconcilers: &[Box<dyn Reconciler>],
    inner: &RwLock<Inner>,
    path: &str,
) -> Vec<String> {
    let settings = inner.read().await.settings.clone();
    let mut failures = Vec::new();
    for reconciler in reconcilers {
        if paths_overlap(path, reconciler.subtree()) {
            let result = reconciler.apply(&settings).await;
            let mut inner = inner.write().await;
            if let Some(failure) = record(&mut inner.state, reconciler.name(), result) {
                failures.push(failure);
            }
        }
    }
    failures
}

async fn run_apply_worker(
    queue: Arc<ApplyQueue>,
    inner: Arc<RwLock<Inner>>,
    apply_lock: Arc<Mutex<()>>,
    reconcilers: Arc<Vec<Box<dyn Reconciler>>>,
) {
    loop {
        let ApplyJob { id, dot_path } = queue.next().await;
        let Some(started) = queue.start(&id).await else {
            tracing::error!(task_id = id, "queued apply has no task record");
            continue;
        };
        publish_task_transition(&queue, &inner, &started).await;

        let failures = {
            let _apply = apply_lock.lock().await;
            reconcile_subtree(&reconcilers, &inner, &dot_path).await
        };
        let (outcome, message) = if failures.is_empty() {
            ("succeeded", None)
        } else {
            ("failed", Some(failures.join("; ")))
        };
        if let Some(finished) = queue.finish(&id, outcome, message).await {
            publish_task_transition(&queue, &inner, &finished).await;
        }
    }
}

/// Keep the live-state `tasks` list and `TaskChanged` signal on the same
/// record. Signal failure is logged; subscribers then lapse and fall back to
/// `GetTask`, whose source of truth is the queue itself.
async fn publish_task_transition(queue: &ApplyQueue, inner: &RwLock<Inner>, record: &TaskRecord) {
    let snapshot = queue.snapshot().await;
    let tasks = serde_json::to_value(snapshot).unwrap_or_else(|err| {
        tracing::error!(error = %err, "serialize apply task history");
        Value::Array(Vec::new())
    });
    let mut data = inner.write().await;
    if let Some(root) = data.state.as_object_mut() {
        root.insert("tasks".to_string(), tasks);
    }
    drop(data);

    let Some(emitter) = queue.emitter() else {
        return;
    };
    let json = match serde_json::to_string(record) {
        Ok(json) => json,
        Err(err) => {
            tracing::error!(error = %err, task_id = record.id, "serialize task transition");
            return;
        }
    };
    if let Err(err) = MosdService::task_changed(&emitter, &json).await {
        tracing::warn!(error = %err, task_id = record.id, "emit TaskChanged failed");
    }
}

/// Store a reconciler `result` in the live-state tree under `name`; a failure
/// is logged and recorded as `{"error": "..."}`.
fn record(state: &mut Value, name: &str, result: anyhow::Result<Value>) -> Option<String> {
    let (entry, failure) = match result {
        Ok(value) => (value, None),
        Err(err) => {
            tracing::error!(reconciler = name, error = %err, "reconciler apply failed");
            let message = format!("{name}: {err}");
            (
                serde_json::json!({ "error": err.to_string() }),
                Some(message),
            )
        }
    };
    if let Some(map) = state.as_object_mut() {
        map.insert(name.to_string(), entry);
    }
    failure
}

/// Unique bus name of the caller, or `"(unknown)"` on an unnamed message.
///
/// Used by management methods so audit state names the exact D-Bus caller.
pub(crate) fn sender_of<'a>(header: &'a Header<'a>) -> &'a str {
    header.sender().map_or("(unknown)", |name| name.as_str())
}

/// D-Bus error name for a settings dot-path that does not resolve.
pub const NOT_FOUND_ERROR: &str = "com.mos.mosd1.Error.NotFound";
/// D-Bus error name for a settings dot-path that exists but rejects writes.
pub const READ_ONLY_ERROR: &str = "com.mos.mosd1.Error.ReadOnly";

/// Reply error of the settings methods.
///
/// `NotFound` and `ReadOnly` carry interface-scoped error names, because the
/// standard fdo vocabulary has no name that separates "the dot-path does not
/// exist" and "the dot-path rejects writes" from "the value is bad" — mapping
/// all three onto `InvalidArgs` destroyed the distinction at the bus boundary
/// and left apid answering one HTTP status for three conditions. Every other
/// failure keeps the standard fdo name it always had, delegated to
/// [`fdo::Error`] so its replies stay byte-identical.
#[derive(Debug)]
enum SettingsFault {
    /// [`NOT_FOUND_ERROR`], from [`SettingsError::NotFound`].
    NotFound(String),
    /// [`READ_ONLY_ERROR`], from [`SettingsError::ReadOnly`].
    ReadOnly(String),
    /// Everything else, under its standard fdo name.
    Fdo(fdo::Error),
}

impl zbus::DBusError for SettingsFault {
    fn name(&self) -> zbus::names::ErrorName<'_> {
        match self {
            Self::NotFound(_) => zbus::names::ErrorName::from_static_str_unchecked(NOT_FOUND_ERROR),
            Self::ReadOnly(_) => zbus::names::ErrorName::from_static_str_unchecked(READ_ONLY_ERROR),
            Self::Fdo(err) => err.name(),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            Self::NotFound(message) | Self::ReadOnly(message) => Some(message),
            Self::Fdo(err) => err.description(),
        }
    }

    fn create_reply(&self, call: &Header<'_>) -> zbus::Result<zbus::message::Message> {
        match self {
            Self::Fdo(err) => err.create_reply(call),
            // The reply body is the description string, the same single-`s`
            // shape every fdo error reply carries.
            _ => zbus::message::Message::error(call, self.name())?
                .build(&self.description().unwrap_or_default()),
        }
    }
}

/// Map settings errors onto D-Bus error names.
fn to_bus_error(err: SettingsError) -> SettingsFault {
    match err {
        SettingsError::NotFound(_) => SettingsFault::NotFound(err.to_string()),
        SettingsError::ReadOnly(_) => SettingsFault::ReadOnly(err.to_string()),
        SettingsError::Validation { .. } => {
            SettingsFault::Fdo(fdo::Error::InvalidArgs(err.to_string()))
        }
        SettingsError::Io(_) => SettingsFault::Fdo(fdo::Error::IOError(err.to_string())),
        SettingsError::Parse(_) | SettingsError::Migration(_) => {
            SettingsFault::Fdo(fdo::Error::Failed(err.to_string()))
        }
    }
}

/// Whole seconds since boot, from the first field of `/proc/uptime`.
///
/// `None` on an unreadable or malformed file — a soft failure rather than a
/// panic, because `GetState` must keep answering for every other fact when
/// this one is missing.
fn read_uptime_seconds() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = contents.split_whitespace().next()?.parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then_some(secs as u64)
}

/// Map an update-action refusal onto a D-Bus error: policy refusals carry
/// `AccessDenied` (apid maps it to 409 — the request was well-formed and the
/// device said no), malformed requests carry `InvalidArgs` (422), and
/// busy/unavailable stay `Failed`.
fn refusal_to_fdo(refusal: Refusal) -> fdo::Error {
    match refusal {
        Refusal::Policy(message) => fdo::Error::AccessDenied(message),
        Refusal::Invalid(message) => fdo::Error::InvalidArgs(message),
        Refusal::Busy(message) | Refusal::Unavailable(message) => fdo::Error::Failed(message),
    }
}

/// Map a transient-password failure onto a D-Bus error.
///
/// Always `Failed`: the caller cannot distinguish a rejected password from an
/// unwritable shadow file, and neither is worth leaking more detail over. The
/// message is the anyhow chain, which by construction carries lengths and rule
/// names but never the password itself — `transient::validate` never echoes its
/// input.
fn transient_to_fdo(err: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(format!("set transient root password: {err:#}"))
}

#[zbus::interface(name = "com.mos.mosd1")]
impl MosdService {
    /// JSON-encoded settings value at dot-path `path` (`""` = whole tree).
    async fn get_settings(&self, path: &str) -> Result<String, SettingsFault> {
        let inner = self.inner.read().await;
        let value = inner.settings.get(path).map_err(to_bus_error)?;
        Ok(value.to_string())
    }

    /// Parse `value_json`, persist it atomically, enqueue the overlapping
    /// reconcile, and return its task id.
    async fn set_settings(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(header)] header: Header<'_>,
        path: &str,
        value_json: &str,
    ) -> Result<String, SettingsFault> {
        let value: Value = serde_json::from_str(value_json).map_err(|err| {
            SettingsFault::Fdo(fdo::Error::InvalidArgs(format!(
                "invalid JSON value: {err}"
            )))
        })?;
        self.persist_setting(path, value)
            .await
            .map_err(to_bus_error)?;
        Self::settings_changed(&emitter, path, value_json)
            .await
            .map_err(|err| {
                SettingsFault::Fdo(fdo::Error::Failed(format!("emit SettingsChanged: {err}")))
            })?;
        let task = self
            .enqueue_apply(&emitter, "settings-write", path, sender_of(&header))
            .await;
        Ok(task.id)
    }

    /// JSON-encoded task record for `id`.
    async fn get_task(&self, id: &str) -> Result<String, SettingsFault> {
        let task = self
            .apply_queue
            .get(id)
            .await
            .ok_or_else(|| SettingsFault::NotFound(format!("task not found: `{id}`")))?;
        serde_json::to_string(&task).map_err(|err| {
            SettingsFault::Fdo(fdo::Error::Failed(format!("serialize task {id}: {err}")))
        })
    }

    /// JSON-encoded live-state subtree at dot-path `path` (`""` = whole tree).
    ///
    /// `uptime` — whole seconds since boot, a bare JSON number at the top
    /// level — is computed here, at read time, and grafted onto the served
    /// view. Reading it per call is what keeps a cached seconds-counter from
    /// ever being served stale; grafting rather than storing keeps the stored
    /// tree reserved for pushed facts, so a read never manufactures stored
    /// state.
    ///
    /// Two failure paths, under two names: [`NOT_FOUND_ERROR`] when the
    /// dot-path resolves to nothing, and fdo `Failed` when the `/proc/uptime`
    /// read does not answer. apid maps the first to 404 and the second to 500
    /// with no route-specific reading of either.
    async fn get_state(&self, path: &str) -> Result<String, SettingsFault> {
        if path == "uptime" {
            let secs = read_uptime_seconds().ok_or_else(|| {
                SettingsFault::Fdo(fdo::Error::Failed("read /proc/uptime".to_string()))
            })?;
            return Ok(Value::from(secs).to_string());
        }
        let inner = self.inner.read().await;
        if path.is_empty() {
            let mut root = inner.state.clone();
            if let (Some(secs), Some(map)) = (read_uptime_seconds(), root.as_object_mut()) {
                map.insert("uptime".to_string(), Value::from(secs));
            }
            return Ok(root.to_string());
        }
        // The dot-path names nothing in the tree. That is [`NOT_FOUND_ERROR`],
        // the same name `rotate_wireguard_key` raises for an interface the
        // settings do not declare, and not `InvalidArgs`: the argument is
        // well-formed, there is simply no value under it. Under one shared
        // name apid could only tell the two apart by knowing that on the state
        // route the name had a single producer, and it read the name against
        // that private fact to answer 404. Naming the condition here is what lets that reading go.
        let value = json_path_get(&inner.state, path)
            .ok_or_else(|| SettingsFault::NotFound(format!("state path not found: `{path}`")))?;
        Ok(value.to_string())
    }

    /// JSON time-synchronization status, observed from timesyncd at call
    /// time and classified by [`crate::time_status::classify`].
    ///
    /// Observed rather than stored, the `uptime` reasoning: synchronization
    /// moves without any settings write, so a cached copy would only ever be
    /// stale. Read-only — there is deliberately no method beside it that
    /// could pause or stop synchronization.
    async fn get_time_status(&self) -> Result<String, SettingsFault> {
        let evidence = self.time_status.observe().await.map_err(|err| {
            SettingsFault::Fdo(fdo::Error::Failed(format!(
                "observe time synchronization: {err:#}"
            )))
        })?;
        Ok(status_json(&evidence).to_string())
    }

    /// JSON storage status: the fixed tiers, the physical media and the
    /// low-space policy, observed at call time.
    ///
    /// Observed rather than stored, for the `uptime` and `GetTimeStatus`
    /// reason: space and wear move without any settings write, so a cached
    /// copy would only ever be stale. Read-only, and deliberately with
    /// nothing beside it: there is no bus method here that formats,
    /// repartitions, resizes or erases anything, and the layout is fixed by
    /// the image assembler.
    async fn get_storage_status(&self) -> Result<String, SettingsFault> {
        let evidence = self.storage_status.observe().await.map_err(|err| {
            SettingsFault::Fdo(fdo::Error::Failed(format!("observe storage: {err:#}")))
        })?;
        Ok(storage_status::status_json(&evidence, &self.storage_pressure).to_string())
    }

    /// JSON snapshot returned by systemd-networkd's live `Describe` method.
    async fn get_network_state(&self) -> Result<String, SettingsFault> {
        self.network_state
            .describe()
            .await
            .map(|value| value.to_string())
            .map_err(|err| {
                SettingsFault::Fdo(fdo::Error::Failed(format!(
                    "observe network state: {err:#}"
                )))
            })
    }

    /// JSON observed network state (PLAN-052): link/carrier, addresses, DHCP
    /// lease, default routes, DNS reachability, Wi-Fi association and the
    /// radio/modem capabilities, observed at call time and DISTINCT from the
    /// desired `network` settings, which this method never reads.
    async fn get_observed_network(&self) -> Result<String, SettingsFault> {
        self.network_state
            .observe()
            .await
            .map(|value| value.to_string())
            .map_err(|err| {
                SettingsFault::Fdo(fdo::Error::Failed(format!("observe network: {err:#}")))
            })
    }

    /// JSON system information (PLAN-052): machine id, board, kernel,
    /// release, the image version with its git stamp and build date, the
    /// installed packages, the booted slot and the uptime — every one read
    /// at call time from the seam that already carries it, none restated.
    ///
    /// Observed rather than stored, the `uptime` reasoning again: the slot
    /// and the uptime move without any settings write. Read-only.
    async fn get_system_info(&self) -> Result<String, SettingsFault> {
        let evidence = self.system_info.observe().await.map_err(|err| {
            SettingsFault::Fdo(fdo::Error::Failed(format!(
                "observe system information: {err:#}"
            )))
        })?;
        let slot = self.slot_evidence().await;
        Ok(system_info::info_json(
            &evidence,
            slot.as_ref(),
            &system_info::DaemonIdentity::this_build(),
        )
        .to_string())
    }

    /// JSON board telemetry (PLAN-052): temperature, watchdog and the reset
    /// reason the kernel's generic sources support, observed at call time.
    /// Absence is explicit; nothing here reads a vendor register.
    async fn get_telemetry(&self) -> Result<String, SettingsFault> {
        let evidence = self.telemetry.observe().await.map_err(|err| {
            SettingsFault::Fdo(fdo::Error::Failed(format!("observe telemetry: {err:#}")))
        })?;
        Ok(telemetry::telemetry_json(&evidence).to_string())
    }

    /// JSON failure evidence for the diagnostic snapshot (PLAN-052): the
    /// units systemd holds failed and a bounded excerpt of this boot's
    /// journal at warning and worse. Bounded in size and time by
    /// [`crate::diagnostics`]; the redaction is apid's, at the snapshot.
    async fn get_failure_evidence(&self) -> Result<String, SettingsFault> {
        self.failure_evidence
            .observe()
            .await
            .map(|value| value.to_string())
            .map_err(|err| {
                SettingsFault::Fdo(fdo::Error::Failed(format!(
                    "observe failure evidence: {err:#}"
                )))
            })
    }

    /// Record a component health report in the live-state tree under
    /// `health.<component>` as `{"status": ..., "detail": ...}`.
    ///
    /// Used by the boot health gate (`mos-health`) to surface non-fatal
    /// pressure — a full `/var`, for example — without failing the gate.
    async fn report_health(&self, component: &str, status: &str, detail: &str) -> fdo::Result<()> {
        // The live-state tree lives in RAM for the life of the daemon, and
        // this is its only write surface that accepts arbitrary keys and
        // strings with no pruning. Callers are root-only, so the caps guard
        // against a wedged or looping reporter, not an attacker — but a root
        // daemon that can be grown without bound by a misbehaving oneshot is
        // still a daemon that eventually takes the device down with it.
        const MAX_COMPONENT_LEN: usize = 64;
        const MAX_STATUS_LEN: usize = 64;
        const MAX_DETAIL_LEN: usize = 1024;
        const MAX_COMPONENTS: usize = 128;
        if component.is_empty() {
            return Err(fdo::Error::InvalidArgs(
                "component must not be empty".into(),
            ));
        }
        if component.len() > MAX_COMPONENT_LEN
            || status.len() > MAX_STATUS_LEN
            || detail.len() > MAX_DETAIL_LEN
        {
            return Err(fdo::Error::InvalidArgs(format!(
                "health report too large: component <= {MAX_COMPONENT_LEN}, \
                 status <= {MAX_STATUS_LEN}, detail <= {MAX_DETAIL_LEN} bytes"
            )));
        }
        let mut inner = self.inner.write().await;
        if let Some(root) = inner.state.as_object_mut() {
            let health = root
                .entry("health")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(health) = health.as_object_mut() {
                if !health.contains_key(component) && health.len() >= MAX_COMPONENTS {
                    return Err(fdo::Error::InvalidArgs(format!(
                        "health table already holds {MAX_COMPONENTS} components; \
                         refusing a new one"
                    )));
                }
                health.insert(
                    component.to_string(),
                    serde_json::json!({ "status": status, "detail": detail }),
                );
            }
        }
        drop(inner);
        tracing::info!(component, status, detail, "health report recorded");
        Ok(())
    }

    /// Drop a DISCONNECTED service from the registry (`crate::scan`).
    ///
    /// Exported as `ForgetService`. Refuses a service that is still connected,
    /// and a name the registry does not carry, with
    /// [`InvalidArgs`](fdo::Error::InvalidArgs).
    ///
    /// # Why retention plus an explicit removal, rather than auto-eviction
    ///
    /// A service that vanishes is kept with `connected: false` instead of
    /// being deleted, because the two states an operator most needs to tell
    /// apart look identical once an entry is gone: a service that was never
    /// installed, and a service that was installed and has stopped appearing.
    /// Auto-eviction turns the second into the first — the registry would look
    /// tidy and correct while quietly withholding the one fact that explains
    /// why a device stopped reporting. So the entry stays, saying exactly what
    /// is true (this service is known and is not here), and it leaves only
    /// when someone who knows it is not coming back says so.
    ///
    /// Refusing to forget a CONNECTED service is the other half of that: the
    /// scan would re-add it on its next event, so accepting the call would be
    /// a removal that silently undoes itself, which is worse than a refusal
    /// that says what happened.
    async fn forget_service(&self, bus_name: &str) -> fdo::Result<()> {
        let registry = self.registry.as_ref().ok_or_else(|| {
            fdo::Error::Failed("no service registry: this daemon runs no scan".to_string())
        })?;
        let snapshot = registry.forget(bus_name)?;
        self.publish_services(snapshot).await;
        tracing::info!(service = bus_name, "service forgotten by request");
        Ok(())
    }

    /// Reboot the appliance through systemd.
    ///
    /// The request is logged and recorded in the live-state tree before the
    /// call is made.
    async fn reboot(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.request_reboot(sender_of(&header)).await
    }

    /// Power the appliance off through systemd.
    ///
    /// The request is logged and recorded in the live-state tree before the
    /// call is made.
    async fn power_off(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.request_power_off(sender_of(&header)).await
    }

    /// Install the update bundle at absolute path `bundle_path` through RAUC.
    ///
    /// Exported as `InstallUpdate`. Answers as soon as the install has been
    /// validated, recorded and handed to a background task; progress and the
    /// outcome are read back through `GetUpdateState` (or the `update` subtree
    /// of `GetState`). Refuses a relative path, a path that does not name an
    /// existing regular file, a path outside `/mos/updates/verified` or a
    /// `.part` (`InvalidArgs`: only a verified bundle is handed to RAUC), and
    /// a second install while one runs.
    async fn install_update(
        &self,
        #[zbus(header)] header: Header<'_>,
        bundle_path: &str,
    ) -> fdo::Result<()> {
        self.request_install(sender_of(&header), bundle_path).await
    }

    /// Query RAUC and answer the JSON-encoded `update` live-state entry:
    /// operation, last error, progress, per-slot status, booted slot, primary
    /// slot and the pending-not-confirmed flag, plus whatever `install` /
    /// `last_mark` entries earlier calls recorded.
    ///
    /// Exported as `GetUpdateState`. Unlike `GetState("update")`, which
    /// answers from the tree as last recorded, this asks RAUC first — the
    /// polling surface for a UI watching an install.
    async fn get_update_state(&self) -> fdo::Result<String> {
        self.refresh_update_state().await
    }

    /// Manually mark a slot: `state` is `good` or `bad`, `slot` is `booted`
    /// or `other`. Answers RAUC's `(slot_name, message)`.
    ///
    /// Exported as `MarkUpdate`. The manual escape hatch for the case the
    /// boot health gate cannot decide (its automatic mark-good is the gate's
    /// job, not mosd's); see `crate::rauc` for the split.
    async fn mark_update(
        &self,
        #[zbus(header)] header: Header<'_>,
        state: &str,
        slot: &str,
    ) -> fdo::Result<(String, String)> {
        self.request_mark(sender_of(&header), state, slot).await
    }

    /// Run an update metadata check (`rauc-update sync` + `check`) on a
    /// background task.
    ///
    /// Exported as `CheckUpdate`. Answers as soon as the check is admitted;
    /// the outcome lands under live-state `update.lifecycle` and is read
    /// back through `GetUpdateState`. Refused with `AccessDenied` when the
    /// update policy forbids it (offline mode, no configured source, an
    /// invalid policy file) and with `Failed` when the client binary is
    /// absent or another update operation is running.
    async fn check_update(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.update
            .request_check(sender_of(&header))
            .await
            .map_err(refusal_to_fdo)
    }

    /// Download the selected bundle (`rauc-update fetch`) on a background
    /// task; on success the verified bundle path is recorded and the
    /// lifecycle state becomes `ready`.
    ///
    /// Exported as `FetchUpdate`. The same admission and refusal shape as
    /// `CheckUpdate`, plus the metered-mode download refusal.
    async fn fetch_update(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.update
            .request_fetch(sender_of(&header))
            .await
            .map_err(refusal_to_fdo)
    }

    /// Arm the bounded administrative override of the safe-to-reboot gate
    /// for `seconds`; answers the recorded override as JSON.
    ///
    /// Exported as `SetRebootOverride`. The override lifts health-report
    /// blocks only — never an install in flight — and expires on its own;
    /// there is deliberately no member that disarms the gate permanently.
    async fn set_reboot_override(
        &self,
        #[zbus(header)] header: Header<'_>,
        seconds: u32,
    ) -> fdo::Result<String> {
        let record = self
            .update
            .set_reboot_override(sender_of(&header), u64::from(seconds))
            .await
            .map_err(refusal_to_fdo)?;
        Ok(record.to_string())
    }

    /// Set a TRANSIENT root password, then re-apply the SSH subtree.
    ///
    /// Exported as `SetTransientRootPassword`. The password lives until the
    /// next boot, when `mos-shadow-reconcile` clears the root hash it wrote;
    /// persistent access is by SSH public key.
    ///
    /// Deliberately not a setting. Nothing is written into the settings tree
    /// and no [`SettingsChanged`](Self::settings_changed) is emitted, because a
    /// password that reached the settings tree would be persisted, re-applied
    /// on the next boot and readable by anything that can call `GetSettings` —
    /// which is the opposite of transient in all three respects.
    ///
    /// The reconcilers are re-run afterwards so the sshd drop-in re-renders
    /// against a device that now has a password to offer, and sshd picks it up.
    #[zbus(name = "SetTransientRootPassword")]
    async fn enqueue_transient_root_password(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(header)] header: Header<'_>,
        password: &str,
    ) -> fdo::Result<String> {
        // Two concerns share this shape. Serialization: zbus dispatches `&self`
        // methods concurrently, and two unserialized writers would interleave
        // read-modify-write cycles on one shadow file through one fixed temp
        // name — so the write happens under the same lock every other mutating
        // method takes. Blocking: the bcrypt hash inside costs hundreds of
        // milliseconds of CPU, which must not stall the bus dispatcher, so the
        // whole write runs off the async scheduler while the guard is held.
        let _apply = self.apply_lock.lock().await;
        let shadow_path = self.shadow_path.clone();
        let password = password.to_string();
        tokio::task::spawn_blocking(move || {
            transient::set_transient_root_password(&shadow_path, &password)
        })
        .await
        .map_err(|err| fdo::Error::Failed(format!("transient password task: {err}")))?
        .map_err(transient_to_fdo)?;
        let task = self
            .enqueue_apply(
                &emitter,
                "transient-password",
                "access.ssh",
                sender_of(&header),
            )
            .await;
        Ok(task.id)
    }

    /// Draw a new private key for the WireGuard interface `iface` and return
    /// its new public key.
    ///
    /// Deliberately not a setting, for the reason a transient root password is
    /// not one: a key that reached the settings tree would be persisted and
    /// served back out of it. It is not a setting in the other direction
    /// either — the tree holds no key to change — so nothing is written there,
    /// and no [`SettingsChanged`](Self::settings_changed) is emitted.
    ///
    /// The reconcilers are re-run afterwards so the tunnel's unit is
    /// re-rendered and networkd builds the device back around the key now on
    /// disk; the new public key reaches the live-state tree on that pass.
    async fn rotate_wireguard_key(&self, iface: &str) -> Result<String, SettingsFault> {
        // Under the same lock every mutating method takes: two unserialized
        // rotations of one interface would each write a key and each delete
        // the device, and the public key one of them returned would be the
        // half of a private key the other had already replaced.
        let _apply = self.apply_lock.lock().await;
        let settings = self.inner.read().await.settings.clone();
        match settings.network.get(iface) {
            Some(cfg) if cfg.kind == mosd_settings::IfaceKind::Wireguard => {}
            // The entry exists and its kind is wrong: a bad argument, and the
            // 422 apid already answers for one.
            Some(_) => {
                return Err(SettingsFault::Fdo(fdo::Error::InvalidArgs(format!(
                    "network.{iface} is not a WireGuard interface"
                ))));
            }
            // The entry does not exist: the path names nothing, which is the
            // condition every other read on this bus already raises
            // [`NOT_FOUND_ERROR`] for and which apid already maps to 404.
            // These two travelled under one error name until later, and
            // that is why the shipped rotate-key route answered 422 where the
            // settings and state reads beside it answered 404 for the same
            // class of condition. The
            // split is here rather than in apid because apid had nothing left
            // to tell them apart with.
            None => {
                return Err(SettingsFault::NotFound(format!(
                    "network.{iface} is not a declared network entry"
                )));
            }
        }
        let public_key = self
            .wireguard
            .rotate_key(iface)
            .await
            // The anyhow chain names paths and never key material: the key
            // store's errors are written that way, and this adds no value of
            // its own to them.
            .map_err(|err| {
                SettingsFault::Fdo(fdo::Error::Failed(format!("rotate wireguard key: {err:#}")))
            })?;
        self.apply_subtree("network").await;
        Ok(public_key)
    }

    /// Emitted after a successful `SetSettings` with the changed dot-path and
    /// its new JSON-encoded value.
    #[zbus(signal)]
    async fn settings_changed(
        emitter: &SignalEmitter<'_>,
        path: &str,
        value_json: &str,
    ) -> zbus::Result<()>;

    /// Emitted whenever an apply task is queued, starts or finishes. The body
    /// is one JSON-encoded [`TaskRecord`].
    #[zbus(signal)]
    async fn task_changed(emitter: &SignalEmitter<'_>, task_json: &str) -> zbus::Result<()>;
}

/// The automatic driver's window onto the daemon.
///
/// Assembled here because its two halves live in different places: the check
/// and the fetch are the lifecycle's, which the manual `CheckUpdate` and
/// `FetchUpdate` routes also call, and the install and the reboot are this
/// service's, which the object server owns once the connection is built —
/// reached the way [`crate::scan`] reaches it, through the served interface.
///
/// What is deliberately NOT here is `SetRebootOverride`. The automatic path
/// holds this object and nothing else, so it cannot arm the reboot-gate
/// override; see [`AutoRoutes`].
pub struct BusRoutes {
    lifecycle: Arc<UpdateLifecycle>,
    service: InterfaceRef<MosdService>,
}

impl BusRoutes {
    pub fn new(lifecycle: Arc<UpdateLifecycle>, service: InterfaceRef<MosdService>) -> Self {
        Self { lifecycle, service }
    }

    /// `GetUpdateState`'s own document, read exactly as an operator polling
    /// the API reads it.
    async fn update_state(&self) -> Option<Value> {
        let rendered = self
            .service
            .get()
            .await
            .refresh_update_state()
            .await
            .map_err(|err| {
                tracing::debug!(error = %err, "automatic driver could not read the update state");
            })
            .ok()?;
        serde_json::from_str(&rendered).ok()
    }
}

#[async_trait::async_trait]
impl AutoRoutes for BusRoutes {
    fn policy(&self) -> LoadedPolicy {
        self.lifecycle.policy().load()
    }

    async fn check(&self, sender: &str) -> Result<Settled<Available>, Refusal> {
        self.lifecycle.check_now(sender).await
    }

    async fn fetch(&self, sender: &str) -> Result<Settled<String>, Refusal> {
        self.lifecycle.fetch_now(sender).await
    }

    async fn available(&self) -> Option<Available> {
        self.lifecycle.available().await
    }

    async fn staged(&self) -> Option<String> {
        self.lifecycle.staged_bundle().await
    }

    async fn discard_staged(&self, why: &str) {
        self.lifecycle.discard_bundle(why).await;
    }

    async fn facts(&self) -> Option<UpdateFacts> {
        let entry = self.update_state().await?;
        Some(UpdateFacts {
            reboot_pending: entry
                .get("pending_not_confirmed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            install_status: entry
                .pointer("/install/status")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    async fn install(&self, sender: &str, bundle: &str) -> Result<(), String> {
        self.service
            .get()
            .await
            .request_install(sender, bundle)
            .await
            .map_err(|err| err.to_string())
    }

    async fn reboot(&self, sender: &str) -> Result<(), String> {
        self.service
            .get()
            .await
            .request_reboot(sender)
            .await
            .map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::{Inner, MosdService, paths_overlap, run_apply_worker};
    use crate::power::MockPower;
    use crate::rauc::{MockRauc, SlotStatus};

    struct RecordingReconciler {
        name: &'static str,
        subtree: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
    }

    struct BlockingReconciler {
        name: &'static str,
        subtree: &'static str,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::reconciler::Reconciler for BlockingReconciler {
        fn name(&self) -> &'static str {
            self.name
        }

        fn subtree(&self) -> &'static str {
            self.subtree
        }

        async fn apply(
            &self,
            _settings: &mosd_settings::Settings,
        ) -> anyhow::Result<serde_json::Value> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(serde_json::json!({"applied": true}))
        }
    }

    #[async_trait::async_trait]
    impl crate::reconciler::Reconciler for RecordingReconciler {
        fn name(&self) -> &'static str {
            self.name
        }

        fn subtree(&self) -> &'static str {
            self.subtree
        }

        async fn apply(
            &self,
            _settings: &mosd_settings::Settings,
        ) -> anyhow::Result<serde_json::Value> {
            self.calls
                .lock()
                .expect("recording reconciler call log")
                .push(self.name.to_string());
            Ok(serde_json::json!({"applied": true}))
        }
    }

    /// Three accounts, nine fields each — the shape of a Debian `/etc/shadow`.
    const SHADOW: &str = "root:!:19000:0:99999:7:::\n\
        daemon:*:19000:0:99999:7:::\n";

    /// A mock's shared call log ([`MockPower::calls`] / [`MockRauc::calls`]).
    type CallLog = Arc<Mutex<Vec<String>>>;

    /// Service backed by a throwaway settings file, a throwaway shadow file,
    /// a recording power mock and the given RAUC mock; the power log and the
    /// RAUC call log are returned alongside.
    fn service_with_rauc(rauc: MockRauc) -> (MosdService, CallLog, CallLog, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = mosd_settings::Store::new(dir.path().join("settings.toml"));
        let shadow_path = dir.path().join("shadow");
        std::fs::write(&shadow_path, SHADOW).expect("seed shadow");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let rauc_calls = Arc::clone(&rauc.calls);
        let service = MosdService::new(
            store,
            mosd_settings::Settings::default(),
            Vec::new(),
            Box::new(MockPower {
                calls: Arc::clone(&calls),
            }),
            shadow_path,
            serde_json::json!({}),
        )
        .with_rauc(Arc::new(rauc))
        // Installs are admitted only from <workspace>/verified; the tests'
        // bundles are placed there by `verified_bundle`.
        .with_update_workspace(dir.path().join("updates"));
        (service, calls, rauc_calls, dir)
    }

    /// A bundle file inside the test service's `verified/`, as a string path.
    fn verified_bundle(dir: &tempfile::TempDir, name: &str) -> String {
        let verified = dir.path().join("updates").join("verified");
        std::fs::create_dir_all(&verified).expect("verified/");
        let bundle = verified.join(name);
        std::fs::write(&bundle, b"bundle bytes").expect("seed bundle");
        bundle.to_str().expect("utf-8").to_string()
    }

    /// [`service_with_rauc`] over a default (idle, slotless) RAUC mock.
    fn service_with_mock() -> (MosdService, Arc<Mutex<Vec<String>>>, tempfile::TempDir) {
        let (service, calls, _rauc_calls, dir) = service_with_rauc(MockRauc::default());
        (service, calls, dir)
    }

    /// A service with one reconciler for each subtree that makes an accidental
    /// `apply_all` visible in the call log.
    fn service_with_recording_reconcilers() -> (MosdService, CallLog, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow_path = dir.path().join("shadow");
        std::fs::write(&shadow_path, SHADOW).expect("seed shadow");
        let mut settings = mosd_settings::Settings::default();
        settings.network.insert(
            "wg0".to_string(),
            mosd_settings::IfaceSettings {
                kind: mosd_settings::IfaceKind::Wireguard,
                wireguard: Some(mosd_settings::WireguardConfig::default()),
                ..mosd_settings::IfaceSettings::default()
            },
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reconciler = |name, subtree| {
            Box::new(RecordingReconciler {
                name,
                subtree,
                calls: Arc::clone(&calls),
            }) as Box<dyn crate::reconciler::Reconciler>
        };
        let service = MosdService::new(
            mosd_settings::Store::new(dir.path().join("settings.toml")),
            settings,
            vec![
                reconciler("sshd", "access.ssh"),
                reconciler("network", "network"),
                reconciler("container", "container"),
            ],
            Box::new(MockPower {
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
            shadow_path,
            serde_json::json!({}),
        )
        .with_wireguard(Arc::new(crate::reconciler::network::KeyRotation::new(
            crate::wgkeys::Keystore::under(dir.path(), None),
            crate::reconciler::network::NoDelete,
        )));
        (service, calls, dir)
    }

    #[tokio::test]
    async fn a_transient_password_reapplies_only_the_ssh_subtree() {
        let (service, calls, _dir) = service_with_recording_reconcilers();

        service
            .set_transient_root_password("correct horse battery")
            .await
            .expect("set transient password");

        assert_eq!(
            *calls.lock().expect("call log"),
            vec!["sshd".to_string()],
            "password activation must not reload networkd or container generators"
        );
    }

    #[tokio::test]
    async fn a_wireguard_rotation_reapplies_only_the_network_subtree() {
        let (service, calls, _dir) = service_with_recording_reconcilers();

        service
            .rotate_wireguard_key("wg0")
            .await
            .expect("rotate wireguard key");

        assert_eq!(
            *calls.lock().expect("call log"),
            vec!["network".to_string()],
            "key rotation must not touch sshd or container generators"
        );
    }

    #[tokio::test]
    async fn two_identical_queued_writes_run_one_reconcile() {
        let queue = Arc::new(crate::apply_queue::ApplyQueue::new());
        let first = queue.enqueue("settings-write", "hostname", ":1.7").await;
        let second = queue.enqueue("settings-write", "hostname", ":1.8").await;
        assert_eq!(first.record.id, second.record.id);
        assert_eq!(second.record.folded_count, 1);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let reconcilers: Arc<Vec<Box<dyn crate::reconciler::Reconciler>>> =
            Arc::new(vec![Box::new(RecordingReconciler {
                name: "hostname",
                subtree: "hostname",
                calls: Arc::clone(&calls),
            })]);
        let inner = Arc::new(tokio::sync::RwLock::new(Inner {
            settings: mosd_settings::Settings::default(),
            state: serde_json::json!({}),
        }));
        let worker = tokio::spawn(run_apply_worker(
            Arc::clone(&queue),
            Arc::clone(&inner),
            Arc::new(tokio::sync::Mutex::new(())),
            reconcilers,
        ));
        queue.wake();

        let finished = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let task = queue.get(&first.record.id).await.expect("task record");
                if task.terminal() {
                    break task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task finishes");
        worker.abort();

        assert_eq!(finished.outcome.as_deref(), Some("succeeded"));
        assert_eq!(
            *calls.lock().expect("call log"),
            vec!["hostname".to_string()]
        );
    }

    #[tokio::test]
    async fn settings_reads_answer_while_a_reconcile_is_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let service = Arc::new(MosdService::new(
            mosd_settings::Store::new(dir.path().join("settings.toml")),
            mosd_settings::Settings::default(),
            vec![Box::new(BlockingReconciler {
                name: "hostname",
                subtree: "hostname",
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            })],
            Box::new(MockPower {
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
            dir.path().join("shadow"),
            serde_json::json!({}),
        ));

        let writer = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                service
                    .write_setting("hostname", serde_json::json!("bounded-read"))
                    .await
            })
        };
        started.notified().await;
        let read =
            tokio::time::timeout(Duration::from_millis(100), service.get_settings("hostname"))
                .await;
        release.notify_one();
        writer.await.expect("writer task").expect("settings write");

        assert_eq!(
            read.expect("a settings read must not wait for reconcile")
                .expect("settings read"),
            "\"bounded-read\""
        );
    }

    #[tokio::test]
    async fn concurrent_transient_password_writes_stay_serialized_by_the_apply_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow_path = dir.path().join("shadow");
        std::fs::write(&shadow_path, SHADOW).expect("seed shadow");
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let service = Arc::new(MosdService::new(
            mosd_settings::Store::new(dir.path().join("settings.toml")),
            mosd_settings::Settings::default(),
            vec![Box::new(BlockingReconciler {
                name: "sshd",
                subtree: "access.ssh",
                started: Arc::clone(&started),
                release: Arc::clone(&release),
            })],
            Box::new(MockPower {
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
            shadow_path.clone(),
            serde_json::json!({}),
        ));

        let first_password = "first correct horse";
        let second_password = "second battery staple";
        let first = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.set_transient_root_password(first_password).await })
        };
        started.notified().await;

        let second = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.set_transient_root_password(second_password).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let root_hash = std::fs::read_to_string(&shadow_path)
            .expect("shadow after first write")
            .lines()
            .next()
            .and_then(|line| line.split(':').nth(1))
            .expect("root hash")
            .to_string();
        assert!(bcrypt::verify(first_password, &root_hash).expect("verify first hash"));
        assert!(
            !bcrypt::verify(second_password, &root_hash).expect("reject second hash"),
            "the second writer reached the shadow file before the first apply finished"
        );

        release.notify_one();
        first.await.expect("first task").expect("first write");
        started.notified().await;
        release.notify_one();
        second.await.expect("second task").expect("second write");

        let root_hash = std::fs::read_to_string(&shadow_path)
            .expect("shadow after second write")
            .lines()
            .next()
            .and_then(|line| line.split(':').nth(1))
            .expect("root hash")
            .to_string();
        assert!(bcrypt::verify(second_password, &root_hash).expect("verify second hash"));
    }

    /// A/B pair with `booted` running from `rootfs.0`; `boot_status` per slot.
    fn ab_slots(booted_status: &str, other_status: &str) -> Vec<SlotStatus> {
        let slot = |name: &str, state: &str, boot_status: &str| SlotStatus {
            name: name.to_string(),
            state: Some(state.to_string()),
            boot_status: Some(boot_status.to_string()),
            ..SlotStatus::default()
        };
        vec![
            slot("rootfs.0", "booted", booted_status),
            slot("rootfs.1", "inactive", other_status),
        ]
    }

    /// Poll `update.install.status` until it reads `want` or ~2s elapse.
    async fn wait_for_install_status(service: &MosdService, want: &str) {
        for _ in 0..200 {
            let state = service
                .get_state("update.install.status")
                .await
                .unwrap_or_default();
            if state == format!("\"{want}\"") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "update.install.status never became \"{want}\"; state: {}",
            service.get_state("").await.unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn reboot_reaches_the_power_control_and_is_recorded_first() {
        let (service, calls, _dir) = service_with_mock();

        service.request_reboot(":1.7").await.expect("reboot");

        assert_eq!(*calls.lock().expect("lock"), vec!["reboot".to_string()]);
        let state = service.get_state("power").await.expect("power state");
        let state: serde_json::Value = serde_json::from_str(&state).expect("json");
        assert_eq!(state["last_action"], "reboot");
        assert_eq!(state["requested_by"], ":1.7");
    }

    #[tokio::test]
    async fn power_off_reaches_the_power_control_and_is_recorded_first() {
        let (service, calls, _dir) = service_with_mock();

        service.request_power_off(":1.9").await.expect("power off");

        assert_eq!(*calls.lock().expect("lock"), vec!["power_off".to_string()]);
        let state = service.get_state("power").await.expect("power state");
        let state: serde_json::Value = serde_json::from_str(&state).expect("json");
        assert_eq!(state["last_action"], "power_off");
        assert_eq!(state["requested_by"], ":1.9");
    }

    #[tokio::test]
    async fn a_reboot_into_a_pending_slot_records_the_warning_first() {
        let (service, calls, _rauc_calls, _dir) = service_with_rauc(MockRauc {
            slots: ab_slots("good", "good"),
            // The bootloader's first pick is not the slot we run from: the
            // exact window in which this reboot burns a boot attempt.
            primary: Some("rootfs.1".to_string()),
            ..MockRauc::default()
        });

        service.request_reboot(":1.4").await.expect("reboot");

        assert_eq!(*calls.lock().expect("lock"), vec!["reboot".to_string()]);
        let power = service.get_state("power").await.expect("power state");
        let power: serde_json::Value = serde_json::from_str(&power).expect("json");
        assert_eq!(power["last_action"], "reboot");
        let warning = power["update_warning"]
            .as_str()
            .expect("a pending slot must put update_warning beside the action");
        assert!(warning.contains("rootfs.1"), "warning: {warning}");
        assert!(warning.contains("boot attempt"), "warning: {warning}");
    }

    #[tokio::test]
    async fn a_converged_system_reboots_without_an_update_warning() {
        let (service, calls, _rauc_calls, _dir) = service_with_rauc(MockRauc {
            slots: ab_slots("good", "good"),
            primary: Some("rootfs.0".to_string()),
            ..MockRauc::default()
        });

        service.request_reboot(":1.4").await.expect("reboot");

        assert_eq!(*calls.lock().expect("lock"), vec!["reboot".to_string()]);
        let power = service.get_state("power").await.expect("power state");
        let power: serde_json::Value = serde_json::from_str(&power).expect("json");
        assert!(
            power.get("update_warning").is_none(),
            "no warning means no key, not an empty one: {power}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_rauc_does_not_block_the_reboot() {
        // A v1 image or a wedged installer: the slot query fails, the reboot
        // still goes through, and no warning is invented.
        let (service, calls, _rauc_calls, _dir) = service_with_rauc(MockRauc {
            queries_fail: true,
            ..MockRauc::default()
        });

        service.request_reboot(":1.4").await.expect("reboot");

        assert_eq!(*calls.lock().expect("lock"), vec!["reboot".to_string()]);
        let power = service.get_state("power").await.expect("power state");
        let power: serde_json::Value = serde_json::from_str(&power).expect("json");
        assert!(power.get("update_warning").is_none(), "got: {power}");
    }

    #[tokio::test]
    async fn an_install_request_validates_the_path_before_touching_rauc() {
        let (service, _calls, rauc_calls, dir) = service_with_rauc(MockRauc::default());

        let relative = service
            .request_install(":1.5", "data/bundle.raucb")
            .await
            .expect_err("a relative path must be refused");
        assert!(relative.to_string().contains("absolute"), "{relative}");

        let missing = dir.path().join("no-such.raucb");
        service
            .request_install(":1.5", missing.to_str().expect("utf-8"))
            .await
            .expect_err("a missing file must be refused");

        service
            .request_install(":1.5", dir.path().to_str().expect("utf-8"))
            .await
            .expect_err("a directory must be refused");

        // A regular file outside <workspace>/verified, a `.part` inside it,
        // and a symbolic link inside it are all refused: nothing but a
        // verified bundle is handed to RAUC, whoever names the path.
        let outside = dir.path().join("outside.raucb");
        std::fs::write(&outside, b"bundle bytes").expect("seed");
        let refused = service
            .request_install(":1.5", outside.to_str().expect("utf-8"))
            .await
            .expect_err("a file outside verified/ must be refused");
        assert!(refused.to_string().contains("verified"), "{refused}");
        let part = verified_bundle(&dir, "half.raucb.part");
        let refused = service
            .request_install(":1.5", &part)
            .await
            .expect_err("a partial must be refused");
        assert!(refused.to_string().contains("partial"), "{refused}");
        let link = dir
            .path()
            .join("updates")
            .join("verified")
            .join("link.raucb");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        service
            .request_install(":1.5", link.to_str().expect("utf-8"))
            .await
            .expect_err("a symbolic link must be refused");

        assert!(
            rauc_calls.lock().expect("lock").is_empty(),
            "no invalid request may reach the installer"
        );
        assert!(
            service.get_state("update").await.is_err(),
            "a refused install must record nothing"
        );
    }

    /// A time observer the test controls, standing in for the two bus reads.
    struct FixedTimesync(crate::time_status::TimesyncEvidence);

    #[async_trait::async_trait]
    impl crate::time_status::TimeStatusSource for FixedTimesync {
        async fn observe(&self) -> anyhow::Result<crate::time_status::TimesyncEvidence> {
            Ok(self.0.clone())
        }
    }

    /// RFCT-300, through the method that serves it: an observation whose
    /// timedate1 read did not answer reaches the wire as `unknown` with the
    /// `synchronized` member absent, not as a device that was asked and found
    /// out of sync.
    ///
    /// The classifier is unit-tested next to itself; this is the seam that
    /// matters to a caller, because `GetTimeStatus` is where the tri-state
    /// stops being a `Option<bool>` and becomes a document a client reads.
    #[tokio::test]
    async fn the_time_status_reports_an_unread_kernel_bit_as_unknown() {
        use crate::time_status::TimesyncEvidence;
        let (service, _calls, _rauc_calls, _dir) = service_with_rauc(MockRauc::default());

        let observed = TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: None,
            server_name: Some("0.pool.ntp.org".to_string()),
            ..TimesyncEvidence::default()
        };
        let service = service.with_time_status(Arc::new(FixedTimesync(observed.clone())));
        let status = service.get_time_status().await.expect("observed");
        let status: serde_json::Value = serde_json::from_str(&status).expect("json");
        assert_eq!(status["status"], "unknown");
        assert!(status.get("synchronized").is_none(), "{status}");
        assert_eq!(status["server"]["name"], "0.pool.ntp.org");

        // The same observation with the bit actually read is the state the
        // unread one must not be confused with.
        let service = service.with_time_status(Arc::new(FixedTimesync(TimesyncEvidence {
            ntp_synchronized: Some(false),
            ..observed
        })));
        let status = service.get_time_status().await.expect("observed");
        let status: serde_json::Value = serde_json::from_str(&status).expect("json");
        assert_eq!(status["status"], "polling");
        assert_eq!(status["synchronized"], serde_json::json!(false));
    }

    /// A storage observer the test controls, standing in for sysfs and the
    /// mount table.
    struct FixedStorage(crate::storage_status::StorageEvidence);

    #[async_trait::async_trait]
    impl crate::storage_status::StorageStatusSource for FixedStorage {
        async fn observe(&self) -> anyhow::Result<crate::storage_status::StorageEvidence> {
            Ok(self.0.clone())
        }
    }

    /// Evidence for a DATA tier at `/srv` with `free` bytes left.
    fn data_evidence(free: u64) -> crate::storage_status::StorageEvidence {
        use crate::storage_status::{FsSpace, MountEvidence, StorageEvidence, TierEvidence};
        let mut tiers = std::collections::BTreeMap::new();
        tiers.insert(
            "data".to_string(),
            TierEvidence {
                device: Some("/dev/mmcblk0p11".to_string()),
                mount: Some(MountEvidence {
                    device: "/dev/mmcblk0p11".to_string(),
                    mount: "/srv".to_string(),
                    fstype: "ext4".to_string(),
                    read_only: false,
                }),
                space: Some(FsSpace {
                    total: 4 * crate::storage_status::UPDATE_WORKSPACE_RESERVED_BYTES,
                    used: 4 * crate::storage_status::UPDATE_WORKSPACE_RESERVED_BYTES - free,
                    free,
                    reserved: 0,
                }),
                ..TierEvidence::default()
            },
        );
        StorageEvidence {
            tiers,
            media: Vec::new(),
            binds: std::collections::BTreeMap::new(),
        }
    }

    /// The storage surface is served from the observer, and a daemon without
    /// one says so instead of answering with an empty layout.
    #[tokio::test]
    async fn the_storage_status_is_observed_and_absent_without_an_observer() {
        let (service, _calls, _rauc_calls, _dir) = service_with_rauc(MockRauc::default());
        let unobserved = service
            .get_storage_status()
            .await
            .expect_err("a daemon with no observer cannot answer");
        assert!(
            format!("{unobserved:?}").contains("storage"),
            "{unobserved:?}"
        );

        let service = service.with_storage_status(Arc::new(FixedStorage(data_evidence(
            10 * crate::storage_status::UPDATE_WORKSPACE_RESERVED_BYTES / 100,
        ))));
        let status = service.get_storage_status().await.expect("observed");
        let status: serde_json::Value = serde_json::from_str(&status).expect("json");
        let data = status["tiers"]
            .as_array()
            .expect("tiers")
            .iter()
            .find(|tier| tier["name"] == "data")
            .expect("a data tier")
            .clone();
        assert_eq!(data["mount"], "/srv");
        assert_eq!(data["pressure"], "critical");
        assert_eq!(data["updateWorkspace"]["available"], false);
    }

    /// The reservation, enforced at the install seam rather than only
    /// reported: an install onto a DATA tier with no workspace left is
    /// refused before the installer is touched.
    #[tokio::test]
    async fn an_install_is_refused_when_the_reserved_workspace_is_gone() {
        let (service, _calls, rauc_calls, dir) = service_with_rauc(MockRauc::default());
        let bundle = verified_bundle(&dir, "ok.raucb");

        let service = service.with_storage_status(Arc::new(FixedStorage(data_evidence(0))));
        let refused = service
            .request_install(":1.9", &bundle)
            .await
            .expect_err("a full DATA must refuse the install");
        assert!(
            refused.to_string().contains("reserved update workspace"),
            "{refused}"
        );
        assert!(
            rauc_calls.lock().expect("lock").is_empty(),
            "a refused install must not reach the installer"
        );
        assert!(
            service.get_state("update").await.is_err(),
            "a refused install must record nothing"
        );

        // The positive control: the same request with the workspace intact
        // is admitted, so the refusal above is the reservation and not a
        // second path failure.
        let service = service.with_storage_status(Arc::new(FixedStorage(data_evidence(
            crate::storage_status::UPDATE_WORKSPACE_RESERVED_BYTES,
        ))));
        service
            .request_install(":1.9", &bundle)
            .await
            .expect("an intact workspace admits the install");
        wait_for_install_status(&service, "done").await;
    }

    #[tokio::test]
    async fn an_install_runs_in_the_background_and_records_its_lifecycle() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (service, _calls, rauc_calls, dir) = service_with_rauc(MockRauc {
            install_gate: Some(Arc::clone(&gate)),
            ..MockRauc::default()
        });
        let bundle = verified_bundle(&dir, "ok.raucb");
        let bundle = bundle.as_str();

        // Returns while the install is still gated: the bus call cannot be
        // blocked by a slow installer.
        service
            .request_install(":1.6", bundle)
            .await
            .expect("install");
        wait_for_install_status(&service, "running").await;

        // A second install while one runs is refused, and the refusal names
        // the reason rather than queueing silently.
        let busy = service
            .request_install(":1.7", bundle)
            .await
            .expect_err("concurrent install must be refused");
        assert!(busy.to_string().contains("already running"), "{busy}");

        gate.notify_one();
        wait_for_install_status(&service, "done").await;

        let install = service.get_state("update.install").await.expect("state");
        let install: serde_json::Value = serde_json::from_str(&install).expect("json");
        assert_eq!(install["requested_by"], ":1.6");
        assert_eq!(install["bundle"], bundle);
        assert_eq!(
            *rauc_calls.lock().expect("lock"),
            vec![format!("install {bundle}")],
            "exactly the admitted install reached the installer"
        );
        // The completed install refreshed the whole update entry.
        let update = service.get_state("update").await.expect("state");
        let update: serde_json::Value = serde_json::from_str(&update).expect("json");
        assert_eq!(update["operation"], "idle");

        // The in-flight flag is released: a new install is admitted again.
        gate.notify_one();
        service
            .request_install(":1.8", bundle)
            .await
            .expect("install");
        wait_for_install_status(&service, "done").await;
    }

    #[tokio::test]
    async fn a_failed_install_records_the_error_and_releases_the_flag() {
        let (service, _calls, _rauc_calls, dir) = service_with_rauc(MockRauc {
            install_error: Some("signature verification failed".to_string()),
            ..MockRauc::default()
        });
        let bundle = verified_bundle(&dir, "bad.raucb");
        let bundle = bundle.as_str();

        service
            .request_install(":1.9", bundle)
            .await
            .expect("admitted");
        wait_for_install_status(&service, "failed").await;

        let install = service.get_state("update.install").await.expect("state");
        let install: serde_json::Value = serde_json::from_str(&install).expect("json");
        assert!(
            install["error"]
                .as_str()
                .is_some_and(|err| err.contains("signature verification failed")),
            "the failure reason must be recorded, got {install}"
        );
        service
            .request_install(":1.9", bundle)
            .await
            .expect("flag released");
    }

    /// PLAN-078 §S4, end to end: the recorded failure carries the device's own
    /// clock and its time state, so an operator reading one document can tell
    /// a real expiry from a wrong RTC.
    ///
    /// Both halves in one test, because the second is what catches an
    /// implementation that just always blames the clock.
    #[tokio::test]
    async fn a_failed_install_records_the_clock_beside_the_reason_without_blaming_it_wrongly() {
        use crate::time_status::TimesyncEvidence;

        // A clock nothing vouches for: timesyncd reachable, the kernel bit
        // read and false. This is the state a stranded device is in.
        let (undisciplined, _c, _r, dir) = service_with_rauc(MockRauc {
            install_error: Some(
                "signature verification failed: Verify error: certificate has expired".to_string(),
            ),
            ..MockRauc::default()
        });
        let undisciplined =
            undisciplined.with_time_status(Arc::new(FixedTimesync(TimesyncEvidence {
                service_reachable: true,
                ntp_synchronized: Some(false),
                ..TimesyncEvidence::default()
            })));
        let bundle = verified_bundle(&dir, "expired.raucb");
        undisciplined
            .request_install(":1.9", bundle.as_str())
            .await
            .expect("admitted");
        wait_for_install_status(&undisciplined, "failed").await;
        let install = undisciplined
            .get_state("update.install")
            .await
            .expect("state");
        let install: serde_json::Value = serde_json::from_str(&install).expect("json");
        assert!(
            install["time"]["clock"]
                .as_str()
                .is_some_and(|c| c.ends_with('Z')),
            "the device's own clock must be recorded beside the failure: {install}"
        );
        assert_eq!(install["time"]["status"]["status"], "offline-degraded");
        assert_eq!(install["time"]["clock_implicated"], true);

        // The SAME failure, on a clock the kernel vouches for. The time facts
        // are still rendered -- a reader must not need a second query -- and
        // the diagnosis does not attribute the failure to them.
        let (good, _c2, _r2, dir2) = service_with_rauc(MockRauc {
            install_error: Some(
                "signature verification failed: Verify error: certificate has expired".to_string(),
            ),
            ..MockRauc::default()
        });
        let good = good.with_time_status(Arc::new(FixedTimesync(TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: Some(true),
            ..TimesyncEvidence::default()
        })));
        let bundle2 = verified_bundle(&dir2, "really-expired.raucb");
        good.request_install(":1.9", bundle2.as_str())
            .await
            .expect("admitted");
        wait_for_install_status(&good, "failed").await;
        let install = good.get_state("update.install").await.expect("state");
        let install: serde_json::Value = serde_json::from_str(&install).expect("json");
        assert_eq!(install["time"]["status"]["status"], "synchronized");
        assert_eq!(
            install["time"]["clock_implicated"], false,
            "a good clock must not be blamed for a real expiry: {install}"
        );
    }

    #[tokio::test]
    async fn an_install_mid_flight_refuses_a_reboot_until_it_finishes() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (service, power_calls, _rauc_calls, dir) = service_with_rauc(MockRauc {
            install_gate: Some(Arc::clone(&gate)),
            ..MockRauc::default()
        });
        let bundle = std::path::PathBuf::from(verified_bundle(&dir, "ok.raucb"));
        service
            .request_install(":1.6", bundle.to_str().expect("utf-8"))
            .await
            .expect("install");
        wait_for_install_status(&service, "running").await;

        let refused = service
            .request_reboot(":1.7")
            .await
            .expect_err("the gate must refuse a reboot mid-install");
        assert!(refused.to_string().contains("install"), "{refused}");
        assert!(
            power_calls.lock().expect("lock").is_empty(),
            "a refused reboot must not reach the power control"
        );
        // No override lifts the install block.
        service
            .update_handle()
            .set_reboot_override(":1.7", 60)
            .await
            .expect("override armed");
        assert!(service.request_reboot(":1.7").await.is_err());

        gate.notify_one();
        wait_for_install_status(&service, "done").await;
        service.request_reboot(":1.7").await.expect("gate reopened");
        assert_eq!(*power_calls.lock().expect("lock"), vec!["reboot"]);
    }

    #[tokio::test]
    async fn a_blocking_health_report_refuses_a_reboot_until_overridden() {
        let (service, power_calls, _dir) = service_with_mock();
        service
            .report_health("exporter", "blocking", "mid-transaction")
            .await
            .expect("report");
        // `degraded` — mos-health's disk-pressure report — must NOT block.
        service
            .report_health("var", "degraded", "/var at 91% of capacity")
            .await
            .expect("report");

        let refused = service
            .request_reboot(":1.4")
            .await
            .expect_err("a blocking report closes the gate");
        assert!(refused.to_string().contains("exporter"), "{refused}");
        assert!(power_calls.lock().expect("lock").is_empty());

        // The reporter clearing its status reopens the gate without any
        // override — the ordinary path.
        service
            .report_health("exporter", "ok", "flushed")
            .await
            .expect("report");
        service.request_reboot(":1.4").await.expect("reopened");

        // And the bounded override lifts a standing block, audited.
        service
            .report_health("exporter", "blocking", "mid-transaction")
            .await
            .expect("report");
        assert!(service.request_reboot(":1.4").await.is_err());
        service
            .update_handle()
            .set_reboot_override(":1.4", 120)
            .await
            .expect("override armed");
        service.request_reboot(":1.4").await.expect("overridden");
        assert_eq!(*power_calls.lock().expect("lock"), vec!["reboot", "reboot"]);
    }

    #[tokio::test]
    async fn an_invalid_policy_file_fails_installs_closed() {
        let (service, _calls, rauc_calls, dir) = service_with_rauc(MockRauc::default());
        let policy_path = dir.path().join("update-policy.toml");
        std::fs::write(&policy_path, "not = valid = toml").expect("seed policy");
        let service = service.with_update(
            Arc::new(crate::update_lifecycle::NoClient),
            crate::update_policy::PolicyStore::at(policy_path),
        );
        let bundle = std::path::PathBuf::from(verified_bundle(&dir, "ok.raucb"));

        let refused = service
            .request_install(":1.5", bundle.to_str().expect("utf-8"))
            .await
            .expect_err("an unreadable policy file must fail closed");
        assert!(refused.to_string().contains("invalid"), "{refused}");
        assert!(
            rauc_calls.lock().expect("lock").is_empty(),
            "the refused install must not reach the installer"
        );
    }

    #[tokio::test]
    async fn the_refreshed_state_carries_the_derived_lifecycle_beside_the_slots() {
        let (service, _calls, _rauc_calls, _dir) = service_with_rauc(MockRauc {
            slots: ab_slots("good", "good"),
            primary: Some("rootfs.1".to_string()),
            ..MockRauc::default()
        });
        let rendered = service.refresh_update_state().await.expect("query");
        let rendered: serde_json::Value = serde_json::from_str(&rendered).expect("json");
        assert_eq!(rendered["lifecycle"]["state"], "reboot-required");
        assert_eq!(rendered["lifecycle"]["reboot_gate"]["safe"], true);
        assert_eq!(
            rendered["lifecycle"]["client"]["available"], false,
            "a daemon with no client must say so"
        );
        assert_eq!(rendered["lifecycle"]["policy"]["networkMode"], "online");
    }

    #[tokio::test]
    async fn update_state_is_queried_recorded_and_returned() {
        let (service, _calls, _rauc_calls, _dir) = service_with_rauc(MockRauc {
            slots: ab_slots("good", "good"),
            primary: Some("rootfs.1".to_string()),
            ..MockRauc::default()
        });

        let rendered = service.refresh_update_state().await.expect("query");
        let rendered: serde_json::Value = serde_json::from_str(&rendered).expect("json");
        assert_eq!(rendered["operation"], "idle");
        assert_eq!(rendered["booted_slot"], "rootfs.0");
        assert_eq!(rendered["primary"], "rootfs.1");
        assert_eq!(rendered["pending_not_confirmed"], true);
        assert_eq!(rendered["slots"]["rootfs.0"]["state"], "booted");

        // What was answered is exactly what was recorded.
        let recorded = service.get_state("update").await.expect("state");
        let recorded: serde_json::Value = serde_json::from_str(&recorded).expect("json");
        assert_eq!(recorded, rendered);
    }

    #[tokio::test]
    async fn an_unreachable_rauc_fails_the_query_and_records_nothing() {
        let (service, _calls, _rauc_calls, _dir) = service_with_rauc(MockRauc {
            queries_fail: true,
            ..MockRauc::default()
        });

        service
            .refresh_update_state()
            .await
            .expect_err("an unreachable installer is the caller's error, not a silent {}");
        assert!(
            service.get_state("update").await.is_err(),
            "a failed query must leave no half-recorded entry"
        );
    }

    #[tokio::test]
    async fn a_mark_is_validated_before_rauc_and_recorded_after() {
        let (service, _calls, rauc_calls, _dir) = service_with_rauc(MockRauc::default());

        // `active` exists in RAUC and is deliberately not offered.
        service
            .request_mark(":1.2", "active", "other")
            .await
            .expect_err("activation is the installer's job");
        // Concrete slot names are RAUC's, not this surface's.
        service
            .request_mark(":1.2", "good", "rootfs.0")
            .await
            .expect_err("slots are addressed as booted/other only");
        assert!(
            rauc_calls.lock().expect("lock").is_empty(),
            "no invalid mark may reach the installer"
        );

        let (slot_name, message) = service
            .request_mark(":1.2", "good", "booted")
            .await
            .expect("a valid mark");
        assert_eq!(slot_name, "rootfs.9");
        assert!(message.contains("good"), "message: {message}");
        assert_eq!(
            *rauc_calls.lock().expect("lock"),
            vec!["mark good booted".to_string()]
        );
        let last = service.get_state("update.last_mark").await.expect("state");
        let last: serde_json::Value = serde_json::from_str(&last).expect("json");
        assert_eq!(last["state"], "good");
        assert_eq!(last["slot"], "booted");
        assert_eq!(last["slot_name"], "rootfs.9");
        assert_eq!(last["requested_by"], ":1.2");
    }

    /// A service whose settings declare one WireGuard tunnel, with a rotation
    /// writing into the same throwaway directory.
    ///
    /// No reconcilers: this exercises the bus method's own contract, and the
    /// reconcile it triggers is the network reconciler's own tests' subject.
    fn service_with_wireguard() -> (MosdService, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut settings = mosd_settings::Settings::default();
        settings.network.insert(
            "wg0".to_string(),
            mosd_settings::IfaceSettings {
                kind: mosd_settings::IfaceKind::Wireguard,
                wireguard: Some(mosd_settings::WireguardConfig::default()),
                ..mosd_settings::IfaceSettings::default()
            },
        );
        settings.network.insert(
            "eth0".to_string(),
            mosd_settings::IfaceSettings {
                dhcp: true,
                ..mosd_settings::IfaceSettings::default()
            },
        );
        let service = MosdService::new(
            mosd_settings::Store::new(dir.path().join("settings.toml")),
            settings,
            Vec::new(),
            Box::new(MockPower {
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
            dir.path().join("shadow"),
            serde_json::json!({}),
        )
        .with_wireguard(Arc::new(crate::reconciler::network::KeyRotation::new(
            crate::wgkeys::Keystore::under(dir.path(), None),
            crate::reconciler::network::NoDelete,
        )));
        (service, dir)
    }

    #[tokio::test]
    async fn a_rotation_returns_the_new_public_key_and_writes_no_key_into_the_tree() {
        let (service, dir) = service_with_wireguard();
        let before = service.get_settings("").await.expect("settings");

        let first = service
            .rotate_wireguard_key("wg0")
            .await
            .expect("rotate wireguard key");
        let second = service
            .rotate_wireguard_key("wg0")
            .await
            .expect("rotate wireguard key");

        assert_ne!(
            first, second,
            "a rotation that returns the same key rotated nothing"
        );
        let key_file = dir.path().join("networkd-secrets/wg-wg0.key");
        let private_key = std::fs::read_to_string(&key_file).expect("key file");
        // The tree holds no key field to write into, and the rotation writes
        // none: what a client reads over `GetSettings` is what it read before.
        assert_eq!(service.get_settings("").await.expect("settings"), before);
        assert!(!before.contains(private_key.trim()), "{before}");
        assert!(!second.contains(private_key.trim()));
        // And nothing was persisted: a rotation is not a settings write.
        assert!(!dir.path().join("settings.toml").exists());
    }

    /// The two refusals, and the error name each one travels under.
    ///
    /// **The names are the assertion, not decoration**. Both
    /// conditions were `InvalidArgs` until this split, so apid answered 422 for
    /// an interface that does not exist — where every other read on the API
    /// answers 404 for a path that names nothing. apid needed no change: it
    /// already maps [`NOT_FOUND_ERROR`] to 404 and `InvalidArgs` to 422, and
    /// had only been handed one of them.
    #[tokio::test]
    async fn a_rotation_refuses_an_interface_that_is_not_a_tunnel() {
        use zbus::DBusError as _;
        let (service, dir) = service_with_wireguard();

        let not_a_tunnel = service.rotate_wireguard_key("eth0").await.unwrap_err();
        let not_declared = service.rotate_wireguard_key("wg9").await.unwrap_err();

        let message = not_a_tunnel.description().unwrap_or_default();
        assert!(
            message.contains("is not a WireGuard interface"),
            "{message}"
        );
        assert_eq!(
            not_a_tunnel.name().as_str(),
            "org.freedesktop.DBus.Error.InvalidArgs",
            "an entry of the wrong kind is a bad argument: {message}"
        );

        let message = not_declared.description().unwrap_or_default();
        assert!(
            message.contains("is not a declared network entry"),
            "{message}"
        );
        assert_eq!(
            not_declared.name().as_str(),
            super::NOT_FOUND_ERROR,
            "an undeclared entry names nothing, which is a 404 and not a 422: {message}"
        );
        // Refused before the key store is reached, so no key was drawn for an
        // interface that has no business having one.
        assert!(!dir.path().join("secrets").exists());
    }

    /// `GetState`'s two failure paths, and the error name each one travels
    /// under.
    ///
    /// **The names are the assertion.** Both used to be readable only by
    /// guessing from the message: an unresolvable dot-path raised fdo
    /// `InvalidArgs`, the same name a rejected *value* travels under, so apid
    /// could not tell "this path names nothing" from "this argument is bad"
    /// without knowing that on this one route the name had a single producer.
    /// It answered 404 by reading the name against that private fact
    /// The condition is the same
    /// one [`a_rotation_refuses_an_interface_that_is_not_a_tunnel`] split for
    /// the rotate-key path on record — a path that names nothing is
    /// [`NOT_FOUND_ERROR`] — and this brings the state read into line with it.
    #[tokio::test]
    async fn a_state_path_that_does_not_resolve_is_not_found() {
        use zbus::DBusError as _;
        let (service, _calls, _dir) = service_with_mock();

        let unresolvable = service.get_state("no.such.path").await.unwrap_err();

        let message = unresolvable.description().unwrap_or_default();
        assert!(message.contains("state path not found"), "{message}");
        assert_eq!(
            unresolvable.name().as_str(),
            super::NOT_FOUND_ERROR,
            "a dot-path that resolves to nothing names nothing, which is a 404 \
             and not a 422: {message}"
        );
    }

    #[tokio::test]
    async fn a_daemon_with_no_key_store_rotates_nothing() {
        let (service, _calls, dir) = service_with_mock();
        let mut settings = mosd_settings::Settings::default();
        settings.network.insert(
            "wg0".to_string(),
            mosd_settings::IfaceSettings {
                kind: mosd_settings::IfaceKind::Wireguard,
                wireguard: Some(mosd_settings::WireguardConfig::default()),
                ..mosd_settings::IfaceSettings::default()
            },
        );
        service
            .write_setting("network", serde_json::to_value(&settings.network).unwrap())
            .await
            .expect("declare the tunnel");

        let err = service.rotate_wireguard_key("wg0").await.unwrap_err();

        // The dry-run default: a daemon that was never handed a key store has
        // nowhere to put a key, and says so instead of inventing a place.
        let message = zbus::DBusError::description(&err).unwrap_or_default();
        assert!(message.contains("no WireGuard key store"), "{message}");
        assert!(!dir.path().join("secrets").exists());
    }

    #[tokio::test]
    async fn power_requests_do_not_touch_the_settings_tree() {
        let (service, _calls, _dir) = service_with_mock();
        let before = service.get_settings("").await.expect("settings");

        service.request_reboot(":1.1").await.expect("reboot");
        service.request_power_off(":1.1").await.expect("power off");

        assert_eq!(service.get_settings("").await.expect("settings"), before);
    }

    /// The uptime graft: `GetState` serves whole seconds since boot at
    /// `uptime` and inside the whole tree, computed at read time, while the
    /// stored tree — what the item façade projects — stays untouched by the
    /// read.
    #[tokio::test]
    async fn get_state_serves_uptime_without_storing_it() {
        let (service, _calls, _dir) = service_with_mock();

        let direct = service.get_state("uptime").await.expect("uptime");
        let direct: u64 = serde_json::from_str(&direct).expect("a bare JSON number");
        let whole = service.get_state("").await.expect("whole tree");
        let whole: serde_json::Value = serde_json::from_str(&whole).expect("json");
        let grafted = whole["uptime"].as_u64().expect("uptime in the whole tree");
        assert!(
            grafted >= direct,
            "uptime went backwards: {grafted} < {direct}"
        );

        let (_settings, state) = service.trees().await;
        assert!(
            state.get("uptime").is_none(),
            "a read must not write the stored tree: {state}"
        );
    }

    /// Every `SettingsError` variant, against the error name it must travel
    /// under: the two conditions the fdo vocabulary cannot separate get
    /// interface-scoped names, everything else keeps its standard fdo name.
    #[test]
    fn each_settings_failure_travels_under_its_own_error_name() {
        use mosd_settings::SettingsError;
        use zbus::DBusError as _;

        for (err, name) in [
            (
                SettingsError::NotFound("a.path".into()),
                "com.mos.mosd1.Error.NotFound",
            ),
            (
                SettingsError::ReadOnly("a.path".into()),
                "com.mos.mosd1.Error.ReadOnly",
            ),
            (
                SettingsError::Validation {
                    path: "a.path".into(),
                    message: "bad".into(),
                },
                "org.freedesktop.DBus.Error.InvalidArgs",
            ),
            (
                SettingsError::Io(std::io::Error::other("disk")),
                "org.freedesktop.DBus.Error.IOError",
            ),
            (
                SettingsError::Parse("mangled".into()),
                "org.freedesktop.DBus.Error.Failed",
            ),
            (
                SettingsError::Migration("stuck".into()),
                "org.freedesktop.DBus.Error.Failed",
            ),
        ] {
            let message = err.to_string();
            let fault = super::to_bus_error(err);
            assert_eq!(fault.name().as_str(), name);
            assert_eq!(
                fault.description(),
                Some(message.as_str()),
                "the description must stay mosd's own words ({name})"
            );
        }
    }

    #[tokio::test]
    async fn a_transient_password_does_not_touch_the_settings_tree() {
        let (service, _calls, dir) = service_with_mock();
        let before = service.get_settings("").await.expect("settings");

        service
            .set_transient_root_password("correct horse battery")
            .await
            .expect("set transient root password");

        assert_eq!(
            service.get_settings("").await.expect("settings"),
            before,
            "a password must never enter the settings tree"
        );
        assert!(
            !before.contains("correct horse"),
            "the fixture itself must not carry the password"
        );
        assert!(
            !dir.path().join("settings.toml").exists(),
            "nothing was persisted, so no settings file was written at all"
        );
        assert!(
            crate::transient::transient_password_active(&dir.path().join("shadow")),
            "the call must still have done its actual job"
        );
    }

    #[tokio::test]
    async fn a_rejected_transient_password_is_an_error_that_does_not_echo_it() {
        let (service, _calls, dir) = service_with_mock();

        let err = service
            .set_transient_root_password("short12")
            .await
            .expect_err("seven bytes is below the floor");

        assert!(
            !err.to_string().contains("short12"),
            "the password leaked into the D-Bus error: {err}"
        );
        assert!(
            !crate::transient::transient_password_active(&dir.path().join("shadow")),
            "a rejected password must leave no marker behind"
        );
    }

    #[test]
    fn root_matches_everything() {
        assert!(paths_overlap("", "network"));
        assert!(paths_overlap("network", ""));
        assert!(paths_overlap("", ""));
        assert!(paths_overlap(".", "hostname"));
        assert!(paths_overlap("hostname", "."));
    }

    #[test]
    fn prefix_matches_both_directions() {
        assert!(paths_overlap("network.eth0.dhcp", "network"));
        assert!(paths_overlap("network", "network.eth0.dhcp"));
        assert!(paths_overlap("hostname", "hostname"));
    }

    #[test]
    fn disjoint_paths_do_not_match() {
        assert!(!paths_overlap("hostname", "network"));
        assert!(!paths_overlap("network.eth0", "network2"));
        assert!(!paths_overlap("net", "network"));
    }

    struct FixedSystemInfo(crate::system_info::SystemInfoEvidence);

    #[async_trait::async_trait]
    impl crate::system_info::SystemInfoSource for FixedSystemInfo {
        async fn observe(&self) -> anyhow::Result<crate::system_info::SystemInfoEvidence> {
            Ok(self.0.clone())
        }
    }

    /// The system-information surface: absent without an observer, and with
    /// one it carries the booted slot read from the RAUC client the service
    /// already holds — one client, not a second reader of the installer.
    #[tokio::test]
    async fn the_system_info_is_observed_with_the_booted_slot_and_absent_without_an_observer() {
        let (service, _calls, _rauc_calls, _dir) = service_with_rauc(MockRauc {
            slots: ab_slots("good", "good"),
            primary: Some("rootfs.0".to_string()),
            ..MockRauc::default()
        });
        let err = service
            .get_system_info()
            .await
            .expect_err("no observer means no answer");
        assert!(format!("{err:?}").contains("observes no system information"));

        let evidence = crate::system_info::SystemInfoEvidence {
            machine_id: Ok("0123456789abcdef0123456789abcdef".to_string()),
            uptime_seconds: Some(42),
            ..crate::system_info::SystemInfoEvidence::default()
        };
        let service = service.with_system_info(Arc::new(FixedSystemInfo(evidence)));
        let info: serde_json::Value =
            serde_json::from_str(&service.get_system_info().await.expect("observed")).unwrap();
        assert_eq!(info["machineId"]["id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(info["uptime"]["seconds"], 42);
        assert_eq!(info["slot"]["available"], true);
        assert_eq!(info["slot"]["booted"], "rootfs.0");
        assert_eq!(info["slot"]["primary"], "rootfs.0");
        assert_eq!(info["daemon"]["name"], "mosd");
        // A member the fixture did not supply is absent with a reason, not
        // manufactured.
        assert_eq!(info["board"]["available"], false);
    }

    /// The other three PLAN-052 reads refuse without an observer, so a
    /// dry-run daemon can never inspect its host through them.
    #[tokio::test]
    async fn the_diagnostic_reads_are_absent_without_observers() {
        let (service, _calls, _dir) = service_with_mock();
        assert!(service.get_telemetry().await.is_err());
        assert!(service.get_observed_network().await.is_err());
        assert!(service.get_failure_evidence().await.is_err());

        let service = service
            .with_telemetry(Arc::new(crate::telemetry::SysfsTelemetry::at(_dir.path())))
            .with_failure_evidence(Arc::new(crate::diagnostics::HostFailureEvidence::new(
                Box::new(EmptyJournal),
                Box::new(NoUnits),
            )));
        let telemetry: serde_json::Value =
            serde_json::from_str(&service.get_telemetry().await.expect("telemetry")).unwrap();
        assert_eq!(telemetry["reset"]["reason"], "unknown");
        assert_eq!(telemetry["reset"]["available"], false);
        let failures: serde_json::Value =
            serde_json::from_str(&service.get_failure_evidence().await.expect("failures")).unwrap();
        assert_eq!(failures["journal"]["lineCount"], 0);
        assert_eq!(failures["units"]["count"], 0);
    }

    struct EmptyJournal;

    #[async_trait::async_trait]
    impl crate::diagnostics::JournalReader for EmptyJournal {
        async fn read(&self, _max_lines: usize) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    struct NoUnits;

    #[async_trait::async_trait]
    impl crate::diagnostics::UnitLister for NoUnits {
        async fn failed_units(&self) -> anyhow::Result<Vec<crate::diagnostics::FailedUnit>> {
            Ok(Vec::new())
        }
    }
}

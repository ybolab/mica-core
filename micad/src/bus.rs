//! D-Bus service implementation for `com.mica.micad`.
//!
//! Exposes the settings tree and the live-state tree on the bus as the
//! `com.mica.micad1` interface at [`OBJECT_PATH`], owned under [`BUS_NAME`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use micad_settings::{
    ACTOR_POLICY, DocumentRefusal, REQUESTED, Settings, SettingsError, Store, append_audit_line,
    audit_line, audit_ring_dir, json_path_get,
};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use zbus::fdo;
use zbus::message::Header;
use zbus::object_server::{InterfaceRef, SignalEmitter};

use crate::apply_queue::{ApplyJob, ApplyQueue, TaskRecord};
use crate::deployment::{self, DeploymentClient, NativeClient};
use crate::diagnostics::{FailureEvidenceSource, UnavailableFailureEvidence};
use crate::network_state::{NetworkState, UnavailableNetworkState};
use crate::power::PowerControl;
use crate::reconciler::Reconciler;
use crate::reconciler::network::WireguardRotate;
use crate::scan::Registry;
use crate::storage_status::{self, PressureTracker, StorageStatusSource, UnavailableStorageStatus};
use crate::system_info::{self, SystemInfoSource, UnavailableSystemInfo};
use crate::telemetry::{self, TelemetrySource, UnavailableTelemetry};
use crate::time_status::{self, ClockTrust, TimeStatusSource, UnavailableTimeStatus, status_json};
use crate::transient;
use crate::update_auto::{self, AutoRoutes, UpdateFacts};
use crate::update_codes;
use crate::update_lifecycle::{
    Available, DEFAULT_WORKSPACE_ROOT, LifecycleHost, NoClient, Refusal, Settled, UpdateClient,
    UpdateLifecycle,
};
use crate::update_policy::{LoadedPolicy, PolicyStore};

/// Well-known bus name owned by the daemon.
pub const BUS_NAME: &str = "com.mica.micad";
/// Object path the service is registered at.
pub const OBJECT_PATH: &str = "/com/mos/micad";

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

/// The `com.mica.micad1` service: settings tree, live-state tree, store,
/// reconcilers, the power control, the update installer client and the shadow
/// file a transient root password is written into.
pub struct MosdService {
    store: Store,
    reconcilers: Arc<Vec<Box<dyn Reconciler>>>,
    power: Box<dyn PowerControl>,
    deployments: Arc<dyn DeploymentClient>,
    /// Shared interlock for install admission and reboot refusal.
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
    /// The `/mos/config/` documents this boot found and refused (PLAN-070
    /// §5.2.7, F6g — **the pour**).
    ///
    /// Empty on every device whose documents parse, which is every device that
    /// was configured through the API. It fills when an integrator pours a
    /// document by hand and gets it wrong, and what it buys is that the mistake
    /// costs exactly the subsystems that document configures:
    /// [`reconcile_subtree`] skips their reconcilers and records the refusal
    /// where the applied state would go, rather than applying a schema default
    /// nobody chose.
    ///
    /// Mutable because a refusal ends: an authenticated write that reaches the
    /// refused subtree rewrites the document, which is §5.2.7's sanctioned
    /// repair, and [`Self::persist_setting`] clears it there.
    refusals: Arc<RwLock<Vec<DocumentRefusal>>>,
}

/// The lifecycle's window onto this service: it records under
/// `update.lifecycle` and reads the `health` subtree, and nothing else.
struct InnerLifecycleHost(Arc<RwLock<Inner>>);

#[async_trait::async_trait]
impl LifecycleHost for InnerLifecycleHost {
    async fn record(&self, lifecycle: Value) {
        let mut inner = self.0.write().await;
        update_entry(&mut inner.state).insert("lifecycle".into(), lifecycle);
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

fn update_entry(state: &mut Value) -> &mut serde_json::Map<String, Value> {
    state
        .as_object_mut()
        .expect("live-state root is always an object")
        .entry("update")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .expect("update is always an object")
}

/// The rotation a daemon with no key store has: none.
///
/// The dry-run shape [`NoClient`] and [`crate::power::DryRunPower`]
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
            deployments: Arc::new(NativeClient::new(Arc::new(NoClient))),
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
            refusals: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Record the `/mos/config/` documents this boot refused, and publish them
    /// into the live-state tree (PLAN-070 §5.2.7, F6g).
    ///
    /// An `async` setter rather than a `with_*` builder step, unlike every
    /// other attachment on this type, because it does two things and the second
    /// one needs the tree: the refusals have to be readable by a reconcile, and
    /// they have to be legible to an operator who is looking at the device
    /// wondering why its Wi-Fi is not up. `main.rs` calls it before
    /// [`Self::apply_all`], which is what makes the first reconcile of the boot
    /// already see them.
    pub async fn set_config_refusals(&self, refusals: Vec<DocumentRefusal>) {
        *self.refusals.write().await = refusals;
        self.publish_refusals().await;
    }

    /// Mirror the current refusals into `configuration.refused` in the
    /// live-state tree.
    ///
    /// A list at a fixed place, and not only the per-reconciler entries
    /// [`reconcile_subtree`] writes, because the two answer different
    /// questions: the per-reconciler entry says why *this* subsystem is not
    /// running, and this says what the boot found — including a document with
    /// no reconciler behind it, which the other form cannot report at all.
    async fn publish_refusals(&self) {
        let refused: Vec<Value> = self
            .refusals
            .read()
            .await
            .iter()
            .map(|refusal| {
                serde_json::json!({
                    "document": refusal.document,
                    "path": refusal.path.display().to_string(),
                    "message": refusal.message,
                    "subtrees": refusal.subtrees,
                })
            })
            .collect();
        let mut inner = self.inner.write().await;
        if let Some(root) = inner.state.as_object_mut() {
            root.insert(
                "configuration".to_string(),
                serde_json::json!({ "refused": refused }),
            );
        }
    }

    /// Attach the native command transport and operator policy.
    #[must_use]
    pub fn with_update(mut self, client: Arc<dyn UpdateClient>, policy: PolicyStore) -> Self {
        self.deployments = Arc::new(NativeClient::new(Arc::clone(&client)));
        self.update = Arc::new(UpdateLifecycle::new(
            client,
            policy,
            Arc::new(InnerLifecycleHost(Arc::clone(&self.inner))),
            Arc::clone(&self.installing),
            self.update.workspace_root().to_path_buf(),
        ));
        self
    }

    #[cfg(test)]
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
    /// A builder step for the same reason [`Self::with_update`] is: the default
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

    #[cfg(test)]
    #[must_use]
    pub fn with_deployments(mut self, deployments: Arc<dyn DeploymentClient>) -> Self {
        self.deployments = deployments;
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
    /// under `power`, with `update_warning` — an unconfirmed-deployment warning, when
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

    /// Read authenticated deployment evidence without delaying unrelated power or information calls.
    async fn deployment_evidence(&self) -> Option<deployment::Status> {
        match tokio::time::timeout(Duration::from_secs(2), self.deployments.status()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(error)) => {
                tracing::debug!(%error, "deployment status unavailable");
                None
            }
            Err(_) => {
                tracing::warn!("deployment query timed out");
                None
            }
        }
    }

    async fn reboot_update_warning(&self) -> Option<String> {
        let status = self.deployment_evidence().await?;
        let (phase, reason) = status.phase();
        matches!(phase, "reboot-required" | "validating").then_some(reason)
    }

    /// Reboot the machine on behalf of `sender`.
    ///
    /// Update-aware: the deployment status is read first, and a deployment that is
    /// installed-but-not-confirmed — a reboot into it burns one of its
    /// boot attempts — is logged and recorded in the `power` live-state entry
    /// BEFORE the reboot fires. A warning, not a refusal: booting the new deployment
    /// is exactly what the operator installing an update wants, and the
    /// attempt-burning edge case (rebooting *again* before the health gate
    /// confirms) is one the operator must be able to drive through anyway.
    ///
    /// Split out from the D-Bus method so unit tests can drive it without
    /// forging a message header.
    pub async fn request_reboot(&self, sender: &str) -> fdo::Result<()> {
        // The safe-to-reboot interlock, and the one REFUSAL on this path.
        // Distinct from the unconfirmed-deployment warning below, which stays a
        // warning: booting a fresh deployment is what an updating operator wants,
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
            tracing::warn!(warning, "rebooting with an unconfirmed deployment");
        }
        self.note_power_request("reboot", sender, warning).await;
        self.power
            .reboot()
            .await
            .map_err(|err| fdo::Error::Failed(format!("reboot: {err}")))
    }

    /// Power the machine off on behalf of `sender`.
    ///
    /// No unconfirmed-deployment warning here, deliberately: a power-off does not
    /// boot anything, so it spends no boot attempt. The attempt is spent by
    /// whatever powers the machine back ON, which is not an event micad can
    /// see, let alone warn about.
    pub async fn request_power_off(&self, sender: &str) -> fdo::Result<()> {
        self.note_power_request("power_off", sender, None).await;
        self.power
            .power_off()
            .await
            .map_err(|err| fdo::Error::Failed(format!("power off: {err}")))
    }

    /// Admit a verified descriptor and record the asynchronous native installation.
    pub async fn request_install(&self, sender: &str, descriptor_path: &str) -> fdo::Result<()> {
        if let Some(refusal) = self.update.install_refusal().await {
            return Err(fdo::Error::AccessDenied(refusal));
        }
        let descriptor = PathBuf::from(descriptor_path);
        self.update
            .installable(&descriptor)
            .map_err(fdo::Error::InvalidArgs)?;
        if self
            .installing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(fdo::Error::Failed(
                "an update install is already running".into(),
            ));
        }
        let id = descriptor
            .file_stem()
            .expect("validated descriptor")
            .to_string_lossy()
            .into_owned();
        tracing::warn!(deployment_id = id, sender, "deployment install requested");
        update_entry(&mut self.inner.write().await.state).insert(
            "install".into(),
            serde_json::json!({"status":"running","deploymentId":id,"requested_by":sender}),
        );
        let client = Arc::clone(&self.deployments);
        let inner = Arc::clone(&self.inner);
        let installing = Arc::clone(&self.installing);
        let lifecycle = Arc::clone(&self.update);
        let sender = sender.to_owned();
        tokio::spawn(async move {
            let outcome = match client.install(&descriptor).await {
                Ok(()) => {
                    serde_json::json!({"status":"done","deploymentId":id,"requested_by":sender})
                }
                Err(error) => {
                    tracing::error!(deployment_id = id, %error, "deployment install failed");
                    serde_json::json!({"status":"failed","deploymentId":id,"requested_by":sender,
                        "error":format!("{error:#}"),"error_code":update_codes::CLIENT_EXIT_FAILURE})
                }
            };
            let status = client.status().await.ok();
            let mut guard = inner.write().await;
            let entry = update_entry(&mut guard.state);
            entry.insert("install".into(), outcome);
            if let Some(status) = &status
                && let Err(error) = status.merge_into(entry)
            {
                tracing::error!(%error, "deployment status encoding failed");
            }
            drop(guard);
            installing.store(false, Ordering::Release);
            if let Some(status) = status {
                lifecycle.installed(&status).await;
            }
        });
        Ok(())
    }

    /// Refresh every deployment fact from the authenticated native backend.
    pub async fn refresh_update_state(&self) -> fdo::Result<String> {
        let status = self
            .deployments
            .status()
            .await
            .map_err(|error| fdo::Error::Failed(format!("query deployments: {error:#}")))?;
        self.update.refresh(&status).await;
        let mut inner = self.inner.write().await;
        let entry = update_entry(&mut inner.state);
        status
            .merge_into(entry)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        Ok(Value::Object(entry.clone()).to_string())
    }

    /// Explicit operator action; automatic confirmation belongs to the boot health gate.
    pub async fn request_deployment_action(
        &self,
        sender: &str,
        action: &str,
        id: &str,
    ) -> fdo::Result<()> {
        if !deployment::valid_id(id) || !matches!(action, "confirm" | "reject" | "rollback") {
            return Err(fdo::Error::InvalidArgs(
                "invalid deployment action or ID".into(),
            ));
        }
        if self.installing.load(Ordering::Acquire) {
            return Err(fdo::Error::Failed("an update install is running".into()));
        }
        let status = self
            .deployments
            .status()
            .await
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        if action != "reject" && status.boot.deployment_id != id {
            return Err(fdo::Error::InvalidArgs(
                "action must name the running deployment".into(),
            ));
        }
        tracing::warn!(
            sender,
            action,
            deployment_id = id,
            "manual deployment action requested"
        );
        match action {
            "confirm" => self.deployments.confirm().await,
            "reject" => self.deployments.reject(id).await,
            "rollback" => self.deployments.rollback().await,
            _ => unreachable!("validated action"),
        }
        .map_err(|error| fdo::Error::Failed(format!("{action}: {error:#}")))?;
        update_entry(&mut self.inner.write().await.state).insert(
            "last_action".into(),
            serde_json::json!({"action":action,"deploymentId":id,"requested_by":sender}),
        );
        self.refresh_update_state().await?;
        Ok(())
    }

    /// The clock-trust evidence PLAN-071 §7's automatic-install predicate
    /// reads: `docs/design/time.md` §5's classification, and §3's saved
    /// floor.
    ///
    /// Both limbs are that document's, deliberately, rather than a notion of
    /// trusted time invented for updates — the same evidence RFCT-311's S4
    /// puts beside a failed install, asked here as a precondition instead of
    /// as a diagnosis. A daemon with no observer answers `None` for the
    /// first limb, which is unread evidence and not a claim, exactly as
    /// `unknown` is on the status surface.
    pub async fn clock_trust(&self) -> ClockTrust {
        let status = self
            .time_status
            .observe()
            .await
            .ok()
            .map(|evidence| time_status::classify(&evidence));
        time_status::observed_clock_trust(status)
    }

    /// Validate and atomically persist `value` without waiting for a
    /// reconcile. The D-Bus method enqueues the apply after this returns.
    ///
    /// # Errors
    ///
    /// Whatever [`Settings::set`](micad_settings::Settings::set) or
    /// [`Store::save`] rejected the write with.
    async fn persist_setting(&self, path: &str, value: Value) -> Result<(), SettingsError> {
        // **A refused document keeps its bytes unless this write is the one
        // that repairs it** (PLAN-070 §5.2.7, F6g). A refused subtree sits at
        // its schema default in the addressed tree, and `save` writes every
        // document out of that tree — so without this, an operator setting the
        // hostname would replace a poured `wifi.json` with the default nobody
        // chose, which is the silent revert the fail-closed rule forbids,
        // arriving one write later than the load.
        //
        // The document this write DOES reach is deliberately not preserved:
        // §5.2.7 says a human edits these documents through an authenticated
        // API, so that write is the repair and has to land.
        let preserve: Vec<String> = self
            .refusals
            .read()
            .await
            .iter()
            .filter(|refusal| !refusal_covers(refusal, path))
            .map(|refusal| refusal.document.clone())
            .collect();
        {
            let mut inner = self.inner.write().await;
            let mut candidate = inner.settings.clone();
            candidate.set(path, value)?;
            let preserve: Vec<&str> = preserve.iter().map(String::as_str).collect();
            self.store.preserving(&preserve).save(&candidate)?;
            inner.settings = candidate;
        }
        // The write landed, so the document it reached now holds what this
        // build writes and parses as this build reads: its refusal is over, and
        // the reconcilers it was gating run again on the apply that follows.
        let repaired = {
            let mut refusals = self.refusals.write().await;
            let before = refusals.len();
            refusals.retain(|refusal| !refusal_covers(refusal, path));
            before != refusals.len()
        };
        if repaired {
            self.publish_refusals().await;
        }
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
        reconcile_subtree(&self.reconcilers, &self.inner, &self.refusals, path).await
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
            Arc::clone(&self.refusals),
        ));
    }

    /// Direct form retained only for unit tests whose subject is scoping or
    /// serialization. The production D-Bus member queues the apply and is
    /// exercised over a real bus by `micad/tests/bus.rs`.
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
    refusals: &RwLock<Vec<DocumentRefusal>>,
    path: &str,
) -> Vec<String> {
    let settings = inner.read().await.settings.clone();
    let refused = refusals.read().await.clone();
    let mut failures = Vec::new();
    for reconciler in reconcilers {
        if !paths_overlap(path, reconciler.subtree()) {
            continue;
        }
        // **"Refuses its subsystem" is enforced here** (PLAN-070 §5.2.7,
        // F6g). The reconciler whose settings come out of a document that did
        // not load is skipped rather than run against that document's schema
        // default, and the refusal takes the place of the applied state so the
        // live-state tree says why. Its neighbours are untouched: the loop
        // moves on, so a poured typo in one document costs exactly what that
        // document configures.
        if let Some(refusal) = refused
            .iter()
            .find(|refusal| refusal_covers(refusal, reconciler.subtree()))
        {
            tracing::warn!(
                reconciler = reconciler.name(),
                document = refusal.document,
                "reconciler skipped: the document that configures it did not load"
            );
            let mut inner = inner.write().await;
            if let Some(map) = inner.state.as_object_mut() {
                map.insert(
                    reconciler.name().to_string(),
                    serde_json::json!({ "refused": refusal.message }),
                );
            }
            continue;
        }
        let result = reconciler.apply(&settings).await;
        let mut inner = inner.write().await;
        if let Some(failure) = record(&mut inner.state, reconciler.name(), result) {
            failures.push(failure);
        }
    }
    failures
}

/// Whether `refusal` covers the dot-path `path`.
///
/// Two callers ask it: the reconcile loop, where `path` is a reconciler's
/// declared subtree and the answer is "this subsystem is refused"; and
/// [`MosdService::persist_setting`], where `path` is the write and the answer
/// is "this write repairs that document". One relation, because they are one
/// question about the same table — which capabilities that document gates.
///
/// [`paths_overlap`] in both directions, the same relation the reconcile loop
/// already scopes with: `wifi.json` carries `wifi`, so it gates `wifi.client`
/// and a write to `wifi.ap.ssid` reaches it.
fn refusal_covers(refusal: &DocumentRefusal, path: &str) -> bool {
    refusal
        .subtrees
        .iter()
        .any(|subtree| paths_overlap(path, subtree))
}

async fn run_apply_worker(
    queue: Arc<ApplyQueue>,
    inner: Arc<RwLock<Inner>>,
    apply_lock: Arc<Mutex<()>>,
    reconcilers: Arc<Vec<Box<dyn Reconciler>>>,
    refusals: Arc<RwLock<Vec<DocumentRefusal>>>,
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
            reconcile_subtree(&reconcilers, &inner, &refusals, &dot_path).await
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
pub const NOT_FOUND_ERROR: &str = "com.mica.micad1.Error.NotFound";
/// D-Bus error name for a settings dot-path that exists but rejects writes.
pub const READ_ONLY_ERROR: &str = "com.mica.micad1.Error.ReadOnly";

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
        // The configuration medium being gone is an IO condition and not a
        // malformed request: the caller asked for something reasonable and the
        // device cannot reach the store. The message already names the mount.
        SettingsError::Io(_) | SettingsError::Unavailable { .. } => {
            SettingsFault::Fdo(fdo::Error::IOError(err.to_string()))
        }
        SettingsError::Parse(_) | SettingsError::SchemaVersion(_) => {
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

#[zbus::interface(name = "com.mica.micad1")]
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
    /// installed packages, the running deployment and the uptime — every one read
    /// at call time from the seam that already carries it, none restated.
    ///
    /// Observed rather than stored, the `uptime` reasoning again: the deployment
    /// and the uptime move without any settings write. Read-only.
    async fn get_system_info(&self) -> Result<String, SettingsFault> {
        let evidence = self.system_info.observe().await.map_err(|err| {
            SettingsFault::Fdo(fdo::Error::Failed(format!(
                "observe system information: {err:#}"
            )))
        })?;
        let deployment = self.deployment_evidence().await;
        Ok(system_info::info_json(
            &evidence,
            deployment.as_ref(),
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
    /// Used by the boot health gate (`mica-health`) to surface non-fatal
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

    /// Install a signed deployment already verified in the acquisition workspace.
    async fn install_update(
        &self,
        #[zbus(header)] header: Header<'_>,
        deployment_id: &str,
    ) -> fdo::Result<()> {
        if !deployment::valid_id(deployment_id) {
            return Err(fdo::Error::InvalidArgs("invalid deployment ID".into()));
        }
        let path = self
            .update
            .verified_dir()
            .join(format!("{deployment_id}.json"));
        self.request_install(sender_of(&header), &path.to_string_lossy())
            .await
    }

    /// Refresh native boot, deployment and rollback evidence, retaining the
    /// installation, action and acquisition lifecycle records.
    async fn get_update_state(&self) -> fdo::Result<String> {
        self.refresh_update_state().await
    }

    async fn confirm_deployment(
        &self,
        #[zbus(header)] header: Header<'_>,
        deployment_id: &str,
    ) -> fdo::Result<()> {
        self.request_deployment_action(sender_of(&header), "confirm", deployment_id)
            .await
    }

    async fn reject_deployment(
        &self,
        #[zbus(header)] header: Header<'_>,
        deployment_id: &str,
    ) -> fdo::Result<()> {
        self.request_deployment_action(sender_of(&header), "reject", deployment_id)
            .await
    }

    async fn rollback_deployment(
        &self,
        #[zbus(header)] header: Header<'_>,
        deployment_id: &str,
    ) -> fdo::Result<()> {
        self.request_deployment_action(sender_of(&header), "rollback", deployment_id)
            .await
    }

    /// Run an update metadata check (`mica-deploy sync` + `check`) on a
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

    /// Download the selected descriptor (`mica-deploy fetch`) on a background
    /// task; on success the verified descriptor path is recorded and the
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

    /// Write the operator's update configuration document; answers the
    /// document as saved, as JSON.
    ///
    /// Exported as `SetUpdateConfig`, and it is the **only** writer of
    /// `/mos/config/updates.json` (PLAN-071 §3, PLAN-070 §5.2.7). apid holds
    /// no path to that file and asks here instead, so one fact has one writer
    /// all the way down to the filesystem.
    ///
    /// The argument is a **patch**: the keys the operator changed, merged over
    /// what is on the disk. Absent leaves a key alone, `null` clears an
    /// override back to the baked default, a value overrides. A patch naming
    /// a key this document does not have, or a trust anchor at any depth, or
    /// one that would produce a document the reader refuses — `auto` with no
    /// maintenance window, a window that is not `HH:MM` — is `InvalidArgs`
    /// with the offending field named, and the file is not touched.
    ///
    /// Audited on both sides: micad logs who wrote what, apid records the
    /// event with the actor that asked.
    async fn set_update_config(
        &self,
        #[zbus(header)] header: Header<'_>,
        patch_json: &str,
    ) -> fdo::Result<String> {
        let document = self
            .update
            .write_config(sender_of(&header), patch_json)
            .await
            .map_err(refusal_to_fdo)?;
        Ok(document.to_string())
    }

    /// Set a TRANSIENT root password, then re-apply the SSH subtree.
    ///
    /// Exported as `SetTransientRootPassword`. The password lives until the
    /// next boot, when `mica-shadow-reconcile` clears the root hash it wrote;
    /// persistent access is by SSH public key.
    ///
    /// Deliberately not a setting. Nothing is written into the settings tree
    /// and no [`SettingsChanged`](Self::settings_changed) is emitted, because a
    /// password that reached the settings tree would be persisted, re-applied
    /// on the next boot and readable by anything that can call `GetSettings` —
    /// which is the opposite of transient in all three respects.
    ///
    /// The reconcilers are re-run afterwards so dropbear's arguments re-render
    /// against a device that now has a password to offer, and dropbear is
    /// restarted onto them.
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
            Some(cfg) if cfg.kind == micad_settings::IfaceKind::Wireguard => {}
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

/// The four service routes the automatic driver reaches, and the ONE hop a
/// test cannot build.
///
/// `main.rs` moves the [`MosdService`] into the object server and hands the
/// driver an [`InterfaceRef`] to it, so the install and the reboot automation
/// calls are the object server's own — the very instance `InstallUpdate` and
/// `Reboot` land on, which is what makes "the automatic path meets the gates
/// the manual path meets" true by construction rather than by review. An
/// `InterfaceRef` needs a live connection, so that one indirection is also
/// the only part of the wiring a unit test cannot stand up.
///
/// Naming those four calls here keeps the delegation a single piece of code:
/// [`BusRoutes`]'s [`AutoRoutes`] implementation below is what production
/// runs AND what `tests::the_automatic_and_manual_paths_meet_the_same_gate_set`
/// drives, and all a test substitutes is the `self.get().await` hop. A second
/// implementation written for the test would be a test of itself.
#[async_trait::async_trait]
pub trait ServedDaemon: Send + Sync {
    /// `GetUpdateState`'s own document, read exactly as an operator polling
    /// the API reads it.
    async fn update_state(&self) -> fdo::Result<String>;

    /// The `InstallUpdate` route, with every gate it answers an operator with.
    async fn install(&self, sender: &str, descriptor: &str) -> fdo::Result<()>;

    /// The `Reboot` route, honouring the safe-to-reboot gate.
    async fn reboot(&self, sender: &str) -> fdo::Result<()>;

    /// PLAN-071 §7's clock-trust evidence.
    async fn clock_trust(&self) -> ClockTrust;
}

#[async_trait::async_trait]
impl ServedDaemon for InterfaceRef<MosdService> {
    async fn update_state(&self) -> fdo::Result<String> {
        self.get().await.refresh_update_state().await
    }

    async fn install(&self, sender: &str, descriptor: &str) -> fdo::Result<()> {
        self.get().await.request_install(sender, descriptor).await
    }

    async fn reboot(&self, sender: &str) -> fdo::Result<()> {
        self.get().await.request_reboot(sender).await
    }

    async fn clock_trust(&self) -> ClockTrust {
        self.get().await.clock_trust().await
    }
}

/// The automatic driver's window onto the daemon.
///
/// Assembled here because its two halves live in different places: the check
/// and the fetch are the lifecycle's, which the manual `CheckUpdate` and
/// `FetchUpdate` routes also call, and the install and the reboot are the
/// service's, which the object server owns once the connection is built —
/// reached the way [`crate::scan`] reaches it, through the served interface.
///
/// What is deliberately NOT here is `SetRebootOverride`. The automatic path
/// holds this object and nothing else, so it cannot arm the reboot-gate
/// override; see [`AutoRoutes`].
pub struct BusRoutes<S> {
    lifecycle: Arc<UpdateLifecycle>,
    service: S,
}

impl<S: ServedDaemon> BusRoutes<S> {
    pub fn new(lifecycle: Arc<UpdateLifecycle>, service: S) -> Self {
        Self { lifecycle, service }
    }

    /// `GetUpdateState`, parsed. A query that did not answer is `None`, which
    /// the driver reads as "ask again next tick" and never as "nothing is
    /// pending".
    async fn update_state(&self) -> Option<Value> {
        let rendered = self
            .service
            .update_state()
            .await
            .map_err(|err| {
                tracing::debug!(error = %err, "automatic driver could not read the update state");
            })
            .ok()?;
        serde_json::from_str(&rendered).ok()
    }
}

#[async_trait::async_trait]
impl<S: ServedDaemon + 'static> AutoRoutes for BusRoutes<S> {
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
        self.lifecycle.staged_descriptor().await
    }

    async fn discard_staged(&self, why: &str) {
        self.lifecycle.discard_descriptor(why).await;
    }

    async fn facts(&self) -> Option<UpdateFacts> {
        let entry = self.update_state().await?;
        Some(UpdateFacts {
            reboot_pending: entry
                .pointer("/state/candidate")
                .is_some_and(Value::is_string)
                || entry.pointer("/lifecycle/state").and_then(Value::as_str)
                    == Some("reboot-required"),
            install_status: entry
                .pointer("/install/status")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    async fn install(&self, sender: &str, descriptor: &str) -> Result<(), String> {
        self.service
            .install(sender, descriptor)
            .await
            .map_err(|err| err.to_string())
    }

    async fn reboot(&self, sender: &str) -> Result<(), String> {
        self.service
            .reboot(sender)
            .await
            .map_err(|err| err.to_string())
    }

    async fn clock(&self) -> ClockTrust {
        self.service.clock_trust().await
    }

    async fn audit(&self, event: &str) {
        record_policy_action(&audit_ring_dir(), event);
    }

    async fn defer(&self, reason: &str, detail: &str) {
        self.lifecycle.defer(reason, detail).await;
    }

    async fn resume(&self, only: Option<&str>) {
        self.lifecycle.resume(only).await;
    }
}

/// One audit line for an action the update policy took on its own (U8).
///
/// The device's ONE ring, the same file apid appends an operator's actions to
/// and the same writer — `append_audit_line`'s single `O_APPEND` write is what
/// makes two processes on one file safe. The source is the daemon rather than
/// a peer address, because no peer asked: [`update_auto::SENDER`] is already
/// the name the lifecycle records this driver's requests under, so the audit
/// trail and `update.lifecycle` name the same actor with the same word.
///
/// A failed write is logged and swallowed, as it is on both other sides:
/// refusing to update a device because its audit ring is unwritable is a
/// lockdown decision this campaign does not take.
///
/// `dir` is a parameter rather than [`audit_ring_dir`] read here, because the
/// only other way to point a test at a temporary ring is the environment, and
/// setting one is `unsafe` under this workspace's `forbid(unsafe_code)`.
fn record_policy_action(dir: &std::path::Path, event: &str) {
    tracing::info!(
        target: "audit",
        event,
        outcome = REQUESTED,
        source = update_auto::SENDER,
        actor = ACTOR_POLICY,
        "audit event"
    );
    if let Err(err) = std::fs::create_dir_all(dir).and_then(|()| {
        append_audit_line(
            dir,
            &audit_line(event, REQUESTED, update_auto::SENDER, ACTOR_POLICY),
        )
    }) {
        tracing::warn!(
            error = %err,
            dir = %dir.display(),
            "the automatic update audit line could not be written"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::Value;

    use super::{
        BusRoutes, DocumentRefusal, Inner, MosdService, ServedDaemon, fdo, paths_overlap,
        record_policy_action, run_apply_worker,
    };
    use crate::deployment::{DeploymentClient, Status};
    use crate::power::MockPower;
    use crate::update_lifecycle::Refusal;

    struct MockDeployments {
        calls: CallLog,
        status: Status,
        install_error: Option<String>,
        install_gate: Option<Arc<tokio::sync::Notify>>,
        queries_fail: bool,
    }

    impl Default for MockDeployments {
        fn default() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                status: native_status(),
                install_error: None,
                install_gate: None,
                queries_fail: false,
            }
        }
    }

    fn native_status() -> Status {
        Status::parse(&crate::deployment::tests::fixture().to_string()).unwrap()
    }

    fn pending_status() -> Status {
        let mut status = native_status();
        let mut candidate = status.deployments[0].clone();
        candidate.id = "e".repeat(64);
        candidate.generation = 3;
        candidate.tries_left = Some(3);
        candidate.file = format!("mos-{}+3.conf", candidate.id);
        status.state.candidate = Some(candidate.id.clone());
        status.state.highest_generation = 3;
        status.deployments.push(candidate);
        status
    }

    #[async_trait::async_trait]
    impl DeploymentClient for MockDeployments {
        async fn status(&self) -> anyhow::Result<Status> {
            anyhow::ensure!(!self.queries_fail, "native backend unreachable");
            Ok(self.status.clone())
        }
        async fn install(&self, path: &std::path::Path) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("install {}", path.display()));
            if let Some(gate) = &self.install_gate {
                gate.notified().await;
            }
            if let Some(error) = &self.install_error {
                anyhow::bail!("{error}");
            }
            Ok(())
        }
        async fn confirm(&self) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push("confirm".into());
            Ok(())
        }
        async fn reject(&self, id: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("reject {id}"));
            Ok(())
        }
        async fn rollback(&self) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push("rollback".into());
            Ok(())
        }
    }

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
            _settings: &micad_settings::Settings,
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
            _settings: &micad_settings::Settings,
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

    /// A mock's shared call log ([`MockPower::calls`] / [`MockDeployments::calls`]).
    type CallLog = Arc<Mutex<Vec<String>>>;

    /// A store over a throwaway tree: the STATE document and the
    /// `/mos/config/` namespace beside it.
    ///
    /// The namespace is created, because an absent one is the DATA medium
    /// being gone and the store refuses that rather than defaulting
    /// (PLAN-070 §5.2.6).
    fn store_in(dir: &tempfile::TempDir) -> micad_settings::Store {
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).expect("create the config namespace");
        micad_settings::Store::new(dir.path().join("settings.toml"), config)
    }

    /// Service backed by a throwaway settings file, a throwaway shadow file,
    /// a recording power mock and the given the native backend mock; the power log and the
    /// the native backend call log are returned alongside.
    fn service_with_deployments(
        native: MockDeployments,
    ) -> (MosdService, CallLog, CallLog, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_in(&dir);
        let shadow_path = dir.path().join("shadow");
        std::fs::write(&shadow_path, SHADOW).expect("seed shadow");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let deployment_calls = Arc::clone(&native.calls);
        let service = MosdService::new(
            store,
            micad_settings::Settings::default(),
            Vec::new(),
            Box::new(MockPower {
                calls: Arc::clone(&calls),
            }),
            shadow_path,
            serde_json::json!({}),
        )
        .with_deployments(Arc::new(native))
        // Installs are admitted only from <workspace>/verified; the tests'
        // descriptors are placed there by `verified_descriptor`.
        .with_update_workspace(dir.path().join("updates"));
        (service, calls, deployment_calls, dir)
    }

    /// A descriptor file inside the test service's `verified/`, as a string path.
    fn verified_descriptor(dir: &tempfile::TempDir, name: &str) -> String {
        let verified = dir.path().join("updates").join("verified");
        std::fs::create_dir_all(&verified).expect("verified/");
        let descriptor = verified.join(name);
        std::fs::write(&descriptor, b"descriptor bytes").expect("seed descriptor");
        descriptor.to_str().expect("utf-8").to_string()
    }

    /// [`service_with_deployments`] over a default native backend mock.
    fn service_with_mock() -> (MosdService, Arc<Mutex<Vec<String>>>, tempfile::TempDir) {
        let (service, calls, _deployment_calls, dir) =
            service_with_deployments(MockDeployments::default());
        (service, calls, dir)
    }

    /// A service with one reconciler for each subtree that makes an accidental
    /// `apply_all` visible in the call log.
    fn service_with_recording_reconcilers() -> (MosdService, CallLog, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let shadow_path = dir.path().join("shadow");
        std::fs::write(&shadow_path, SHADOW).expect("seed shadow");
        let mut settings = micad_settings::Settings::default();
        settings.network.insert(
            "wg0".to_string(),
            micad_settings::IfaceSettings {
                kind: micad_settings::IfaceKind::Wireguard,
                wireguard: Some(micad_settings::WireguardConfig::default()),
                ..micad_settings::IfaceSettings::default()
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
            store_in(&dir),
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
            settings: micad_settings::Settings::default(),
            state: serde_json::json!({}),
        }));
        let worker = tokio::spawn(run_apply_worker(
            Arc::clone(&queue),
            Arc::clone(&inner),
            Arc::new(tokio::sync::Mutex::new(())),
            reconcilers,
            Arc::new(tokio::sync::RwLock::new(Vec::new())),
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
            store_in(&dir),
            micad_settings::Settings::default(),
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
            store_in(&dir),
            micad_settings::Settings::default(),
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
    async fn a_reboot_into_a_pending_deployment_records_the_warning_first() {
        let (service, calls, _deployment_calls, _dir) = service_with_deployments(MockDeployments {
            status: pending_status(),
            ..MockDeployments::default()
        });

        service.request_reboot(":1.4").await.expect("reboot");

        assert_eq!(*calls.lock().expect("lock"), vec!["reboot".to_string()]);
        let power = service.get_state("power").await.expect("power state");
        let power: serde_json::Value = serde_json::from_str(&power).expect("json");
        assert_eq!(power["last_action"], "reboot");
        let warning = power["update_warning"]
            .as_str()
            .expect("a pending deployment must put update_warning beside the action");
        assert!(warning.contains(&"e".repeat(64)), "warning: {warning}");
        assert!(warning.contains("awaits reboot"), "warning: {warning}");
    }

    #[tokio::test]
    async fn a_converged_system_reboots_without_an_update_warning() {
        let (service, calls, _deployment_calls, _dir) = service_with_deployments(MockDeployments {
            ..MockDeployments::default()
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
    async fn an_unreachable_native_does_not_block_the_reboot() {
        // An unavailable native backend: the deployment query fails, the reboot
        // still goes through, and no warning is invented.
        let (service, calls, _deployment_calls, _dir) = service_with_deployments(MockDeployments {
            queries_fail: true,
            ..MockDeployments::default()
        });

        service.request_reboot(":1.4").await.expect("reboot");

        assert_eq!(*calls.lock().expect("lock"), vec!["reboot".to_string()]);
        let power = service.get_state("power").await.expect("power state");
        let power: serde_json::Value = serde_json::from_str(&power).expect("json");
        assert!(power.get("update_warning").is_none(), "got: {power}");
    }

    #[tokio::test]
    async fn install_admission_rejects_paths_outside_the_verified_descriptor_namespace() {
        let (service, _, calls, dir) = service_with_deployments(MockDeployments::default());
        let good = verified_descriptor(&dir, &format!("{}.json", "a".repeat(64)));
        let verified = std::path::Path::new(&good).parent().unwrap();
        let link = verified.join(format!("{}.json", "b".repeat(64)));
        std::os::unix::fs::symlink(&good, &link).unwrap();
        let outside = dir.path().join(format!("{}.json", "c".repeat(64)));
        std::fs::write(&outside, b"unverified").unwrap();
        let missing = verified.join(format!("{}.json", "d".repeat(64)));
        let partial = verified_descriptor(&dir, "partial.json.partial");
        for path in [
            "relative.json",
            dir.path().to_str().unwrap(),
            link.to_str().unwrap(),
            outside.to_str().unwrap(),
            missing.to_str().unwrap(),
            &partial,
        ] {
            assert!(
                matches!(
                    service.request_install(":1.5", path).await,
                    Err(fdo::Error::InvalidArgs(_))
                ),
                "{path}"
            );
        }
        assert!(calls.lock().unwrap().is_empty());
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
        let (service, _calls, _deployment_calls, _dir) =
            service_with_deployments(MockDeployments::default());

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
                    root: "/".to_string(),
                    device: "/dev/mmcblk0p11".to_string(),
                    mount: "/srv".to_string(),
                    fstype: "ext4".to_string(),
                    read_only: false,
                }),
                space: Some(FsSpace {
                    total: 1_000_000_000,
                    used: 1_000_000_000 - free,
                    free,
                    reserved: 0,
                }),
                ..TierEvidence::default()
            },
        );
        StorageEvidence {
            directory_bytes: std::collections::BTreeMap::new(),
            project_quotas: None,
            tiers,
            media: Vec::new(),
            binds: std::collections::BTreeMap::new(),
        }
    }

    /// The storage surface is served from the observer, and a daemon without
    /// one says so instead of answering with an empty layout.
    #[tokio::test]
    async fn the_storage_status_is_observed_and_absent_without_an_observer() {
        let (service, _calls, _deployment_calls, _dir) =
            service_with_deployments(MockDeployments::default());
        let unobserved = service
            .get_storage_status()
            .await
            .expect_err("a daemon with no observer cannot answer");
        assert!(
            format!("{unobserved:?}").contains("storage"),
            "{unobserved:?}"
        );

        let service =
            service.with_storage_status(Arc::new(FixedStorage(data_evidence(25_000_000))));
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
    }

    #[tokio::test]
    async fn an_install_runs_in_the_background_and_records_its_lifecycle() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (service, _calls, deployment_calls, dir) = service_with_deployments(MockDeployments {
            install_gate: Some(Arc::clone(&gate)),
            ..MockDeployments::default()
        });
        let descriptor = verified_descriptor(&dir, &format!("{}.json", "a".repeat(64)));
        let descriptor = descriptor.as_str();

        // Returns while the install is still gated: the bus call cannot be
        // blocked by a slow installer.
        service
            .request_install(":1.6", descriptor)
            .await
            .expect("install");
        wait_for_install_status(&service, "running").await;

        // A second install while one runs is refused, and the refusal names
        // the reason rather than queueing silently.
        let busy = service
            .request_install(":1.7", descriptor)
            .await
            .expect_err("concurrent install must be refused");
        assert!(busy.to_string().contains("already running"), "{busy}");

        gate.notify_one();
        wait_for_install_status(&service, "done").await;

        let install = service.get_state("update.install").await.expect("state");
        let install: serde_json::Value = serde_json::from_str(&install).expect("json");
        assert_eq!(install["requested_by"], ":1.6");
        assert_eq!(install["deploymentId"], "a".repeat(64));
        assert_eq!(
            *deployment_calls.lock().expect("lock"),
            vec![format!("install {descriptor}")],
            "exactly the admitted install reached the installer"
        );
        // The completed install refreshed the whole update entry.
        let update = service.get_state("update").await.expect("state");
        let update: serde_json::Value = serde_json::from_str(&update).expect("json");
        assert_eq!(update["state"]["current"], "a".repeat(64));

        // The in-flight flag is released: a new install is admitted again.
        gate.notify_one();
        service
            .request_install(":1.8", descriptor)
            .await
            .expect("install");
        wait_for_install_status(&service, "done").await;
    }

    #[tokio::test]
    async fn a_failed_install_records_the_error_and_releases_the_flag() {
        let (service, _calls, _deployment_calls, dir) = service_with_deployments(MockDeployments {
            install_error: Some("signature verification failed".to_string()),
            ..MockDeployments::default()
        });
        let descriptor = verified_descriptor(&dir, &format!("{}.json", "a".repeat(64)));
        let descriptor = descriptor.as_str();

        service
            .request_install(":1.9", descriptor)
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
        // PLAN-076 B4, driven through the real install path rather than
        // asserted at the classifier: the native backend's sentence stays in `error` and its
        // class is beside it, so a fleet groups signature refusals without
        // matching on the native backend's words.
        assert_eq!(
            install["error_code"],
            crate::update_codes::CLIENT_EXIT_FAILURE,
            "got {install}"
        );
        service
            .request_install(":1.9", descriptor)
            .await
            .expect("flag released");
    }

    // The same path with a failure this repository has NOT measured: the code
    // is `unknown` and it is NOT the sentence. This is the half of the gate
    // that a mapping with a pass-through fallback would silently fail —
    // there, `error_code` would read `Compatible mismatch: …` and a consumer
    // matching on codes would be back to matching on text without being told.

    #[tokio::test]
    async fn an_install_mid_flight_refuses_a_reboot_until_it_finishes() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (service, power_calls, _deployment_calls, dir) =
            service_with_deployments(MockDeployments {
                install_gate: Some(Arc::clone(&gate)),
                ..MockDeployments::default()
            });
        let descriptor = std::path::PathBuf::from(verified_descriptor(
            &dir,
            &format!("{}.json", "a".repeat(64)),
        ));
        service
            .request_install(":1.6", descriptor.to_str().expect("utf-8"))
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
        // `degraded` — mica-health's disk-pressure report — must NOT block.
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
        let (service, _calls, deployment_calls, dir) =
            service_with_deployments(MockDeployments::default());
        let policy_path = dir.path().join("updates.json");
        std::fs::write(&policy_path, "{not json").expect("seed policy");
        let service = service.with_update(
            Arc::new(crate::update_lifecycle::NoClient),
            crate::update_policy::PolicyStore::at(policy_path),
        );
        let descriptor = std::path::PathBuf::from(verified_descriptor(
            &dir,
            &format!("{}.json", "a".repeat(64)),
        ));

        let refused = service
            .request_install(":1.5", descriptor.to_str().expect("utf-8"))
            .await
            .expect_err("an unreadable policy file must fail closed");
        assert!(refused.to_string().contains("invalid"), "{refused}");
        assert!(
            deployment_calls.lock().expect("lock").is_empty(),
            "the refused install must not reach the installer"
        );
    }

    /// The write route's whole point: micad is the file's only writer, and a
    /// patch changes what it names and nothing else.
    #[tokio::test]
    async fn the_write_route_merges_a_patch_and_the_next_state_read_reports_it() {
        let (service, _calls, _deployment_calls, dir) =
            service_with_deployments(MockDeployments::default());
        let policy_path = dir.path().join("updates.json");
        std::fs::write(
            &policy_path,
            r#"{ "policy": "check", "source": { "channel": "beta" } }"#,
        )
        .expect("seed policy");
        let service = service.with_update(
            Arc::new(crate::update_lifecycle::NoClient),
            crate::update_policy::PolicyStore::at(policy_path.clone()),
        );

        let saved = service
            .update_handle()
            .write_config(":1.5", r#"{ "source": { "channel": "stable" } }"#)
            .await
            .expect("a well-formed patch is written");

        assert_eq!(saved["source"]["channel"], "stable");
        assert_eq!(saved["policy"], "check", "an unnamed key is left alone");
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&policy_path).unwrap()).unwrap();
        assert_eq!(on_disk["source"]["channel"], "stable");
        // The snapshot is retaken on the write, so an operator polling the
        // state reads what they just set rather than what the last action saw.
        let recorded = service.trees().await.1;
        assert_eq!(
            recorded["update"]["lifecycle"]["policy"]["channel"],
            "stable"
        );
    }

    /// §2's rule fires at the API, which is the difference between telling an
    /// operator now and a device failing closed some hours later.
    #[tokio::test]
    async fn the_write_route_refuses_auto_without_a_window_and_replaces_nothing() {
        let (service, _calls, _deployment_calls, dir) =
            service_with_deployments(MockDeployments::default());
        let policy_path = dir.path().join("updates.json");
        std::fs::write(&policy_path, r#"{ "policy": "check" }"#).expect("seed policy");
        let before = std::fs::read_to_string(&policy_path).unwrap();
        let service = service.with_update(
            Arc::new(crate::update_lifecycle::NoClient),
            crate::update_policy::PolicyStore::at(policy_path.clone()),
        );

        let refused = service
            .update_handle()
            .write_config(":1.5", r#"{ "policy": "auto" }"#)
            .await
            .expect_err("`auto` with no window is refused on write");

        assert!(
            matches!(refused, Refusal::Invalid(_)),
            "an InvalidArgs, which apid answers 422: {}",
            refused.message()
        );
        assert!(
            refused.message().contains("maintenance window"),
            "the refusal names the rule: {}",
            refused.message()
        );
        assert_eq!(std::fs::read_to_string(&policy_path).unwrap(), before);
    }

    /// The address is the operator's; what the device will accept is not.
    #[tokio::test]
    async fn the_write_route_refuses_a_trust_anchor_by_name() {
        let (service, _calls, _deployment_calls, dir) =
            service_with_deployments(MockDeployments::default());
        let policy_path = dir.path().join("updates.json");
        let service = service.with_update(
            Arc::new(crate::update_lifecycle::NoClient),
            crate::update_policy::PolicyStore::at(policy_path.clone()),
        );

        let refused = service
            .update_handle()
            .write_config(
                ":1.5",
                r#"{ "source": { "url": "https://x/", "keyring": "/k" } }"#,
            )
            .await
            .expect_err("an anchor-shaped key is not writable");

        assert!(
            matches!(refused, Refusal::Invalid(_)),
            "{}",
            refused.message()
        );
        assert!(
            refused.message().contains("keyring"),
            "the refusal names the key: {}",
            refused.message()
        );
        assert!(!policy_path.exists(), "nothing was written");
    }

    /// A document that does not load is refused the way every other action on
    /// it is refused — 409, not 422 — because it is the file that is wrong.
    #[tokio::test]
    async fn the_write_route_refuses_to_patch_over_a_document_that_does_not_load() {
        let (service, _calls, _deployment_calls, dir) =
            service_with_deployments(MockDeployments::default());
        let policy_path = dir.path().join("updates.json");
        std::fs::write(&policy_path, "{not json").expect("seed policy");
        let service = service.with_update(
            Arc::new(crate::update_lifecycle::NoClient),
            crate::update_policy::PolicyStore::at(policy_path.clone()),
        );

        let refused = service
            .update_handle()
            .write_config(":1.5", r#"{ "policy": "off" }"#)
            .await
            .expect_err("there is no base to merge over");

        assert!(
            matches!(refused, Refusal::Policy(_)),
            "{}",
            refused.message()
        );
        assert_eq!(std::fs::read_to_string(&policy_path).unwrap(), "{not json");
    }

    /// U8: an action the policy took names `policy`, in the same ring and
    /// under the same event names an operator's action lands in.
    #[test]
    fn an_automatic_action_is_audited_under_the_policy_actor() {
        let dir = tempfile::tempdir().expect("tempdir");
        record_policy_action(dir.path(), micad_settings::UPDATE_CHECK_EVENT);

        let contents =
            std::fs::read_to_string(dir.path().join(micad_settings::AUDIT_LOG)).expect("a line");
        let line: Value = serde_json::from_str(contents.trim()).expect("JSONL");
        assert_eq!(line["event"], "update-check");
        assert_eq!(line["outcome"], "requested");
        assert_eq!(line["actor"], "policy");
        assert_eq!(
            line["source"], "auto-update",
            "the name the lifecycle already records this driver's requests under"
        );
    }

    #[tokio::test]
    async fn native_candidate_state_and_lifecycle_are_refreshed_together() {
        let (service, _, _, _) = service_with_deployments(MockDeployments {
            status: pending_status(),
            ..MockDeployments::default()
        });
        let value: Value =
            serde_json::from_str(&service.refresh_update_state().await.unwrap()).unwrap();
        assert_eq!(value["lifecycle"]["state"], "reboot-required");
        assert_eq!(value["state"]["candidate"], "e".repeat(64));
        assert_eq!(value["rollback"]["reason"], "candidate_pending");
        assert!(value.get("slots").is_none());
    }

    #[tokio::test]
    async fn update_state_is_queried_recorded_and_returned() {
        let (service, _, _, _) = service_with_deployments(MockDeployments::default());
        let value: Value =
            serde_json::from_str(&service.refresh_update_state().await.unwrap()).unwrap();
        assert_eq!(value["state"]["current"], "a".repeat(64));
        assert_eq!(value["state"]["fallback"], "b".repeat(64));
        assert_eq!(value["state"]["highestGeneration"], 2);
        assert_eq!(value["boot"]["kernelId"], "c".repeat(64));
        assert_eq!(value["rollback"]["permitted"], true);
        assert_eq!(value["lifecycle"]["state"], "succeeded");
        let recorded: Value =
            serde_json::from_str(&service.get_state("update").await.unwrap()).unwrap();
        assert_eq!(recorded, value);
    }

    #[tokio::test]
    async fn an_unreachable_native_fails_the_query_and_records_nothing() {
        let (service, _calls, _deployment_calls, _dir) =
            service_with_deployments(MockDeployments {
                queries_fail: true,
                ..MockDeployments::default()
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
    async fn native_actions_validate_identity_and_record_the_operator() {
        let (service, _, calls, _) = service_with_deployments(MockDeployments::default());
        for (action, id) in [
            ("confirm", "a".repeat(63)),
            ("activate", "a".repeat(64)),
            ("confirm", "b".repeat(64)),
        ] {
            assert!(
                service
                    .request_deployment_action(":1.2", action, &id)
                    .await
                    .is_err()
            );
        }
        assert!(calls.lock().unwrap().is_empty());
        service
            .request_deployment_action(":1.2", "confirm", &"a".repeat(64))
            .await
            .unwrap();
        service
            .request_deployment_action(":1.3", "reject", &"e".repeat(64))
            .await
            .unwrap();
        service
            .request_deployment_action(":1.4", "rollback", &"a".repeat(64))
            .await
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "confirm".to_owned(),
                format!("reject {}", "e".repeat(64)),
                "rollback".into()
            ]
        );
        let status: Value =
            serde_json::from_str(&service.get_state("update").await.unwrap()).unwrap();
        assert_eq!(status["last_action"]["requested_by"], ":1.4");
        assert_eq!(status["last_action"]["action"], "rollback");
    }

    /// A service whose settings declare one WireGuard tunnel, with a rotation
    /// writing into the same throwaway directory.
    ///
    /// No reconcilers: this exercises the bus method's own contract, and the
    /// reconcile it triggers is the network reconciler's own tests' subject.
    fn service_with_wireguard() -> (MosdService, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut settings = micad_settings::Settings::default();
        settings.network.insert(
            "wg0".to_string(),
            micad_settings::IfaceSettings {
                kind: micad_settings::IfaceKind::Wireguard,
                wireguard: Some(micad_settings::WireguardConfig::default()),
                ..micad_settings::IfaceSettings::default()
            },
        );
        settings.network.insert(
            "eth0".to_string(),
            micad_settings::IfaceSettings {
                dhcp: true,
                ..micad_settings::IfaceSettings::default()
            },
        );
        let service = MosdService::new(
            store_in(&dir),
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
        let mut settings = micad_settings::Settings::default();
        settings.network.insert(
            "wg0".to_string(),
            micad_settings::IfaceSettings {
                kind: micad_settings::IfaceKind::Wireguard,
                wireguard: Some(micad_settings::WireguardConfig::default()),
                ..micad_settings::IfaceSettings::default()
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
        use micad_settings::SettingsError;
        use zbus::DBusError as _;

        for (err, name) in [
            (
                SettingsError::NotFound("a.path".into()),
                "com.mica.micad1.Error.NotFound",
            ),
            (
                SettingsError::ReadOnly("a.path".into()),
                "com.mica.micad1.Error.ReadOnly",
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
                SettingsError::SchemaVersion("stuck".into()),
                "org.freedesktop.DBus.Error.Failed",
            ),
            // PLAN-070 §5.2.6's variant. An IO name and not `InvalidArgs`:
            // the caller asked for something reasonable and the device cannot
            // reach the store. It was added with the medium check and left out
            // of this table, which is the one place that says what a variant
            // travels as.
            (
                SettingsError::Unavailable {
                    directory: "/mos/config".into(),
                    mount: "/mos".into(),
                },
                "org.freedesktop.DBus.Error.IOError",
            ),
        ] {
            let message = err.to_string();
            let fault = super::to_bus_error(err);
            assert_eq!(fault.name().as_str(), name);
            assert_eq!(
                fault.description(),
                Some(message.as_str()),
                "the description must stay micad's own words ({name})"
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
    /// one it carries the running deployment read from the native backend client the service
    /// already holds — one client, not a second reader of the installer.
    #[tokio::test]
    async fn the_system_info_is_observed_with_the_running_deployment_and_absent_without_an_observer()
     {
        let (service, _calls, _deployment_calls, _dir) =
            service_with_deployments(MockDeployments {
                ..MockDeployments::default()
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
        assert_eq!(info["deployment"]["available"], true);
        assert_eq!(info["deployment"]["id"], "a".repeat(64));
        assert_eq!(info["deployment"]["kernelId"], "c".repeat(64));
        assert_eq!(info["daemon"]["name"], "micad");
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

    // --- F6g: the pour (PLAN-070 §5.2.7) -----------------------------------

    /// A service with one recording reconciler per document-backed subtree,
    /// over a store whose `/mos/config/` namespace is on disk and writable.
    ///
    /// The wifi pair is spelled out because it is the case that makes the
    /// refusal a *document* rule rather than a reconciler one: `wifi.json`
    /// carries `wifi` and backs two reconcilers, one of which declares
    /// `wifi.client`, so a refusal that matched reconciler subtrees by equality
    /// would leave the client half running on a schema default.
    /// Hand-write one `/mos/config/` document, creating the namespace the way
    /// `mica-data-layout` does before an integrator ever sees the partition.
    fn pour(dir: &tempfile::TempDir, document: &str, text: &str) -> std::path::PathBuf {
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).expect("create the config namespace");
        let path = config.join(document);
        std::fs::write(&path, text).expect("pour the document");
        path
    }

    async fn service_over(dir: &tempfile::TempDir) -> (MosdService, CallLog, Vec<DocumentRefusal>) {
        let shadow_path = dir.path().join("shadow");
        std::fs::write(&shadow_path, SHADOW).expect("seed shadow");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reconciler = |name, subtree| {
            Box::new(RecordingReconciler {
                name,
                subtree,
                calls: Arc::clone(&calls),
            }) as Box<dyn crate::reconciler::Reconciler>
        };
        let store = store_in(dir);
        // `load_with_refusals` and not `load`, which is the daemon's own choice
        // at `main.rs` and the whole subject here: `load` refuses the load, and
        // a test harness that used it could not reach a device that came up at
        // all with a broken document on it.
        let loaded = store
            .load_with_refusals()
            .expect("load the poured namespace");
        let refusals = loaded.refusals.clone();
        let service = MosdService::new(
            store,
            loaded.settings,
            vec![
                reconciler("hostname", "hostname"),
                reconciler("network", "network"),
                reconciler("wifiAp", "wifi"),
                reconciler("wifiClient", "wifi.client"),
                reconciler("sshd", "access.ssh"),
            ],
            Box::new(MockPower {
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
            shadow_path,
            serde_json::json!({}),
        );
        service.set_config_refusals(loaded.refusals).await;
        (service, calls, refusals)
    }

    /// **Clause 2 of the F6g gate, both directions, at the reconcile loop.**
    ///
    /// A poured `wifi.json` that does not parse refuses the two reconcilers it
    /// backs — recorded where the applied state would go, naming the file — and
    /// the three it does not back run exactly as they would have. The negative
    /// half is the one the whole slice exists for: before this, a mistyped
    /// `wifi.json` stopped micad, which took the network reconciler with it and
    /// put the device off the air for a mistake in an unrelated subsystem.
    #[tokio::test]
    async fn a_refused_document_skips_its_reconcilers_and_leaves_the_others_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        pour(
            &dir,
            micad_settings::WIFI_DOCUMENT,
            r#"{"schema_version": 1, "wifi": {"ap": {"mode": "alwys"}}}"#,
        );
        let (service, calls, refusals) = service_over(&dir).await;
        assert_eq!(refusals.len(), 1);
        let refusal = refusals[0].clone();
        service.apply_all().await;

        let mut ran = calls.lock().expect("call log").clone();
        ran.sort();
        assert_eq!(
            ran,
            vec![
                "hostname".to_string(),
                "network".to_string(),
                "sshd".to_string()
            ],
            "a refused wifi.json must cost the wifi reconcilers and nothing else"
        );

        let (_, state) = service.trees().await;
        for name in ["wifiAp", "wifiClient"] {
            let message = state[name]["refused"].as_str().unwrap_or_default();
            assert!(
                message.contains(&refusal.path.display().to_string()),
                "{name} must record a refusal naming the file, got {:?}",
                state[name]
            );
            assert!(
                state[name].get("applied").is_none(),
                "{name} must not report an applied state"
            );
        }
        assert_eq!(state["network"]["applied"], serde_json::json!(true));
        // The list form, for an operator who does not know which reconciler to
        // look under.
        assert_eq!(
            state["configuration"]["refused"][0]["document"],
            serde_json::json!(micad_settings::WIFI_DOCUMENT)
        );
    }

    /// **The write half.** An unrelated write must not overwrite the refused
    /// document, and a write that reaches it must repair it.
    ///
    /// Both halves in one test because they are one rule with a sign: `save`
    /// writes every document out of an addressed tree in which the refused
    /// subtree sits at its schema default, so preserving nothing loses the
    /// integrator's file on the next hostname change — and preserving
    /// everything leaves the serial console as the only way out of a typo,
    /// against §5.2.7's *a human edits it through an authenticated API*.
    #[tokio::test]
    async fn an_unrelated_write_preserves_a_refused_document_and_a_reaching_one_repairs_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let poured = r#"{"schema_version": 1, "wifi": {"ap": {"mode": "alwys"}}}"#;
        let document = pour(&dir, micad_settings::WIFI_DOCUMENT, poured);
        let (service, calls, refusals) = service_over(&dir).await;
        assert_eq!(refusals.len(), 1);

        service
            .write_setting("hostname", serde_json::json!("edge-1"))
            .await
            .expect("write the hostname");
        assert_eq!(
            std::fs::read_to_string(&document).expect("read back"),
            poured,
            "an unrelated write must leave the refused document byte identical"
        );
        let (_, state) = service.trees().await;
        assert_eq!(
            state["configuration"]["refused"]
                .as_array()
                .map(Vec::len)
                .unwrap_or_default(),
            1,
            "the refusal stands until the document is repaired"
        );

        calls.lock().expect("call log").clear();
        service
            .write_setting("wifi.ap.ssid", serde_json::json!("repaired"))
            .await
            .expect("repair the document");

        let repaired: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&document).expect("read back"))
                .expect("the repaired document parses");
        assert_eq!(
            repaired["wifi"]["ap"]["ssid"],
            serde_json::json!("repaired")
        );
        let (_, state) = service.trees().await;
        assert!(
            state["configuration"]["refused"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "the repair clears the refusal: {:?}",
            state["configuration"]
        );
        // The write's own scope is `wifi.ap.ssid`, which the existing dot-path
        // scoping narrows to the AP reconciler; what matters is that it ran at
        // all, because a standing refusal would have skipped it.
        assert_eq!(*calls.lock().expect("call log"), vec!["wifiAp".to_string()]);
        // And the full reconcile that follows reaches both halves of the
        // document, which is the refusal being over rather than merely narrowed.
        calls.lock().expect("call log").clear();
        service.apply_all().await;
        let mut ran = calls.lock().expect("call log").clone();
        ran.sort();
        assert_eq!(
            ran,
            vec![
                "hostname".to_string(),
                "network".to_string(),
                "sshd".to_string(),
                "wifiAp".to_string(),
                "wifiClient".to_string()
            ],
            "after the repair every reconciler runs again"
        );
    }

    /// Every shipped reconciler is backed by exactly one `/mos/config/`
    /// document.
    ///
    /// The refusal is computed from [`micad_settings::DOCUMENT_SUBTREES`], so a
    /// reconciler no document claims can never be refused — it would keep
    /// running on a schema default while the document that configures it was
    /// unreadable, which is the failure open this slice exists to close. Two
    /// documents claiming one reconciler is the other direction of the same
    /// hole: whichever refusal is found first decides, and the second is
    /// silently ignored.
    #[test]
    fn every_reconciler_is_backed_by_exactly_one_config_document() {
        for reconciler in crate::reconciler::all() {
            let backing: Vec<&str> = micad_settings::DOCUMENT_SUBTREES
                .iter()
                .filter(|(_, subtrees)| {
                    subtrees
                        .iter()
                        .any(|subtree| paths_overlap(subtree, reconciler.subtree()))
                })
                .map(|(document, _)| *document)
                .collect();
            assert_eq!(
                backing.len(),
                1,
                "{} ({}) is backed by {backing:?}",
                reconciler.name(),
                reconciler.subtree()
            );
        }
    }

    // ---- PLAN-071 U2 and U3 -------------------------------------------
    //
    // What is under test here is the wiring, not a model of it: the real
    // `UpdateLifecycle` the manual `CheckUpdate`/`FetchUpdate` routes call,
    // the real `MosdService` the manual `InstallUpdate`/`Reboot` routes land
    // on, and the real `BusRoutes` — the production `AutoRoutes`
    // implementation — over both. The only substitution is
    // [`ServedDaemon`]'s `self.get().await` hop, which needs a live bus
    // connection and is one line per route.

    /// The served-object hop, and nothing else. See [`ServedDaemon`].
    struct TestServed(Arc<MosdService>);

    #[async_trait::async_trait]
    impl ServedDaemon for TestServed {
        async fn update_state(&self) -> fdo::Result<String> {
            self.0.refresh_update_state().await
        }

        async fn install(&self, sender: &str, descriptor: &str) -> fdo::Result<()> {
            self.0.request_install(sender, descriptor).await
        }

        async fn reboot(&self, sender: &str) -> fdo::Result<()> {
            self.0.request_reboot(sender).await
        }

        async fn clock_trust(&self) -> crate::time_status::ClockTrust {
            self.0.clock_trust().await
        }
    }

    /// An update client scripted by subcommand, answering in `mica-deploy`'s
    /// printed contract.
    ///
    /// By subcommand rather than in a fixed order because the driver decides
    /// how many checks a pass makes — a cadence check and a pre-install
    /// re-check are both checks — and a script that fixed the count would be
    /// asserting the shape of the pass rather than letting the pass have one.
    /// The output is the contract's, so the lifecycle's real `parse_probe`,
    /// `parse_check` and `parse_fetch` run.
    struct ScriptedClient {
        staged: String,
        selected: String,
        calls: CallLog,
    }

    #[async_trait::async_trait]
    impl crate::update_lifecycle::UpdateClient for ScriptedClient {
        fn unavailable(&self) -> Option<String> {
            None
        }

        async fn run(
            &self,
            args: &[String],
            _timeout: Duration,
        ) -> anyhow::Result<crate::update_lifecycle::ClientOutput> {
            let verb = args
                .get(if args.first().is_some_and(|arg| arg == "--max-bytes") {
                    2
                } else {
                    0
                })
                .cloned()
                .unwrap_or_default();
            self.calls.lock().expect("client calls").push(verb.clone());
            let id = self.selected.strip_suffix(".json").unwrap();
            let stdout = match verb.as_str() {
                "probe" => serde_json::json!({"status":"ready","freeBytes":1_000_000_000u64}).to_string(),
                "check" => serde_json::json!({"revision":1,"channel":"stable","selected":{"deploymentId":id,"deployment":{"version":"1.5.0"}}}).to_string(),
                "fetch" => serde_json::json!({"id":id,"path":self.staged,"objects":std::path::Path::new(&self.staged).parent().unwrap().join("objects"),"version":"1.5.0","generation":3}).to_string(),
                "status" => crate::deployment::tests::fixture().to_string(),
                other => anyhow::bail!("unexpected native command: {other}"),
            };
            Ok(crate::update_lifecycle::ClientOutput {
                code: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }

    /// One device, wired the way `main.rs` wires it.
    struct Device {
        dir: tempfile::TempDir,
        service: Arc<MosdService>,
        lifecycle: Arc<crate::update_lifecycle::UpdateLifecycle>,
        routes: Arc<BusRoutes<TestServed>>,
        power: CallLog,
        native: CallLog,
        descriptor: String,
    }

    const STAGED_BUNDLE: &str =
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.json";

    impl Device {
        /// A device carrying `document`, a believed clock and the native backend mock
        /// given.
        fn with_deployments(document: &str, native: MockDeployments) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let shadow_path = dir.path().join("shadow");
            std::fs::write(&shadow_path, SHADOW).expect("seed shadow");
            let policy_path = dir.path().join("updates.json");
            std::fs::write(&policy_path, document).expect("seed the policy document");
            let descriptor = verified_descriptor(&dir, STAGED_BUNDLE);
            let power = Arc::new(Mutex::new(Vec::new()));
            let deployment_calls = Arc::clone(&native.calls);
            let service = MosdService::new(
                store_in(&dir),
                micad_settings::Settings::default(),
                Vec::new(),
                Box::new(MockPower {
                    calls: Arc::clone(&power),
                }),
                shadow_path,
                serde_json::json!({}),
            )
            .with_update_workspace(dir.path().join("updates"))
            .with_update(
                Arc::new(ScriptedClient {
                    staged: descriptor.clone(),
                    selected: STAGED_BUNDLE.to_string(),
                    calls: Arc::new(Mutex::new(Vec::new())),
                }),
                crate::update_policy::PolicyStore::at(policy_path),
            )
            .with_deployments(Arc::new(native))
            // PLAN-071 §7's predicate is asserted on its own in
            // `update_auto`; here it must simply hold, or every install
            // below would defer on the clock before reaching its gate.
            .with_time_status(Arc::new(FixedTimesync(synchronized_evidence())));
            let service = Arc::new(service);
            let lifecycle = service.update_handle();
            let routes = Arc::new(BusRoutes::new(
                Arc::clone(&lifecycle),
                TestServed(Arc::clone(&service)),
            ));
            Self {
                dir,
                service,
                lifecycle,
                routes,
                power,
                native: deployment_calls,
                descriptor,
            }
        }

        fn new(document: &str) -> Self {
            Self::with_deployments(document, MockDeployments::default())
        }

        fn rewrite(&self, document: &str) {
            std::fs::write(self.dir.path().join("updates.json"), document)
                .expect("rewrite the policy document");
        }

        /// A driver over this device's real routes, with a cadence a test
        /// moves by hand.
        fn driver(
            &self,
        ) -> (
            crate::update_auto::AutoDriver,
            Arc<crate::update_auto::TestCadence>,
        ) {
            let cadence = crate::update_auto::TestCadence::new();
            let routes: Arc<dyn crate::update_auto::AutoRoutes> = self.routes.clone();
            let clock: Arc<dyn crate::update_auto::Cadence> = cadence.clone();
            (crate::update_auto::AutoDriver::new(routes, clock), cadence)
        }

        async fn update_entry(&self, path: &str) -> Value {
            let raw = self.service.get_state(path).await.unwrap_or_default();
            serde_json::from_str(&raw).unwrap_or(Value::Null)
        }

        /// The deferral the automatic path last recorded, as an operator
        /// reads it back out of `update.lifecycle`.
        async fn deferral(&self) -> Value {
            self.update_entry("update.lifecycle.deferred").await
        }

        async fn reboot_gate(&self) -> Value {
            self.update_entry("update.lifecycle.reboot_gate").await
        }

        /// Wait out a check the manual route spawned, so the next assertion
        /// is not answered by the busy guard instead of the gate it means to
        /// ask about.
        async fn settled(&self) {
            for _ in 0..200 {
                let state = self.update_entry("update.lifecycle.state").await;
                if state != Value::String("checking".to_string())
                    && state != Value::String("downloading".to_string())
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("the lifecycle never settled");
        }
    }

    /// Timesync evidence a healthy networked device settles into, so
    /// `clock_trust` believes the clock by §7's first limb.
    fn synchronized_evidence() -> crate::time_status::TimesyncEvidence {
        crate::time_status::TimesyncEvidence {
            service_reachable: true,
            ntp_synchronized: Some(true),
            server_name: Some("time.example".to_string()),
            server_address: Some("192.0.2.10".to_string()),
            sample: Some(crate::time_status::NtpSample {
                leap: 0,
                stratum: 2,
                spike: false,
                offset_seconds: 0.001,
                packet_count: 8,
            }),
        }
    }

    /// An `auto` document with a maintenance window `hours` from now, open
    /// when `open`. Two hours either side, so the verdict does not depend on
    /// the minute the suite runs in.
    fn auto_document(open: bool, reboot_policy: &str) -> String {
        let face =
            |offset: chrono::Duration| (chrono::Utc::now() + offset).format("%H:%M").to_string();
        let (start, end) = if open {
            (
                face(-chrono::Duration::hours(2)),
                face(chrono::Duration::hours(2)),
            )
        } else {
            (
                face(chrono::Duration::hours(2)),
                face(chrono::Duration::hours(4)),
            )
        };
        format!(
            r#"{{"policy": "auto", "checkIntervalMinutes": 60,
                 "rebootPolicy": "{reboot_policy}",
                 "source": {{"url": "http://mirror/tuf", "channel": "stable"}},
                 "maintenance": {{"windows": [{{"start": "{start}", "end": "{end}"}}]}}}}"#
        )
    }

    /// PLAN-071 U2: **a test asserting the automatic and manual paths meet
    /// the same gate set.**
    ///
    /// The property that makes `auto` safe, and the one a unit test of either
    /// path alone cannot see. For each gate that can be closed, the operator's
    /// route and the automatic route are driven against the same daemon and
    /// must be refused by the same rule *in the same words* — the words being
    /// the point, because a second window check or a second readiness probe
    /// written for automation would answer with different ones, and an
    /// automatic path that skipped the gate would answer with none.
    ///
    /// Both sides here are production code: `request_check`/`request_fetch`
    /// and `request_install`/`request_reboot` are what the D-Bus methods call,
    /// and `BusRoutes` is what `main.rs` hands the driver.
    #[tokio::test]
    async fn the_automatic_and_manual_paths_meet_the_same_gate_set() {
        use crate::update_auto::{AutoRoutes, SENDER};

        // The check, refused by network mode. Both routes call `admit_check`.
        let device = Device::new(
            r#"{"policy": "auto", "network": {"mode": "offline"},
                "maintenance": {"windows": [{"start": "00:00", "end": "23:59"}]}}"#,
        );
        let manual = device
            .lifecycle
            .request_check(":1.7")
            .await
            .expect_err("offline refuses an operator's check");
        let automatic = device
            .routes
            .check(SENDER)
            .await
            .expect_err("and refuses the automatic one");
        assert_eq!(manual.message(), automatic.message());
        assert!(manual.message().contains("offline"), "{}", manual.message());

        // The check, refused because the document does not load. There is no
        // selection to read a channel out of, so neither path may guess one.
        let device = Device::new("{not json");
        let manual = device
            .lifecycle
            .request_check(":1.7")
            .await
            .expect_err("an unreadable document refuses an operator's check");
        let automatic = device
            .routes
            .check(SENDER)
            .await
            .expect_err("and the automatic one");
        assert_eq!(manual.message(), automatic.message());
        assert!(manual.message().contains("invalid"), "{}", manual.message());

        // The fetch, refused by metered mode. `fetch_refusal` is a superset of
        // `check_refusal`, and both routes call `admit_fetch`.
        let device = Device::new(
            r#"{"policy": "auto", "source": {"url": "http://mirror/tuf"},
                "network": {"mode": "metered", "meteredAllowsFetch": false},
                "maintenance": {"windows": [{"start": "00:00", "end": "23:59"}]}}"#,
        );
        let manual = device
            .lifecycle
            .request_fetch(":1.7")
            .await
            .expect_err("metered refuses an operator's download");
        let automatic = device
            .routes
            .fetch(SENDER)
            .await
            .expect_err("and the automatic one");
        assert_eq!(manual.message(), automatic.message());
        assert!(manual.message().contains("metered"), "{}", manual.message());
        // The same document admits the CHECK on both routes: metered gates
        // downloads, not discovery, and a gate set copied wholesale from the
        // fetch would be stricter than the one designed.
        device.routes.check(SENDER).await.expect("admitted");
        device
            .lifecycle
            .request_check(":1.7")
            .await
            .expect("admitted");
        device.settled().await;

        // The install, refused by the maintenance window.
        let device = Device::new(&auto_document(false, "manual"));
        let manual = device
            .service
            .request_install(":1.7", &device.descriptor)
            .await
            .expect_err("a shut window refuses an operator's install");
        let automatic = device
            .routes
            .install(SENDER, &device.descriptor)
            .await
            .expect_err("and the automatic one");
        assert!(
            manual.to_string().ends_with(&automatic),
            "manual: {manual}, automatic: {automatic}"
        );
        assert!(automatic.contains("outside every configured maintenance window"));

        // The install, refused because the descriptor is not a verified one. The
        // automatic path reaches this route with a staged path, so the rule
        // that only `verified/` is installable binds it too.
        let stray = device.dir.path().join("stray.json");
        std::fs::write(&stray, b"not staged").expect("seed a stray descriptor");
        let stray = stray.to_str().expect("utf-8");
        let device = Device::new(&auto_document(true, "manual"));
        let manual = device
            .service
            .request_install(":1.7", stray)
            .await
            .expect_err("a path outside verified/ refuses an operator's install");
        let automatic = device
            .routes
            .install(SENDER, stray)
            .await
            .expect_err("and the automatic one");
        assert!(
            manual.to_string().ends_with(&automatic),
            "manual: {manual}, automatic: {automatic}"
        );

        // The reboot, refused by the safe-to-reboot gate.
        let device = Device::new(&auto_document(true, "window"));
        device
            .service
            .report_health("exporter", "blocking", "mid-transaction")
            .await
            .expect("report");
        let manual = device
            .service
            .request_reboot(":1.7")
            .await
            .expect_err("a blocking report refuses an operator's reboot");
        let automatic = device
            .routes
            .reboot(SENDER)
            .await
            .expect_err("and the automatic one");
        assert!(
            manual.to_string().ends_with(&automatic),
            "manual: {manual}, automatic: {automatic}"
        );
        assert!(automatic.contains("exporter"), "{automatic}");
        assert!(
            device.power.lock().expect("power").is_empty(),
            "neither refused reboot may reach the power control"
        );

        // The reboot, refused by an install in flight — the block no override
        // lifts. Reached by an install this driver started, which is the only
        // way the automatic path can be holding one.
        let gate = Arc::new(tokio::sync::Notify::new());
        let device = Device::with_deployments(
            &auto_document(true, "window"),
            MockDeployments {
                install_gate: Some(Arc::clone(&gate)),
                ..MockDeployments::default()
            },
        );
        device
            .routes
            .install(SENDER, &device.descriptor)
            .await
            .expect("the window is open");
        wait_for_install_status(&device.service, "running").await;
        let manual = device
            .service
            .request_reboot(":1.7")
            .await
            .expect_err("an install in flight refuses an operator's reboot");
        let automatic = device
            .routes
            .reboot(SENDER)
            .await
            .expect_err("and the automatic one");
        assert!(
            manual.to_string().ends_with(&automatic),
            "manual: {manual}, automatic: {automatic}"
        );
        assert!(automatic.contains("install"), "{automatic}");
        gate.notify_one();
        wait_for_install_status(&device.service, "done").await;
    }

    /// PLAN-071 U2, end to end: an automatic pass driven against a closed
    /// gate records **the gate's own words** and reaches nothing behind it.
    ///
    /// The pairs above assert that both routes answer the same refusal; this
    /// asserts that the driver actually meets those routes — a driver that
    /// installed through some other path would leave the same policy document
    /// and a very different the native backend call log.
    #[tokio::test]
    async fn an_automatic_pass_records_the_gates_own_refusal_and_reaches_nothing_behind_it() {
        use crate::update_auto::SENDER;

        let device = Device::new(&auto_document(false, "window"));
        let (mut driver, cadence) = device.driver();

        // Check and fetch run — neither is gated by the window — and the
        // install is refused by it.
        cadence.advance_past_the_check_interval();
        driver.tick().await;

        let manual = device
            .service
            .request_install(SENDER, &device.descriptor)
            .await
            .expect_err("the window is shut for an operator too");
        let deferred = device.deferral().await;
        assert_eq!(deferred["reason"], "outside-window");
        assert!(
            manual.to_string().ends_with(
                deferred["detail"]
                    .as_str()
                    .expect("a deferral carries the refusing rule")
            ),
            "manual: {manual}, deferred: {deferred}"
        );
        assert!(
            device.native.lock().expect("native").is_empty(),
            "a refused install must not reach the installer: {:?}",
            device.native.lock().expect("native")
        );
        // The fetch DID run: the window gates installs and only installs.
        assert_eq!(
            device.update_entry("update.lifecycle.deploymentId").await,
            Value::String("e".repeat(64))
        );

        // The operator opens the window; the same driver installs and reboots
        // through the same routes.
        device.rewrite(&auto_document(true, "window"));
        driver.tick().await;
        wait_for_install_status(&device.service, "done").await;
        driver.tick().await;
        assert_eq!(
            *device.power.lock().expect("power"),
            vec!["reboot".to_string()],
            "an open window and an open gate is the only combination that reboots"
        );
    }

    /// PLAN-071 U3: **an automatic path against a closed gate arms no
    /// override.**
    ///
    /// The invariant is structural — `SetRebootOverride` is not on
    /// [`crate::update_auto::AutoRoutes`], so the driver holds no capability
    /// to reach it — and this is what keeps the trait from growing one. The
    /// assertion is made against the real gate, where an armed override is a
    /// member of the recorded `update.reboot_gate`, and the last paragraph
    /// arms one by hand so that "no override" is a fact this test can tell
    /// apart from a state that renders nothing either way.
    #[tokio::test]
    async fn the_automatic_path_against_a_closed_gate_arms_no_override() {
        let device = Device::new(&auto_document(true, "window"));
        let (mut driver, cadence) = device.driver();

        // A pass that installs, so a reboot is owed and the gate is what
        // stands between the driver and it.
        cadence.advance_past_the_check_interval();
        driver.tick().await;
        wait_for_install_status(&device.service, "done").await;
        device
            .service
            .report_health("exporter", "blocking", "mid-transaction")
            .await
            .expect("report");

        for _ in 0..3 {
            driver.tick().await;
        }

        let deferred = device.deferral().await;
        assert_eq!(deferred["reason"], "reboot-gate-closed");
        assert_eq!(
            deferred["attempts"], 3,
            "four automatic attempts were refused, and by what: {deferred}"
        );
        assert!(
            device.power.lock().expect("power").is_empty(),
            "a closed gate is not a gate automation walks through"
        );
        let gate = device.reboot_gate().await;
        assert_eq!(gate["safe"], false);
        assert_eq!(gate["overridden"], false);
        assert!(
            gate["override"].is_null(),
            "the automatic path armed the override: {gate}"
        );

        // The override is a human's judgement, and it is visible in exactly
        // this member when a human makes it — so the assertion above is a
        // measurement and not an empty tree.
        device
            .lifecycle
            .set_reboot_override(":1.7", 120)
            .await
            .expect("an operator may arm it");
        let gate = device.reboot_gate().await;
        assert!(
            gate["override"]["until"].is_string(),
            "an armed override is visible here: {gate}"
        );
        driver.tick().await;
        assert_eq!(
            *device.power.lock().expect("power"),
            vec!["reboot".to_string()],
            "and the reboot the driver deferred goes through on the operator's judgement"
        );
    }
}

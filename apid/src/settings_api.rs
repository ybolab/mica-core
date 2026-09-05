//! Minimal async facade over the mosd settings/state API.
//!
//! Handlers depend on this trait so tests can substitute an in-memory fake
//! for the D-Bus client.

use serde_json::Value;

use crate::task_registry::TaskRecord;

#[derive(Debug)]
pub struct TaskNotFound(pub String);

impl std::fmt::Display for TaskNotFound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "task not found: `{}`", self.0)
    }
}

impl std::error::Error for TaskNotFound {}

/// mosd answered a task read, but the payload did not match the public task
/// record. This is a daemon-side 500, distinct from a transport outage.
#[derive(Debug)]
pub struct InvalidTaskPayload(pub serde_json::Error);

impl std::fmt::Display for InvalidTaskPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid task payload from mosd: {}", self.0)
    }
}

impl std::error::Error for InvalidTaskPayload {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// The mosd operations apid needs, JSON in and out.
///
/// The power actions are here rather than executed locally because mosd owns
/// every system action: apid never spawns a process and never talks to
/// systemd itself.
#[async_trait::async_trait]
pub trait SettingsApi: Send + Sync {
    /// Settings subtree at dot-path `path` (`""` = whole tree).
    async fn get_settings(&self, path: &str) -> anyhow::Result<Value>;
    /// Persist `value` at dot-path `path`, enqueue its apply and return the
    /// task id.
    async fn set_settings(&self, path: &str, value: &Value) -> anyhow::Result<String>;
    /// Task record by id.
    async fn get_task(&self, id: &str) -> anyhow::Result<TaskRecord> {
        Err(TaskNotFound(id.to_string()).into())
    }
    /// Live-state subtree at dot-path `path` (`""` = whole tree).
    async fn get_state(&self, path: &str) -> anyhow::Result<Value>;
    /// Live interface details observed by systemd-networkd.
    async fn get_network_state(&self) -> anyhow::Result<Value> {
        self.get_state("network").await
    }
    /// Time-synchronization status observed from timesyncd, classified by
    /// mosd. Read-only: there is deliberately no method beside it that could
    /// pause or stop synchronization.
    async fn get_time_status(&self) -> anyhow::Result<Value>;
    /// Storage status observed by mosd: the fixed tiers, their space and
    /// check evidence, the physical media and the low-space policy.
    ///
    /// Read-only, and deliberately alone: there is no method here that
    /// formats, repartitions or erases anything, because the layout is fixed
    /// by the image assembler and a management API that could rewrite it
    /// would be a remote destructive surface with no product use.
    async fn get_storage_status(&self) -> anyhow::Result<Value>;
    /// The system-information surface mosd assembles (PLAN-052): machine id,
    /// board, kernel, release, image version with git stamp and build date,
    /// installed packages, booted slot and uptime. Read-only.
    async fn get_system_info(&self) -> anyhow::Result<Value>;
    /// Board telemetry observed by mosd (PLAN-052): thermal, watchdog and
    /// reset reason, absence explicit. Read-only.
    async fn get_telemetry(&self) -> anyhow::Result<Value>;
    /// The OBSERVED network state (PLAN-052): link/carrier, addresses, DHCP
    /// lease, default routes, DNS reachability, Wi-Fi association and the
    /// radio/modem capabilities. Distinct from `get_network_state`'s reduced
    /// view and from the desired `network` settings. Read-only.
    async fn get_observed_network(&self) -> anyhow::Result<Value>;
    /// Failure evidence for the diagnostic snapshot (PLAN-052): failed units
    /// and a bounded journal excerpt, bounded by mosd. Read-only.
    async fn get_failure_evidence(&self) -> anyhow::Result<Value>;
    /// Ask mosd to reboot the appliance.
    async fn reboot(&self) -> anyhow::Result<()>;
    /// Ask mosd to power the appliance off.
    async fn power_off(&self) -> anyhow::Result<()>;
    /// Ask mosd to set a TRANSIENT root password, cleared on the next boot.
    ///
    /// Deliberately not a `set_settings` call: a password that reached the
    /// settings tree would be persisted, re-applied on the next boot and
    /// readable by anything that can call `GetSettings`.
    async fn set_transient_root_password(&self, password: &str) -> anyhow::Result<String>;
    /// Draw a new WireGuard private key for `iface` and return its new base64
    /// public key.
    ///
    /// Deliberately not a `set_settings` call, and for a stronger reason than
    /// the transient password's: the settings tree holds no key to write. The
    /// private half never leaves mosd and there is no accessor that returns
    /// one, so the only thing this call can hand back is the public half.
    async fn rotate_wireguard_key(&self, iface: &str) -> anyhow::Result<String>;
    /// The complete update state (`GetUpdateState`): mosd queries RAUC and
    /// re-derives the lifecycle before answering, so this is never stale.
    async fn get_update_state(&self) -> anyhow::Result<Value>;
    /// Ask mosd to run an update metadata check on a background task.
    async fn check_update(&self) -> anyhow::Result<()>;
    /// Ask mosd to download the selected bundle on a background task.
    async fn fetch_update(&self) -> anyhow::Result<()>;
    /// Ask mosd to install the bundle at absolute path `bundle` (RAUC's
    /// background install; progress lands in the update state).
    async fn install_update(&self, bundle: &str) -> anyhow::Result<()>;
    /// Manually mark a slot (`good`/`bad` on `booted`/`other`); answers
    /// RAUC's `(slot_name, message)`.
    async fn mark_update(&self, state: &str, slot: &str) -> anyhow::Result<(String, String)>;
    /// Arm the bounded safe-to-reboot override for `seconds`; answers the
    /// recorded override.
    async fn set_reboot_override(&self, seconds: u32) -> anyhow::Result<Value>;
    /// Clear the automatic-update suppression on `version`; answers the
    /// record that was removed.
    async fn clear_update_suppression(&self, version: &str) -> anyhow::Result<Value>;
    /// Ask mosd to merge `patch` into `/mos/config/updates.json` and write
    /// it; answers the document as saved.
    ///
    /// Deliberately not a file apid opens. mosd owns that document and is its
    /// only writer (PLAN-070 §5.2.7, PLAN-071 §3), so one fact has one writer
    /// all the way down to the filesystem — and the validation that decides
    /// what may be written is the same code the reader runs, in the same
    /// process, rather than a second copy here that would eventually disagree
    /// with it.
    async fn set_update_config(&self, patch: &Value) -> anyhow::Result<Value>;
}

/// A rendezvous armed over [`FakeSettings::hold_access_reads`]: the first
/// `parties` reads of the `access` subtree wait for each other before they are
/// answered.
///
/// It exists for the concurrent-claim test, which needs both claimants to have
/// taken the read half of the claim's check-then-act before either takes the
/// write half. **It widens a window it does not create** — the window is the
/// argon2id hash and the bus round trips the route runs between its read and
/// its write.
///
/// The wait is bounded, and the bound is load-bearing rather than defensive:
/// once the claim is one step the second read cannot happen until the first
/// request has finished, so the first read waits the bound out and then
/// proceeds. An unbounded barrier would hang there instead.
#[cfg(test)]
struct AccessHold {
    barrier: tokio::sync::Barrier,
    parties: usize,
    timeout: std::time::Duration,
}

/// In-memory [`SettingsApi`] used by the route tests.
#[cfg(test)]
pub struct FakeSettings {
    tree: std::sync::Mutex<Value>,
    state: std::sync::Mutex<Value>,
    /// What `get_time_status` answers; the shape mosd's `status_json` serves.
    time_status: std::sync::Mutex<Value>,
    /// What `get_storage_status` answers; the shape mosd's storage
    /// `status_json` serves.
    storage_status: std::sync::Mutex<Value>,
    /// What `get_system_info` answers; the shape mosd's `info_json` serves.
    system_info: std::sync::Mutex<Value>,
    /// What `get_telemetry` answers; the shape mosd's `telemetry_json` serves.
    telemetry: std::sync::Mutex<Value>,
    /// What `get_observed_network` answers; the shape mosd's `observed_json`
    /// serves.
    observed_network: std::sync::Mutex<Value>,
    /// What `get_failure_evidence` answers; the shape mosd's failure
    /// evidence serves.
    failure_evidence: std::sync::Mutex<Value>,
    /// When set, every PLAN-052 diagnostic read sleeps this long before
    /// answering — how the snapshot collector's deadline is proven to be
    /// enforced rather than hoped for.
    diagnostic_delay: std::sync::Mutex<Option<std::time::Duration>>,
    get_log: std::sync::Mutex<Vec<String>>,
    set_log: std::sync::Mutex<Vec<String>>,
    power_log: std::sync::Mutex<Vec<String>>,
    /// Interfaces passed to `rotate_wireguard_key`, in call order, each
    /// paired with the public key handed back.
    ///
    /// The interface and the answer, never a private key: the fake has none to
    /// store because the trait has no method that would produce one.
    rotations: std::sync::Mutex<Vec<(String, String)>>,
    /// How many times `set_transient_root_password` was called.
    ///
    /// A count, never the password. A fake that stored the password would let
    /// a test assert "the right password arrived" and pass while the real path
    /// leaks it somewhere else; there is nothing to assert about here except
    /// whether the call happened and how often.
    transient_password_calls: std::sync::Mutex<usize>,
    next_task: std::sync::atomic::AtomicU64,
    tasks: std::sync::Mutex<std::collections::BTreeMap<String, TaskRecord>>,
    /// Update calls received, in order (`check`, `fetch`, `install <path>`,
    /// `mark <state> <slot>`, `reboot-override <s>`, `get_update_state`).
    update_log: std::sync::Mutex<Vec<String>>,
    /// When set, every update action fails with a `zbus` `MethodError` of
    /// this fdo name and message — how a route test provokes the 409/422
    /// mappings the real mosd produces.
    update_refusal: std::sync::Mutex<Option<(&'static str, String)>>,
    /// When armed, the first reads of the `access` subtree rendezvous before
    /// they are answered. See [`AccessHold`].
    access_hold: std::sync::Mutex<Option<std::sync::Arc<AccessHold>>>,
    /// How many `access` reads have been taken, so a hold engages for the
    /// first `parties` of them and for no others.
    access_reads: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl FakeSettings {
    pub fn new(tree: Value) -> Self {
        Self {
            tree: std::sync::Mutex::new(tree),
            state: std::sync::Mutex::new(Value::Object(serde_json::Map::new())),
            time_status: std::sync::Mutex::new(serde_json::json!({
                "status": "synchronized",
                "synchronized": true,
            })),
            storage_status: std::sync::Mutex::new(serde_json::json!({
                "tiers": [],
                "namespaces": { "sharedCapacityTier": "data", "binds": [] },
                "media": [],
                "policy": {},
                "lifecycle": {},
            })),
            system_info: std::sync::Mutex::new(serde_json::json!({
                "machineId": { "available": true, "id": "0123456789abcdef0123456789abcdef" },
                "board": { "available": false, "detail": "fake" },
                "kernel": { "available": true, "release": "6.1.0-fake", "version": "#1" },
                "release": { "available": false, "detail": "fake" },
                "system": { "available": false, "detail": "fake" },
                "daemon": { "name": "mosd", "version": "0.1.0", "commit": null },
                "packages": { "available": false, "detail": "fake" },
                "slot": { "available": false, "detail": "fake" },
                "uptime": { "available": true, "seconds": 7 },
            })),
            telemetry: std::sync::Mutex::new(serde_json::json!({
                "thermal": { "available": false, "detail": "fake" },
                "watchdog": { "available": false, "detail": "fake" },
                "reset": { "available": false, "reason": "unknown", "detail": "fake",
                           "evidence": { "watchdogBootstatus": [], "pstore": { "available": false, "detail": "fake" } } },
            })),
            observed_network: std::sync::Mutex::new(serde_json::json!({
                "interfaces": { "available": false, "detail": "fake" },
                "defaultRoutes": { "available": false, "detail": "fake" },
                "dns": { "available": false, "detail": "fake", "linkServers": [] },
                "wifi": { "available": false, "detail": "fake" },
                "capabilities": {
                    "wifi": { "supported": false, "interfaces": [], "detail": "fake" },
                    "bluetooth": { "supported": false, "adapters": [], "detail": "fake" },
                    "cellular": { "supported": false, "interfaces": [], "detail": "fake" },
                },
            })),
            failure_evidence: std::sync::Mutex::new(serde_json::json!({
                "journal": { "available": false, "detail": "fake" },
                "units": { "available": false, "detail": "fake" },
            })),
            diagnostic_delay: std::sync::Mutex::new(None),
            get_log: std::sync::Mutex::new(Vec::new()),
            set_log: std::sync::Mutex::new(Vec::new()),
            power_log: std::sync::Mutex::new(Vec::new()),
            transient_password_calls: std::sync::Mutex::new(0),
            rotations: std::sync::Mutex::new(Vec::new()),
            next_task: std::sync::atomic::AtomicU64::new(0),
            tasks: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            update_log: std::sync::Mutex::new(Vec::new()),
            update_refusal: std::sync::Mutex::new(None),
            access_hold: std::sync::Mutex::new(None),
            access_reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Update actions received, in call order.
    pub fn update_calls(&self) -> Vec<String> {
        self.update_log.lock().unwrap().clone()
    }

    /// Make every subsequent update action fail as mosd would: a bus
    /// `MethodError` under fdo `name` carrying `message`.
    pub fn refuse_updates(&self, name: &'static str, message: &str) {
        *self.update_refusal.lock().unwrap() = Some((name, message.to_string()));
    }

    /// Log one update action and fail it when a refusal is scripted.
    fn update_call(&self, call: &str) -> anyhow::Result<()> {
        self.update_log.lock().unwrap().push(call.to_string());
        if let Some((name, message)) = self.update_refusal.lock().unwrap().clone() {
            let reply_to = zbus::message::Message::method_call("/com/mos/mosd", "CheckUpdate")
                .expect("a well-formed method call")
                .build(&())
                .expect("an empty body serialises");
            let name = zbus::names::ErrorName::try_from(name).expect("a well-formed error name");
            return Err(zbus::Error::MethodError(name.into(), Some(message), reply_to).into());
        }
        Ok(())
    }

    fn completed_task(&self, operation: &str, path: &str) -> String {
        let sequence = self
            .next_task
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let id = format!("fake-task-{sequence}");
        self.tasks.lock().unwrap().insert(
            id.clone(),
            TaskRecord {
                id: id.clone(),
                operation: operation.to_string(),
                dot_path: path.to_string(),
                source: "test".to_string(),
                status: "finished".to_string(),
                enqueued_at: "2026-08-31T00:00:00.000Z".to_string(),
                started_at: Some("2026-08-31T00:00:00.000Z".to_string()),
                finished_at: Some("2026-08-31T00:00:00.000Z".to_string()),
                outcome: Some("succeeded".to_string()),
                message: None,
                folded_count: 0,
            },
        );
        id
    }

    /// Rotations requested, as `(iface, public key answered)`, in call order.
    pub fn rotations(&self) -> Vec<(String, String)> {
        self.rotations.lock().unwrap().clone()
    }

    /// Replace what `get_time_status` answers.
    pub fn set_time_status(&self, value: Value) {
        *self.time_status.lock().unwrap() = value;
    }

    /// Replace what `get_storage_status` answers.
    pub fn set_storage_status(&self, value: Value) {
        *self.storage_status.lock().unwrap() = value;
    }

    /// Replace what `get_system_info` answers.
    pub fn set_system_info(&self, value: Value) {
        *self.system_info.lock().unwrap() = value;
    }

    /// Replace what `get_telemetry` answers.
    pub fn set_telemetry(&self, value: Value) {
        *self.telemetry.lock().unwrap() = value;
    }

    /// Replace what `get_observed_network` answers.
    pub fn set_observed_network(&self, value: Value) {
        *self.observed_network.lock().unwrap() = value;
    }

    /// Replace what `get_failure_evidence` answers.
    pub fn set_failure_evidence(&self, value: Value) {
        *self.failure_evidence.lock().unwrap() = value;
    }

    /// Make every diagnostic read sleep `delay` before answering.
    pub fn set_diagnostic_delay(&self, delay: std::time::Duration) {
        *self.diagnostic_delay.lock().unwrap() = Some(delay);
    }

    /// Hold the first `parties` reads of the `access` subtree at a rendezvous,
    /// each waiting at most `timeout` for the others.
    ///
    /// The seam a concurrent-claim test drives: it makes both claimants take
    /// the read half of the claim's check-then-act before either takes the
    /// write half. See [`AccessHold`] for what the bound means and for why
    /// this widens a window rather than inventing one.
    pub fn hold_access_reads(&self, parties: usize, timeout: std::time::Duration) {
        *self.access_hold.lock().unwrap() = Some(std::sync::Arc::new(AccessHold {
            barrier: tokio::sync::Barrier::new(parties),
            parties,
            timeout,
        }));
    }

    /// Hold this read at the armed rendezvous, if it is an `access` read and
    /// one of the first `parties` of them.
    async fn hold_access_read(&self, path: &str) {
        if path != "access" {
            return;
        }
        // The lock is released before the await: a `std::sync` guard held
        // across one would deadlock the second claimant against the first.
        let hold = {
            let armed = self.access_hold.lock().unwrap();
            let Some(hold) = armed.as_ref() else { return };
            let taken = self
                .access_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if taken >= hold.parties {
                return;
            }
            hold.clone()
        };
        if tokio::time::timeout(hold.timeout, hold.barrier.wait())
            .await
            .is_err()
        {
            // The rendezvous did not fill, so the parties it was waiting for
            // are not coming. Disarm it rather than make every later read wait
            // the bound out on its own.
            *self.access_hold.lock().unwrap() = None;
        }
    }

    async fn diagnostic_pause(&self) {
        let delay = *self.diagnostic_delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
    }

    /// Insert `value` at top-level `key` of the live-state tree.
    pub fn set_state_entry(&self, key: &str, value: Value) {
        self.state
            .lock()
            .unwrap()
            .as_object_mut()
            .expect("state root is an object")
            .insert(key.to_string(), value);
    }

    /// Dot-paths passed to `set_settings`, in call order.
    pub fn set_paths(&self) -> Vec<String> {
        self.set_log.lock().unwrap().clone()
    }

    /// Power actions requested, in call order.
    pub fn power_calls(&self) -> Vec<String> {
        self.power_log.lock().unwrap().clone()
    }

    /// How many transient-password requests reached the backend.
    pub fn transient_password_calls(&self) -> usize {
        *self.transient_password_calls.lock().unwrap()
    }

    /// Poll [`Self::power_calls`] until it holds at least `count` entries or a
    /// short deadline passes, then return it.
    ///
    /// The routes fire power actions on a detached task so the HTTP response
    /// can go out first, so a positive assertion has to wait for the task; a
    /// negative assertion passes `count` one higher than it expects and gets
    /// the full deadline as its quiet period.
    pub async fn await_power_calls(&self, count: usize) -> Vec<String> {
        for _ in 0..150 {
            let calls = self.power_calls();
            if calls.len() >= count {
                return calls;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        self.power_calls()
    }
}

/// Split `path` the way the real store splits it.
///
/// [`mosd_settings::path_segments`] and not `str::split('.')`: a quoted
/// segment carries a dot as an ordinary character, so a fake that split
/// unconditionally would put a VLAN write at the two keys `"eth0` and `100"`
/// and let a route test assert the write "arrived".
#[cfg(test)]
fn fake_segments(path: &str) -> anyhow::Result<Vec<String>> {
    mosd_settings::path_segments(path).ok_or_else(|| anyhow::anyhow!("malformed path: `{path}`"))
}

#[cfg(test)]
fn fake_get(root: &Value, path: &str) -> anyhow::Result<Value> {
    if path.is_empty() {
        return Ok(root.clone());
    }
    fake_segments(path)?
        .iter()
        .try_fold(root, |node, segment| node.get(segment))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("path not found: `{path}`"))
}

#[cfg(test)]
#[async_trait::async_trait]
impl SettingsApi for FakeSettings {
    async fn get_settings(&self, path: &str) -> anyhow::Result<Value> {
        self.get_log.lock().unwrap().push(path.to_string());
        let value = fake_get(&self.tree.lock().unwrap(), path);
        self.hold_access_read(path).await;
        value
    }

    async fn set_settings(&self, path: &str, value: &Value) -> anyhow::Result<String> {
        self.set_log.lock().unwrap().push(path.to_string());
        let mut segments = fake_segments(path)?;
        let last = segments.pop().expect("a path has at least one segment");
        let mut tree = self.tree.lock().unwrap();
        let mut node = &mut *tree;
        for segment in segments {
            node = node
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("not an object at `{segment}`"))?
                .entry(segment)
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
        }
        node.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("not an object at `{last}`"))?
            .insert(last, value.clone());
        Ok(self.completed_task("settings-write", path))
    }

    async fn get_task(&self, id: &str) -> anyhow::Result<TaskRecord> {
        self.tasks
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| TaskNotFound(id.to_string()).into())
    }

    async fn get_state(&self, path: &str) -> anyhow::Result<Value> {
        if path == "tasks" {
            return serde_json::to_value(
                self.tasks
                    .lock()
                    .unwrap()
                    .values()
                    .cloned()
                    .collect::<Vec<_>>(),
            )
            .map_err(Into::into);
        }
        let mut state = fake_get(&self.state.lock().unwrap(), path)?;
        if path.is_empty() {
            let tasks = serde_json::to_value(
                self.tasks
                    .lock()
                    .unwrap()
                    .values()
                    .cloned()
                    .collect::<Vec<_>>(),
            )?;
            if let Some(root) = state.as_object_mut() {
                root.insert("tasks".to_string(), tasks);
            }
        }
        Ok(state)
    }

    async fn get_time_status(&self) -> anyhow::Result<Value> {
        Ok(self.time_status.lock().unwrap().clone())
    }

    async fn get_storage_status(&self) -> anyhow::Result<Value> {
        self.diagnostic_pause().await;
        Ok(self.storage_status.lock().unwrap().clone())
    }

    async fn get_system_info(&self) -> anyhow::Result<Value> {
        self.diagnostic_pause().await;
        Ok(self.system_info.lock().unwrap().clone())
    }

    async fn get_telemetry(&self) -> anyhow::Result<Value> {
        self.diagnostic_pause().await;
        Ok(self.telemetry.lock().unwrap().clone())
    }

    async fn get_observed_network(&self) -> anyhow::Result<Value> {
        self.diagnostic_pause().await;
        Ok(self.observed_network.lock().unwrap().clone())
    }

    async fn get_failure_evidence(&self) -> anyhow::Result<Value> {
        self.diagnostic_pause().await;
        Ok(self.failure_evidence.lock().unwrap().clone())
    }

    async fn reboot(&self) -> anyhow::Result<()> {
        self.power_log.lock().unwrap().push("reboot".to_string());
        Ok(())
    }

    async fn power_off(&self) -> anyhow::Result<()> {
        self.power_log.lock().unwrap().push("power_off".to_string());
        Ok(())
    }

    async fn set_transient_root_password(&self, _password: &str) -> anyhow::Result<String> {
        // The password is dropped here on purpose; see the field's comment.
        *self.transient_password_calls.lock().unwrap() += 1;
        Ok(self.completed_task("transient-password", "access.ssh"))
    }

    async fn rotate_wireguard_key(&self, iface: &str) -> anyhow::Result<String> {
        // A distinct answer per call, so a test can tell a fresh rotation from
        // a cached one. Base64 of 32 bytes, the shape a real public key has.
        let count = self.rotations.lock().unwrap().len();
        let public_key = mosd_settings::encode_base64_nopad(&[count as u8; 32]);
        self.rotations
            .lock()
            .unwrap()
            .push((iface.to_string(), public_key.clone()));
        Ok(public_key)
    }

    async fn get_update_state(&self) -> anyhow::Result<Value> {
        // The fake's "refreshed" state is whatever the test seeded under the
        // live-state `update` key — the route's job is transport, not
        // derivation, which mosd's own tests own.
        self.update_log
            .lock()
            .unwrap()
            .push("get_update_state".to_string());
        fake_get(&self.state.lock().unwrap(), "update")
    }

    async fn check_update(&self) -> anyhow::Result<()> {
        self.update_call("check")
    }

    async fn fetch_update(&self) -> anyhow::Result<()> {
        self.update_call("fetch")
    }

    async fn install_update(&self, bundle: &str) -> anyhow::Result<()> {
        self.update_call(&format!("install {bundle}"))
    }

    async fn mark_update(&self, state: &str, slot: &str) -> anyhow::Result<(String, String)> {
        self.update_call(&format!("mark {state} {slot}"))?;
        Ok(("rootfs.0".to_string(), format!("marked {slot} as {state}")))
    }

    async fn set_reboot_override(&self, seconds: u32) -> anyhow::Result<Value> {
        self.update_call(&format!("reboot-override {seconds}"))?;
        Ok(serde_json::json!({
            "until": "2026-09-02T00:10:00Z",
            "requestedBy": ":1.9",
        }))
    }

    async fn clear_update_suppression(&self, version: &str) -> anyhow::Result<Value> {
        self.update_call(&format!("clear-suppression {version}"))?;
        Ok(serde_json::json!({
            "version": version,
            "slot": "rootfs.1",
            "at": "2026-09-02T00:00:00Z",
            "bootStatus": "bad",
            "detail": format!("version {version} was installed into slot rootfs.1"),
        }))
    }

    async fn set_update_config(&self, patch: &Value) -> anyhow::Result<Value> {
        self.update_call(&format!("config {patch}"))?;
        // The patch echoed as the document, which is what merging it over an
        // empty one produces. The merge itself is mosd's and is tested there:
        // this route's job is the transport, the authority and the audit, and
        // a fake that re-implemented the precedence would be a second answer
        // to a question the library already answers once.
        Ok(patch.clone())
    }
}

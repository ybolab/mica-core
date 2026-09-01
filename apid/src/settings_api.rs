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
}

/// In-memory [`SettingsApi`] used by the route tests.
#[cfg(test)]
pub struct FakeSettings {
    tree: std::sync::Mutex<Value>,
    state: std::sync::Mutex<Value>,
    /// What `get_time_status` answers; the shape mosd's `status_json` serves.
    time_status: std::sync::Mutex<Value>,
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
            get_log: std::sync::Mutex::new(Vec::new()),
            set_log: std::sync::Mutex::new(Vec::new()),
            power_log: std::sync::Mutex::new(Vec::new()),
            transient_password_calls: std::sync::Mutex::new(0),
            rotations: std::sync::Mutex::new(Vec::new()),
            next_task: std::sync::atomic::AtomicU64::new(0),
            tasks: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
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
        fake_get(&self.tree.lock().unwrap(), path)
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
}

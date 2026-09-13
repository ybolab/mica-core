//! Notification-fed mirror of micad's apply-task records.
//!
//! The registry follows the same lockout rule as [`crate::access_cache`]: it
//! serves only while a `TaskChanged` subscription is known live. A lapse
//! clears the mirror and forces callers back to `GetTask`; a generation check
//! prevents a direct read that raced a signal from overwriting the newer
//! record.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};

const MAX_TASKS: usize = 128;

/// One queued apply lifecycle, shared by the D-Bus client and HTTP surface.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: String,
    pub operation: String,
    pub dot_path: String,
    pub source: String,
    pub status: String,
    pub enqueued_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub folded_count: u64,
}

impl TaskRecord {
    pub fn terminal(&self) -> bool {
        self.status == "finished"
    }
}

#[derive(Default)]
struct Inner {
    synchronised: bool,
    snapshot_loaded: bool,
    generation: u64,
    tasks: BTreeMap<String, TaskRecord>,
    /// Task ids in micad's oldest-first insertion order. Task ids end in an
    /// unpadded hexadecimal counter, so sorting the map keys would put `10`
    /// before `2` and break the collection contract.
    order: VecDeque<String>,
    /// Last records from a lapsed subscription. Never served directly; they
    /// are used only after a direct `GetTask` confirms micad no longer retains
    /// the id, which lets a pre-restart running record become interrupted.
    stale: BTreeMap<String, TaskRecord>,
}

impl Inner {
    fn insert(&mut self, task: TaskRecord) {
        if !self.tasks.contains_key(&task.id) {
            self.order.push_back(task.id.clone());
        }
        self.tasks.insert(task.id.clone(), task);
        while self.tasks.len() > MAX_TASKS {
            let Some(id) = self.order.pop_front() else {
                break;
            };
            self.tasks.remove(&id);
        }
    }
}

#[derive(Default)]
pub struct TaskRegistry {
    inner: Mutex<Inner>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn get(&self, id: &str) -> Option<TaskRecord> {
        let inner = self.lock();
        inner
            .synchronised
            .then(|| inner.tasks.get(id).cloned())
            .flatten()
    }

    pub fn list(&self) -> Option<Vec<TaskRecord>> {
        let inner = self.lock();
        (inner.synchronised && inner.snapshot_loaded).then(|| {
            inner
                .order
                .iter()
                .filter_map(|id| inner.tasks.get(id).cloned())
                .collect()
        })
    }

    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    pub fn stale(&self, id: &str) -> Option<TaskRecord> {
        self.lock().stale.get(id).cloned()
    }

    /// Fill from a direct read only if no signal or subscription transition
    /// happened since `generation` was sampled.
    pub fn fill(&self, generation: u64, task: TaskRecord) {
        let mut inner = self.lock();
        if inner.synchronised && inner.generation == generation {
            inner.stale.remove(&task.id);
            inner.insert(task);
        }
    }

    pub fn fill_list(&self, generation: u64, mut tasks: Vec<TaskRecord>) {
        let mut inner = self.lock();
        if inner.synchronised && inner.generation == generation {
            if tasks.len() > MAX_TASKS {
                tasks.drain(..tasks.len() - MAX_TASKS);
            }
            for task in &tasks {
                inner.stale.remove(&task.id);
            }
            inner.order = tasks.iter().map(|task| task.id.clone()).collect();
            inner.tasks = tasks
                .into_iter()
                .map(|task| (task.id.clone(), task))
                .collect();
            inner.snapshot_loaded = true;
        }
    }

    /// Apply a `TaskChanged` record. Bumping the generation invalidates any
    /// direct read that began before this notification arrived.
    pub fn update(&self, task: TaskRecord) {
        let mut inner = self.lock();
        if inner.synchronised {
            inner.generation += 1;
            inner.stale.remove(&task.id);
            inner.insert(task);
        }
    }

    pub fn subscribed(&self) {
        let mut inner = self.lock();
        inner.synchronised = true;
        inner.snapshot_loaded = false;
        inner.generation += 1;
        inner.tasks.clear();
        inner.order.clear();
    }

    pub fn lapsed(&self) {
        let mut inner = self.lock();
        inner.synchronised = false;
        inner.snapshot_loaded = false;
        inner.generation += 1;
        let tasks = std::mem::take(&mut inner.tasks);
        inner.order.clear();
        inner.stale.extend(tasks);
        while inner.stale.len() > MAX_TASKS {
            let oldest = inner
                .stale
                .values()
                .min_by_key(|task| (&task.enqueued_at, &task.id))
                .map(|task| task.id.clone())
                .expect("a non-empty stale task set");
            inner.stale.remove(&oldest);
        }
    }

    #[cfg(test)]
    pub fn is_synchronised(&self) -> bool {
        self.lock().synchronised
    }
}

#[cfg(test)]
mod tests {
    use super::{TaskRecord, TaskRegistry};

    fn running(id: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            operation: "settings-write".to_string(),
            dot_path: "hostname".to_string(),
            source: ":1.7".to_string(),
            status: "running".to_string(),
            enqueued_at: "2026-08-31T00:00:00.000Z".to_string(),
            started_at: Some("2026-08-31T00:00:01.000Z".to_string()),
            finished_at: None,
            outcome: None,
            message: None,
            folded_count: 0,
        }
    }

    #[test]
    fn a_lapse_never_serves_a_stale_running_record() {
        let registry = TaskRegistry::new();
        registry.subscribed();
        registry.update(running("task-1"));
        assert_eq!(
            registry.get("task-1").expect("live record").status,
            "running"
        );

        registry.lapsed();
        assert_eq!(registry.get("task-1"), None);
        assert_eq!(registry.list(), None);
        assert_eq!(
            registry
                .stale("task-1")
                .expect("private stale record")
                .status,
            "running"
        );
    }

    #[test]
    fn a_fill_that_raced_a_task_signal_is_discarded() {
        let registry = TaskRegistry::new();
        registry.subscribed();
        let generation = registry.generation();
        let mut finished = running("task-1");
        finished.status = "finished".to_string();
        finished.outcome = Some("succeeded".to_string());
        registry.update(finished.clone());
        registry.fill(generation, running("task-1"));

        assert_eq!(registry.get("task-1"), Some(finished));
    }

    #[test]
    fn task_collection_keeps_micad_order_across_hex_counter_widths() {
        let registry = TaskRegistry::new();
        registry.subscribed();
        let generation = registry.generation();
        registry.fill_list(
            generation,
            ["boot-e", "boot-f", "boot-10"]
                .into_iter()
                .map(running)
                .collect(),
        );

        let ids: Vec<_> = registry
            .list()
            .expect("snapshot")
            .into_iter()
            .map(|task| task.id)
            .collect();
        assert_eq!(ids, ["boot-e", "boot-f", "boot-10"]);
    }

    #[test]
    fn task_collection_remains_bounded_as_signals_arrive() {
        let registry = TaskRegistry::new();
        registry.subscribed();
        let generation = registry.generation();
        registry.fill_list(generation, Vec::new());
        for index in 0..130 {
            registry.update(running(&format!("task-{index}")));
        }

        let tasks = registry.list().expect("snapshot");
        assert_eq!(tasks.len(), 128);
        assert_eq!(tasks.first().map(|task| task.id.as_str()), Some("task-2"));
        assert_eq!(tasks.last().map(|task| task.id.as_str()), Some("task-129"));
    }
}

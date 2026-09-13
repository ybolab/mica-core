//! In-memory reconcile queue and its bounded task history.
//!
//! Settings are persisted before a job enters this queue. Jobs therefore
//! carry only operation metadata and a dot-path; they never carry settings
//! values or secret material. One worker drains the queue and the caller owns
//! the actual reconcile.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{PoisonError, RwLock};

use chrono::{SecondsFormat, Utc};
use tokio::sync::{Mutex, Notify};
use zbus::object_server::SignalEmitter;

/// Maximum number of queued/running/terminal records retained in RAM.
const MAX_TASKS: usize = 128;

/// Public task record carried by D-Bus, HTTP and the live-state tree.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
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

/// Minimal executable half of a task. No setting value and no secret can fit
/// in this type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyJob {
    pub id: String,
    pub dot_path: String,
}

#[derive(Debug)]
struct State {
    boot_id: String,
    next_id: u64,
    pending: VecDeque<ApplyJob>,
    order: VecDeque<String>,
    records: BTreeMap<String, TaskRecord>,
}

/// Result of an enqueue. `created = false` means the request folded into the
/// returned pending task and did not add another reconcile.
#[derive(Clone, Debug)]
pub struct Enqueued {
    pub record: TaskRecord,
    pub created: bool,
}

pub struct ApplyQueue {
    state: Mutex<State>,
    ready: Notify,
    worker_started: AtomicBool,
    /// The service connection/object path learned from the most recent bus
    /// method. The worker uses an owned clone to emit lifecycle transitions.
    emitter: RwLock<Option<SignalEmitter<'static>>>,
}

impl ApplyQueue {
    pub fn new() -> Self {
        let now = Utc::now().timestamp_micros();
        Self {
            state: Mutex::new(State {
                boot_id: format!("{now:x}-{:x}", std::process::id()),
                next_id: 0,
                pending: VecDeque::new(),
                order: VecDeque::new(),
                records: BTreeMap::new(),
            }),
            ready: Notify::new(),
            worker_started: AtomicBool::new(false),
            emitter: RwLock::new(None),
        }
    }

    pub fn remember_emitter(&self, emitter: &SignalEmitter<'_>) {
        *self.emitter.write().unwrap_or_else(PoisonError::into_inner) = Some(emitter.to_owned());
    }

    pub fn emitter(&self) -> Option<SignalEmitter<'static>> {
        self.emitter
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// True exactly once, for the caller that must spawn the one worker.
    pub fn claim_worker(&self) -> bool {
        self.worker_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub async fn enqueue(&self, operation: &str, dot_path: &str, source: &str) -> Enqueued {
        let mut state = self.state.lock().await;

        if let Some(index) = state.pending.iter().position(|job| {
            path_subsumes(&job.dot_path, dot_path) || path_subsumes(dot_path, &job.dot_path)
        }) {
            let job = &mut state.pending[index];
            if path_subsumes(dot_path, &job.dot_path) {
                job.dot_path = dot_path.to_string();
            }
            let id = job.id.clone();
            let dot_path = job.dot_path.clone();
            let record = state
                .records
                .get_mut(&id)
                .expect("INVARIANT: every pending job has a task record");
            record.dot_path = dot_path;
            record.folded_count += 1;
            return Enqueued {
                record: record.clone(),
                created: false,
            };
        }

        if state.records.len() >= MAX_TASKS {
            let terminal = state
                .order
                .iter()
                .position(|id| state.records.get(id).is_some_and(TaskRecord::terminal));
            if let Some(index) = terminal {
                if let Some(id) = state.order.remove(index) {
                    state.records.remove(&id);
                }
            } else if let Some(job) = state.pending.front_mut() {
                // One worker means a full all-active table always has at
                // least one pending job. Broaden the oldest to the root and
                // fold into it: bounded memory, no lost persisted write, and
                // one conservative reconcile instead of rejecting after the
                // setting has already reached disk.
                job.dot_path.clear();
                let id = job.id.clone();
                let record = state
                    .records
                    .get_mut(&id)
                    .expect("INVARIANT: every pending job has a task record");
                record.dot_path.clear();
                record.folded_count += 1;
                return Enqueued {
                    record: record.clone(),
                    created: false,
                };
            }
        }

        state.next_id += 1;
        let id = format!("{}-{:x}", state.boot_id, state.next_id);
        let record = TaskRecord {
            id: id.clone(),
            operation: operation.to_string(),
            dot_path: dot_path.to_string(),
            source: source.to_string(),
            status: "queued".to_string(),
            enqueued_at: timestamp(),
            started_at: None,
            finished_at: None,
            outcome: None,
            message: None,
            folded_count: 0,
        };
        state.pending.push_back(ApplyJob {
            id: id.clone(),
            dot_path: dot_path.to_string(),
        });
        state.order.push_back(id.clone());
        state.records.insert(id, record.clone());
        Enqueued {
            record,
            created: true,
        }
    }

    pub fn wake(&self) {
        self.ready.notify_one();
    }

    pub async fn next(&self) -> ApplyJob {
        loop {
            let notified = self.ready.notified();
            if let Some(job) = self.state.lock().await.pending.pop_front() {
                return job;
            }
            notified.await;
        }
    }

    pub async fn start(&self, id: &str) -> Option<TaskRecord> {
        let mut state = self.state.lock().await;
        let record = state.records.get_mut(id)?;
        record.status = "running".to_string();
        record.started_at = Some(timestamp());
        Some(record.clone())
    }

    pub async fn finish(
        &self,
        id: &str,
        outcome: &str,
        message: Option<String>,
    ) -> Option<TaskRecord> {
        let mut state = self.state.lock().await;
        let record = state.records.get_mut(id)?;
        record.status = "finished".to_string();
        record.finished_at = Some(timestamp());
        record.outcome = Some(outcome.to_string());
        record.message = message;
        Some(record.clone())
    }

    pub async fn get(&self, id: &str) -> Option<TaskRecord> {
        self.state.lock().await.records.get(id).cloned()
    }

    pub async fn snapshot(&self) -> Vec<TaskRecord> {
        let state = self.state.lock().await;
        state
            .order
            .iter()
            .filter_map(|id| state.records.get(id).cloned())
            .collect()
    }
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Segment-wise subtree containment. Quoted dot-path segments are parsed by
/// the settings crate, so `network."eth0.100"` stays one segment.
fn path_subsumes(parent: &str, child: &str) -> bool {
    let segments = |path: &str| {
        if path.is_empty() || path == "." {
            Some(Vec::new())
        } else {
            micad_settings::path_segments(path)
        }
    };
    let (Some(parent), Some(child)) = (segments(parent), segments(child)) else {
        return false;
    };
    parent.len() <= child.len() && parent.iter().zip(&child).all(|(left, right)| left == right)
}

#[cfg(test)]
mod tests {
    use super::{ApplyQueue, path_subsumes};

    #[test]
    fn subsumption_is_segment_wise_and_understands_quoted_segments() {
        assert!(path_subsumes("network", "network.eth0.dhcp"));
        assert!(path_subsumes(
            "network.\"eth0.100\"",
            "network.\"eth0.100\".dhcp"
        ));
        assert!(!path_subsumes("network.eth0", "network.eth01"));
        assert!(!path_subsumes("hostname", "network"));
    }

    #[tokio::test]
    async fn identical_pending_jobs_fold_into_one_record() {
        let queue = ApplyQueue::new();
        let first = queue
            .enqueue("settings-write", "access.ssh.enabled", ":1.7")
            .await;
        let second = queue
            .enqueue("settings-write", "access.ssh.enabled", ":1.7")
            .await;

        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.record.id, second.record.id);
        assert_eq!(second.record.folded_count, 1);
        assert_eq!(queue.snapshot().await.len(), 1);
    }

    #[tokio::test]
    async fn a_broader_pending_job_absorbs_a_narrower_one() {
        let queue = ApplyQueue::new();
        let first = queue
            .enqueue("settings-write", "network.eth0.dhcp", ":1.7")
            .await;
        let second = queue.enqueue("settings-write", "network", ":1.8").await;

        assert_eq!(first.record.id, second.record.id);
        assert_eq!(second.record.dot_path, "network");
        assert_eq!(queue.next().await.dot_path, "network");
    }

    #[tokio::test]
    async fn a_transient_password_task_contains_no_password_material() {
        let queue = ApplyQueue::new();
        let plaintext = "correct horse battery";
        let task = queue
            .enqueue("transient-password", "access.ssh", ":1.9")
            .await;
        let rendered = serde_json::to_string(&task.record).expect("serialize task");

        assert!(!rendered.contains(plaintext), "leaked task: {rendered}");
        assert_eq!(task.record.dot_path, "access.ssh");
        assert_eq!(task.record.operation, "transient-password");
    }
}

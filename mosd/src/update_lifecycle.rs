//! Update lifecycle: the explicit state machine over `rauc-update` and RAUC.
//!
//! Like power actions and installs, the lifecycle is an action surface, not a
//! settings subtree — nothing here persists and nothing reconciles on boot.
//! Everything observable is recorded in the live-state tree under
//! `update.lifecycle` through the [`LifecycleHost`] the bus layer provides.
//!
//! The states, and who produces them:
//!
//! - `idle`, `checking`, `downloading`, `ready`, `failed` — this module's own
//!   machine, driven by `CheckUpdate`/`FetchUpdate` running the device-side
//!   client `rauc-update` as a bounded subprocess.
//! - `installing` — mirrored from the bus layer's install flag; the install
//!   itself stays on the existing `InstallUpdate` path.
//! - `reboot-required`, `validating`, `succeeded`, `rolled-back` — derived
//!   from RAUC slot status plus boot state by [`derive_boot_phase`]; see its
//!   docs for exactly what each derivation reads and the stated limit.
//!
//! Verify-before-install stays in `rauc-update`: the only bundle path this
//! module ever records as `ready` is the verified path the client printed,
//! and mosd's install surface remains `InstallUpdate` — an explicit operator
//! action against a named path. No mosd lock is held across a subprocess or
//! an install.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::rauc::SlotStatus;
use crate::update_policy::{self, GateVerdict, LoadedPolicy, PolicyStore, UpdatePolicy};

/// Where the device-side update client lives unless `MOSD_RAUC_UPDATE_BIN`
/// says otherwise. Shipping the binary there is the image side's half of the
/// contract; its absence is reported, never panicked over.
pub const DEFAULT_CLIENT_PATH: &str = "/usr/bin/rauc-update";

/// Bound on one `sync` or `check` subprocess: metadata is a handful of files
/// capped at 1 MiB each, so ten minutes is generous slack for a slow link,
/// not an expected duration.
const CHECK_TIMEOUT: Duration = Duration::from_secs(600);

/// Bound on one `fetch` subprocess. A bundle is hundreds of MiB and the link
/// may be slow; four hours is a fault bound, and a fetch that dies there
/// resumes from its `.part` file on the next attempt.
const FETCH_TIMEOUT: Duration = Duration::from_secs(4 * 3600);

/// What one client invocation produced.
pub struct ClientOutput {
    /// Process exit code; `None` when killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The client binary is not there to run. Its own error type so the caller
/// can say "client unavailable" instead of a generic failure.
#[derive(Debug)]
pub struct ClientUnavailable(pub String);

impl std::fmt::Display for ClientUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "update client unavailable: {}", self.0)
    }
}

impl std::error::Error for ClientUnavailable {}

/// Runs `rauc-update` (or a stand-in) with an argument vector.
///
/// One method rather than one per subcommand: the argument rendering and the
/// output parsing are pure functions tested on their own, so the trait's only
/// job is "run this and give me what it said" — which is exactly what a mock
/// has to fake and nothing more.
#[async_trait::async_trait]
pub trait UpdateClient: Send + Sync {
    /// Whether the client can run at all; `Some(reason)` when it cannot.
    fn unavailable(&self) -> Option<String>;

    /// Run one invocation, bounded by `timeout`.
    async fn run(&self, args: &[String], timeout: Duration) -> Result<ClientOutput>;
}

/// Production client: spawn the configured binary as a subprocess.
pub struct SubprocessClient {
    binary: PathBuf,
}

impl SubprocessClient {
    pub fn new(binary: PathBuf) -> Self {
        Self { binary }
    }
}

#[async_trait::async_trait]
impl UpdateClient for SubprocessClient {
    fn unavailable(&self) -> Option<String> {
        if self.binary.is_file() {
            None
        } else {
            Some(format!(
                "{} is not present on this image",
                self.binary.display()
            ))
        }
    }

    async fn run(&self, args: &[String], timeout: Duration) -> Result<ClientOutput> {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                anyhow::Error::new(ClientUnavailable(format!(
                    "{} is not present on this image",
                    self.binary.display()
                )))
            } else {
                anyhow::Error::from(err).context(format!("spawn {}", self.binary.display()))
            }
        })?;
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "{} {} did not finish within {timeout:?}",
                    self.binary.display(),
                    args.first().map(String::as_str).unwrap_or_default()
                )
            })??;
        Ok(ClientOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// The client a daemon that was never handed one has: none. The default in
/// [`crate::bus::MosdService`], so a dry-run daemon can neither spawn a
/// process nor claim it could.
pub struct NoClient;

#[async_trait::async_trait]
impl UpdateClient for NoClient {
    fn unavailable(&self) -> Option<String> {
        Some("this daemon was started without an update client".to_string())
    }

    async fn run(&self, _args: &[String], _timeout: Duration) -> Result<ClientOutput> {
        Err(anyhow::Error::new(ClientUnavailable(
            "this daemon was started without an update client".to_string(),
        )))
    }
}

/// What the bus layer gives the lifecycle to observe and record with.
#[async_trait::async_trait]
pub trait LifecycleHost: Send + Sync {
    /// Replace the live-state `update.lifecycle` entry with `lifecycle`.
    async fn record(&self, lifecycle: Value);
    /// The live-state `health` subtree as last reported (empty object when
    /// nothing has reported).
    async fn health(&self) -> Value;
}

/// A refused action, split by what the caller can do about it.
#[derive(Debug)]
pub enum Refusal {
    /// The policy forbids it right now; the message names the rule.
    Policy(String),
    /// Another lifecycle operation or an install is in flight.
    Busy(String),
    /// The client binary is not there to run.
    Unavailable(String),
    /// The request itself is malformed (e.g. an override TTL of zero).
    Invalid(String),
}

impl Refusal {
    pub fn message(&self) -> &str {
        match self {
            Self::Policy(m) | Self::Busy(m) | Self::Unavailable(m) | Self::Invalid(m) => m,
        }
    }
}

/// The candidate the last `check` selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub name: String,
    pub version: String,
    pub channel: String,
}

/// What a finished `check` said.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    Selected(Available),
    /// Exit 2: everything published was rejected; the device is up to date
    /// (or nothing compatible is published, which the reasons distinguish).
    NoneCompatible,
}

/// Parse `rauc-update check` output by its printed contract:
/// `selected <name> version <v> channel <c> (<n> bytes)` on success, exit 2
/// for "nothing compatible", anything else is the stderr reason.
pub fn parse_check(output: &ClientOutput) -> Result<CheckOutcome, String> {
    match output.code {
        Some(0) => {
            let selected = output
                .stdout
                .lines()
                .rev()
                .find(|line| line.starts_with("selected "));
            let Some(line) = selected else {
                return Err(format!(
                    "check exited 0 without a `selected` line; stdout: {}",
                    output.stdout.trim()
                ));
            };
            let mut words = line.split_whitespace();
            let name = words.nth(1).unwrap_or_default().to_string();
            let version = words.nth(1).unwrap_or_default().to_string();
            let channel = words.nth(1).unwrap_or_default().to_string();
            if name.is_empty() || version.is_empty() || channel.is_empty() {
                return Err(format!("unparseable selection line: `{line}`"));
            }
            Ok(CheckOutcome::Selected(Available {
                name,
                version,
                channel,
            }))
        }
        Some(2) => Ok(CheckOutcome::NoneCompatible),
        code => Err(exit_reason("check", code, &output.stderr)),
    }
}

/// Parse `rauc-update fetch` output: the last stdout line of a successful
/// fetch is the verified local bundle path — the one path this module will
/// ever record as `ready`.
pub fn parse_fetch(output: &ClientOutput) -> Result<Option<String>, String> {
    match output.code {
        Some(0) => match output.stdout.lines().last() {
            Some(path) if path.starts_with('/') => Ok(Some(path.to_string())),
            other => Err(format!(
                "fetch exited 0 without a bundle path as its last line, got {other:?}"
            )),
        },
        Some(2) => Ok(None),
        code => Err(exit_reason("fetch", code, &output.stderr)),
    }
}

fn exit_reason(verb: &str, code: Option<i32>, stderr: &str) -> String {
    let reason = stderr.lines().last().unwrap_or("").trim();
    match code {
        Some(code) if !reason.is_empty() => format!("{verb} failed (exit {code}): {reason}"),
        Some(code) => format!("{verb} failed with exit {code}"),
        None if !reason.is_empty() => format!("{verb} was killed by a signal: {reason}"),
        None => format!("{verb} was killed by a signal"),
    }
}

/// Boot-derived lifecycle phases; see [`derive_boot_phase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootPhase {
    RebootRequired,
    Validating,
    Succeeded,
    RolledBack,
}

impl BootPhase {
    fn name(self) -> &'static str {
        match self {
            Self::RebootRequired => "reboot-required",
            Self::Validating => "validating",
            Self::Succeeded => "succeeded",
            Self::RolledBack => "rolled-back",
        }
    }
}

/// Derive the boot-related lifecycle phase from slot status, the primary
/// slot and the health gate's boot report, in strength order:
///
/// - **rolled-back**: a slot we are NOT running has `boot-status: bad` — its
///   boot attempts are exhausted, which is exactly what an automatic fallback
///   leaves behind.
/// - **reboot-required**: the bootloader's first pick is not the booted slot
///   (an installed-and-activated bundle is waiting for its first boot).
/// - **validating** / **succeeded**: only distinguishable when the boot
///   health gate has reported into the live-state `health.boot` entry —
///   `ok` is a confirmed boot, anything else is a boot still under judgement.
///   RAUC's own `boot-status` cannot carry this: the U-Boot backend reads the
///   attempt counter as exhausted-or-not, the same stated limit as
///   [`crate::rauc::pending_not_confirmed`]. Today's `mos-health` does not
///   report that entry yet (recorded as owed in `docs/design/updates.md`), so
///   until it does a healthy converged system reads as plain `None` — honest
///   silence rather than a guessed `succeeded`.
///
/// `None` when the slots say nothing update-related is going on.
pub fn derive_boot_phase(
    slots: &[SlotStatus],
    primary: Option<&str>,
    boot_health: Option<&str>,
) -> Option<(BootPhase, String)> {
    if let Some(fallen) = slots.iter().find(|slot| {
        slot.state.as_deref() != Some("booted") && slot.boot_status.as_deref() == Some("bad")
    }) {
        return Some((
            BootPhase::RolledBack,
            format!(
                "slot {} exhausted its boot attempts; the system fell back to {}",
                fallen.name,
                crate::rauc::booted_slot(slots).map_or("the running slot", |slot| &slot.name)
            ),
        ));
    }
    if crate::rauc::pending_not_confirmed(slots, primary) {
        let primary = primary.unwrap_or("(unknown)");
        return Some((
            BootPhase::RebootRequired,
            format!("slot {primary} is installed and activated; reboot to run it"),
        ));
    }
    match boot_health {
        Some("ok") => Some((
            BootPhase::Succeeded,
            "the boot health gate confirmed this boot".to_string(),
        )),
        Some(status) => Some((
            BootPhase::Validating,
            format!("the boot health gate reports `{status}`; this boot is not confirmed"),
        )),
        None => None,
    }
}

/// An active administrative override of the reboot gate.
#[derive(Debug, Clone)]
struct OverrideRecord {
    expires_at: DateTime<Utc>,
    requested_by: String,
}

/// The machine the mutex guards: every recorded fact that is not read live.
#[derive(Default)]
struct Machine {
    /// `Some("checking")` / `Some("downloading")` while a client subprocess
    /// runs; the busy guard and the reported state in one field.
    operation: Option<&'static str>,
    /// Why the last operation failed; cleared when the next one starts.
    failed: Option<String>,
    available: Option<Available>,
    /// The verified bundle path the last fetch staged.
    bundle: Option<String>,
    last_check: Option<String>,
    boot_phase: Option<(BootPhase, String)>,
    override_record: Option<OverrideRecord>,
}

/// The lifecycle service: one per daemon, shared with the auto-check task.
pub struct UpdateLifecycle {
    client: Arc<dyn UpdateClient>,
    policy: PolicyStore,
    host: Arc<dyn LifecycleHost>,
    /// The bus layer's install-in-flight flag, shared so `installing` here
    /// and the install refusal there can never disagree.
    installing: Arc<AtomicBool>,
    machine: Mutex<Machine>,
}

impl UpdateLifecycle {
    pub fn new(
        client: Arc<dyn UpdateClient>,
        policy: PolicyStore,
        host: Arc<dyn LifecycleHost>,
        installing: Arc<AtomicBool>,
    ) -> Self {
        Self {
            client,
            policy,
            host,
            installing,
            machine: Mutex::new(Machine::default()),
        }
    }

    /// Start a metadata check (`sync` when a URL is configured, then
    /// `check`). Returns as soon as the work is handed to a background task;
    /// progress and the outcome land in `update.lifecycle`.
    pub async fn request_check(self: &Arc<Self>, sender: &str) -> Result<(), Refusal> {
        let loaded = self.policy.load();
        if let Some(reason) = update_policy::check_refusal(&loaded) {
            self.record_refusal("check", &reason).await;
            return Err(Refusal::Policy(reason));
        }
        if let Some(reason) = self.client.unavailable() {
            self.record_refusal("check", &reason).await;
            return Err(Refusal::Unavailable(reason));
        }
        self.begin("checking").await?;
        tracing::info!(sender, "update check requested");
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let result = this.run_check(&loaded.policy).await;
            let mut machine = this.machine.lock().await;
            machine.operation = None;
            machine.last_check = Some(now_rfc3339());
            match result {
                Ok(CheckOutcome::Selected(available)) => {
                    tracing::info!(
                        name = %available.name,
                        version = %available.version,
                        "update check selected a candidate"
                    );
                    machine.available = Some(available);
                }
                Ok(CheckOutcome::NoneCompatible) => {
                    tracing::info!("update check found no compatible target");
                    machine.available = None;
                }
                Err(reason) => {
                    tracing::warn!(reason, "update check failed");
                    machine.failed = Some(reason);
                }
            }
            drop(machine);
            this.record_snapshot().await;
        });
        Ok(())
    }

    /// Start a bundle fetch. Selection happens inside `rauc-update fetch`
    /// itself, so a fetch does not require a prior check; on success the
    /// verified bundle path is recorded and the state becomes `ready`.
    pub async fn request_fetch(self: &Arc<Self>, sender: &str) -> Result<(), Refusal> {
        let loaded = self.policy.load();
        if let Some(reason) = update_policy::fetch_refusal(&loaded) {
            self.record_refusal("fetch", &reason).await;
            return Err(Refusal::Policy(reason));
        }
        if let Some(reason) = self.client.unavailable() {
            self.record_refusal("fetch", &reason).await;
            return Err(Refusal::Unavailable(reason));
        }
        self.begin("downloading").await?;
        tracing::info!(sender, "update fetch requested");
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let result = this.run_fetch(&loaded.policy).await;
            let mut machine = this.machine.lock().await;
            machine.operation = None;
            match result {
                Ok(Some(path)) => {
                    tracing::info!(bundle = %path, "update fetch staged a verified bundle");
                    machine.bundle = Some(path);
                }
                Ok(None) => {
                    tracing::info!("update fetch found no compatible target");
                    machine.available = None;
                }
                Err(reason) => {
                    tracing::warn!(reason, "update fetch failed");
                    machine.failed = Some(reason);
                }
            }
            drop(machine);
            this.record_snapshot().await;
        });
        Ok(())
    }

    /// Take the busy slot for `operation`, refusing when anything runs.
    async fn begin(&self, operation: &'static str) -> Result<(), Refusal> {
        if self.installing.load(Ordering::Acquire) {
            return Err(Refusal::Busy(
                "an update install is running; query GetUpdateState and retry".to_string(),
            ));
        }
        let mut machine = self.machine.lock().await;
        if let Some(running) = machine.operation {
            return Err(Refusal::Busy(format!(
                "an update operation is already {running}; query GetUpdateState and retry"
            )));
        }
        machine.operation = Some(operation);
        machine.failed = None;
        drop(machine);
        self.record_snapshot().await;
        Ok(())
    }

    async fn run_check(&self, policy: &UpdatePolicy) -> Result<CheckOutcome, String> {
        let source = &policy.source;
        if let Some(url) = &source.url {
            let sync_args = vec![
                "sync".to_string(),
                "--url".to_string(),
                url.clone(),
                "--repo".to_string(),
                source.repo_dir.clone(),
            ];
            let output = self
                .client
                .run(&sync_args, CHECK_TIMEOUT)
                .await
                .map_err(|err| format!("sync: {err:#}"))?;
            if output.code != Some(0) {
                return Err(exit_reason("sync", output.code, &output.stderr));
            }
        }
        let output = self
            .client
            .run(&check_args(source), CHECK_TIMEOUT)
            .await
            .map_err(|err| format!("check: {err:#}"))?;
        parse_check(&output)
    }

    async fn run_fetch(&self, policy: &UpdatePolicy) -> Result<Option<String>, String> {
        let source = &policy.source;
        let Some(url) = &source.url else {
            // Unreachable through `request_fetch` (the policy refusal caught
            // it), kept as an error rather than a panic all the same.
            return Err("no update source configured".to_string());
        };
        let mut args = check_args(source);
        args[0] = "fetch".to_string();
        args.extend([
            "--url".to_string(),
            url.clone(),
            "--reserve-dir".to_string(),
            source.reserve_dir.clone(),
            "--max-bytes".to_string(),
            source.max_bytes.to_string(),
        ]);
        let output = self
            .client
            .run(&args, FETCH_TIMEOUT)
            .await
            .map_err(|err| format!("fetch: {err:#}"))?;
        parse_fetch(&output)
    }

    /// Refresh the boot-derived phase from a fresh slot query and re-record.
    /// Called by the bus layer's `GetUpdateState` so the lifecycle entry is
    /// exactly as new as the slot facts beside it.
    pub async fn refresh(&self, slots: &[SlotStatus], primary: Option<&str>) {
        let health = self.host.health().await;
        let boot_health = health
            .get("boot")
            .and_then(|entry| entry.get("status"))
            .and_then(Value::as_str);
        let phase = derive_boot_phase(slots, primary, boot_health);
        self.machine.lock().await.boot_phase = phase;
        self.record_snapshot().await;
    }

    /// Why the machine must not reboot right now, or `None` when it may.
    /// The bus layer's `Reboot` asks this before it asks systemd.
    pub async fn reboot_refusal(&self) -> Option<String> {
        let verdict = self.gate_verdict().await;
        if verdict.safe {
            return None;
        }
        Some(format!(
            "reboot refused by the safe-to-reboot gate: {}. \
             An administrator can lift a health block with SetRebootOverride \
             (POST /api/v1/update/reboot-override).",
            verdict.reasons.join("; ")
        ))
    }

    /// Why an install must not start right now (maintenance window / policy
    /// file), or `None`. Consulted by the bus layer's `InstallUpdate`.
    pub async fn install_refusal(&self) -> Option<String> {
        update_policy::install_refusal(&self.policy.load(), Utc::now())
    }

    /// Arm the administrative reboot-gate override for `seconds`, bounded by
    /// the policy's ceiling. Answers the recorded override as JSON.
    pub async fn set_reboot_override(&self, sender: &str, seconds: u64) -> Result<Value, Refusal> {
        let loaded = self.policy.load();
        let ceiling = loaded.policy.reboot_gate.override_ceiling();
        if seconds == 0 {
            return Err(Refusal::Invalid(
                "override TTL must be at least 1 second".to_string(),
            ));
        }
        if seconds > ceiling {
            return Err(Refusal::Invalid(format!(
                "override TTL must be at most {ceiling} seconds"
            )));
        }
        let record = OverrideRecord {
            expires_at: Utc::now() + chrono::Duration::seconds(seconds as i64),
            requested_by: sender.to_string(),
        };
        // The one log line that must exist for the audit trail: who disarmed
        // the gate, for how long. apid records its own audit event beside it.
        tracing::warn!(
            sender,
            seconds,
            until = %record.expires_at.to_rfc3339_opts(SecondsFormat::Secs, true),
            "safe-to-reboot gate override armed"
        );
        let rendered = json!({
            "until": record.expires_at.to_rfc3339_opts(SecondsFormat::Secs, true),
            "requestedBy": record.requested_by,
        });
        self.machine.lock().await.override_record = Some(record);
        self.record_snapshot().await;
        Ok(rendered)
    }

    /// The gate verdict with the current health tree, install flag and
    /// (expired-pruned) override.
    async fn gate_verdict(&self) -> GateVerdict {
        let loaded = self.policy.load();
        let health = self.host.health().await;
        let installing = self.installing.load(Ordering::Acquire);
        let override_active = {
            let mut machine = self.machine.lock().await;
            prune_override(&mut machine.override_record);
            machine.override_record.is_some()
        };
        update_policy::evaluate_gate(
            &loaded.policy.reboot_gate,
            &health,
            installing,
            override_active,
        )
    }

    /// Record a refused action without disturbing the machine: the refusal is
    /// the newest fact an operator polling the state needs to see.
    async fn record_refusal(&self, action: &str, reason: &str) {
        tracing::warn!(action, reason, "update action refused by policy");
        self.record_snapshot_with(Some(format!("{action} refused: {reason}")))
            .await;
    }

    async fn record_snapshot(&self) {
        self.record_snapshot_with(None).await;
    }

    /// Build the full `update.lifecycle` entry and hand it to the host.
    async fn record_snapshot_with(&self, last_refusal: Option<String>) {
        let loaded = self.policy.load();
        let health = self.host.health().await;
        let installing = self.installing.load(Ordering::Acquire);
        let mut machine = self.machine.lock().await;
        prune_override(&mut machine.override_record);
        let verdict = update_policy::evaluate_gate(
            &loaded.policy.reboot_gate,
            &health,
            installing,
            machine.override_record.is_some(),
        );
        let entry = render_entry(
            &machine,
            &loaded,
            &verdict,
            installing,
            self.client.unavailable(),
            self.policy.path(),
            last_refusal,
        );
        drop(machine);
        self.host.record(entry).await;
    }
}

/// Drop an override whose TTL has passed.
fn prune_override(record: &mut Option<OverrideRecord>) {
    if record
        .as_ref()
        .is_some_and(|active| active.expires_at <= Utc::now())
    {
        *record = None;
    }
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The `check` argument vector; `fetch` extends it.
fn check_args(source: &crate::update_policy::SourcePolicy) -> Vec<String> {
    vec![
        "check".to_string(),
        "--repo".to_string(),
        source.repo_dir.clone(),
        "--root".to_string(),
        source.root_path.clone(),
        "--state".to_string(),
        source.state_path.clone(),
        "--channel".to_string(),
        source.channel.clone(),
    ]
}

/// The one place the recorded entry is shaped, so the state precedence —
/// installing over a running client operation over a failure over a staged
/// bundle over the boot-derived phase over idle — is written once.
fn render_entry(
    machine: &Machine,
    loaded: &LoadedPolicy,
    gate: &GateVerdict,
    installing: bool,
    client_unavailable: Option<String>,
    policy_path: Option<&std::path::Path>,
    last_refusal: Option<String>,
) -> Value {
    let (state, reason): (&str, Option<String>) = if installing {
        ("installing", None)
    } else if let Some(operation) = machine.operation {
        (operation, None)
    } else if let Some(failed) = &machine.failed {
        ("failed", Some(failed.clone()))
    } else if machine.bundle.is_some() {
        (
            "ready",
            Some("a verified bundle is staged for install".to_string()),
        )
    } else if let Some((phase, why)) = &machine.boot_phase {
        (phase.name(), Some(why.clone()))
    } else {
        ("idle", None)
    };
    let mut entry = serde_json::Map::new();
    entry.insert("state".into(), json!(state));
    if let Some(reason) = reason {
        entry.insert("reason".into(), json!(reason));
    }
    if let Some(refusal) = last_refusal {
        entry.insert("last_refusal".into(), json!(refusal));
    }
    if let Some(available) = &machine.available {
        entry.insert(
            "available".into(),
            json!({
                "name": available.name,
                "version": available.version,
                "channel": available.channel,
            }),
        );
    }
    if let Some(bundle) = &machine.bundle {
        entry.insert("bundle".into(), json!(bundle));
    }
    if let Some(last_check) = &machine.last_check {
        entry.insert("last_check".into(), json!(last_check));
    }
    entry.insert(
        "client".into(),
        match &client_unavailable {
            None => json!({ "available": true }),
            Some(reason) => json!({ "available": false, "reason": reason }),
        },
    );
    let policy = &loaded.policy;
    let windows: Vec<Value> = policy
        .maintenance
        .windows
        .iter()
        .map(|window| {
            json!({
                "days": window.days,
                "start": window.start,
                "end": window.end,
            })
        })
        .collect();
    entry.insert(
        "policy".into(),
        json!({
            "file": policy_path.map(|path| path.display().to_string()),
            "networkMode": match policy.network.mode {
                crate::update_policy::NetworkMode::Online => "online",
                crate::update_policy::NetworkMode::Metered => "metered",
                crate::update_policy::NetworkMode::Offline => "offline",
            },
            "meteredAllowsFetch": policy.network.metered_allows_fetch,
            "sourceUrl": policy.source.url,
            "channel": policy.source.channel,
            "autoCheckMinutes": policy.auto_check.interval_minutes,
            "maintenanceWindows": windows,
            "installAllowedNow": update_policy::install_refusal(loaded, Utc::now()).is_none(),
            "blockingStatuses": policy.reboot_gate.blocking_statuses,
            "overrideMaxSeconds": policy.reboot_gate.override_ceiling(),
        }),
    );
    if let Some(error) = &loaded.error {
        entry.insert("policy_error".into(), json!(error));
    }
    let mut gate_entry = gate.to_json();
    if let (Some(record), Some(map)) = (&machine.override_record, gate_entry.as_object_mut()) {
        map.insert(
            "override".into(),
            json!({
                "until": record.expires_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                "requestedBy": record.requested_by,
            }),
        );
    }
    entry.insert("reboot_gate".into(), gate_entry);
    Value::Object(entry)
}

/// The automatic check cadence: sleep the policy's interval, then run a
/// check when policy and client allow it. The interval is re-read every
/// turn, so an operator edit takes effect without a restart; a disabled
/// cadence (`0`) is re-polled every five minutes rather than never again.
///
/// Checks only. Nothing is fetched and nothing is installed automatically —
/// downloads and installs stay operator actions gated by their own policies.
pub async fn auto_check_loop(lifecycle: Arc<UpdateLifecycle>) {
    const DISABLED_POLL: Duration = Duration::from_secs(300);
    loop {
        let interval = lifecycle.policy.load().policy.auto_check.interval_minutes;
        if interval == 0 {
            tokio::time::sleep(DISABLED_POLL).await;
            continue;
        }
        tokio::time::sleep(Duration::from_secs(interval * 60)).await;
        if lifecycle.policy.load().policy.auto_check.interval_minutes == 0 {
            continue;
        }
        if let Err(refusal) = lifecycle.request_check("auto-check").await {
            tracing::debug!(reason = refusal.message(), "auto-check skipped");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use chrono::{Datelike, Timelike};

    use super::*;

    fn output(code: i32, stdout: &str, stderr: &str) -> ClientOutput {
        ClientOutput {
            code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    fn slot(name: &str, state: &str, boot_status: Option<&str>) -> SlotStatus {
        SlotStatus {
            name: name.to_string(),
            state: Some(state.to_string()),
            boot_status: boot_status.map(str::to_string),
            ..SlotStatus::default()
        }
    }

    #[test]
    fn check_output_parses_by_its_printed_contract() {
        let selected = output(
            0,
            "rejected old-1.raucb: version 0.9 is not newer\n\
             selected update-1.1.0.raucb version 1.1.0 channel stable (1234 bytes)\n",
            "",
        );
        assert_eq!(
            parse_check(&selected),
            Ok(CheckOutcome::Selected(Available {
                name: "update-1.1.0.raucb".to_string(),
                version: "1.1.0".to_string(),
                channel: "stable".to_string(),
            }))
        );
        assert_eq!(
            parse_check(&output(2, "none\n", "")),
            Ok(CheckOutcome::NoneCompatible)
        );
        let failed = parse_check(&output(1, "", "rauc-update: state file corrupt\n"))
            .expect_err("exit 1 is a failure");
        assert!(failed.contains("state file corrupt"), "got: {failed}");
        let unparseable =
            parse_check(&output(0, "something else\n", "")).expect_err("no selected line");
        assert!(unparseable.contains("selected"), "got: {unparseable}");
    }

    #[test]
    fn fetch_output_is_the_last_stdout_line_and_nothing_else() {
        let staged = output(
            0,
            "selected update-1.1.0.raucb version 1.1.0 channel stable (12 bytes)\n\
             /var/lib/mos/update/reserve/abc.update-1.1.0.raucb\n",
            "",
        );
        assert_eq!(
            parse_fetch(&staged),
            Ok(Some(
                "/var/lib/mos/update/reserve/abc.update-1.1.0.raucb".to_string()
            ))
        );
        assert_eq!(parse_fetch(&output(2, "none\n", "")), Ok(None));
        assert!(
            parse_fetch(&output(0, "not-a-path\n", "")).is_err(),
            "a relative last line must not be recorded as a bundle"
        );
        assert!(parse_fetch(&output(1, "", "budget exceeded\n")).is_err());
    }

    #[test]
    fn boot_phase_derivation_covers_every_derived_state() {
        let converged = [
            slot("rootfs.0", "booted", Some("good")),
            slot("rootfs.1", "inactive", Some("good")),
        ];
        // Converged and unreported: honest silence, not a guessed success.
        assert_eq!(derive_boot_phase(&converged, Some("rootfs.0"), None), None);
        // Installed and activated, not yet booted.
        let (phase, why) =
            derive_boot_phase(&converged, Some("rootfs.1"), None).expect("pending derives");
        assert_eq!(phase, BootPhase::RebootRequired);
        assert!(why.contains("rootfs.1"), "got: {why}");
        // The other slot exhausted its attempts: automatic fallback.
        let fallen = [
            slot("rootfs.0", "booted", Some("good")),
            slot("rootfs.1", "inactive", Some("bad")),
        ];
        let (phase, why) =
            derive_boot_phase(&fallen, Some("rootfs.1"), None).expect("fallback derives");
        assert_eq!(phase, BootPhase::RolledBack);
        assert!(
            why.contains("rootfs.1") && why.contains("rootfs.0"),
            "got: {why}"
        );
        // The health gate's report is what splits validating from succeeded.
        let (phase, _) = derive_boot_phase(&converged, Some("rootfs.0"), Some("ok"))
            .expect("a confirmed boot derives");
        assert_eq!(phase, BootPhase::Succeeded);
        let (phase, why) = derive_boot_phase(&converged, Some("rootfs.0"), Some("probing"))
            .expect("an unconfirmed boot derives");
        assert_eq!(phase, BootPhase::Validating);
        assert!(why.contains("probing"), "got: {why}");
    }

    /// Scripted client: each expected invocation is `(leading subcommand,
    /// result)`, consumed in order; the full argv of every call is logged.
    struct MockClient {
        script: StdMutex<Vec<(String, Result<ClientOutput, String>)>>,
        calls: Arc<StdMutex<Vec<Vec<String>>>>,
        unavailable: Option<String>,
    }

    impl MockClient {
        fn new(script: Vec<(&str, Result<ClientOutput, String>)>) -> Self {
            Self {
                script: StdMutex::new(
                    script
                        .into_iter()
                        .map(|(verb, result)| (verb.to_string(), result))
                        .collect(),
                ),
                calls: Arc::new(StdMutex::new(Vec::new())),
                unavailable: None,
            }
        }

        fn absent(reason: &str) -> Self {
            Self {
                script: StdMutex::new(Vec::new()),
                calls: Arc::new(StdMutex::new(Vec::new())),
                unavailable: Some(reason.to_string()),
            }
        }
    }

    #[async_trait::async_trait]
    impl UpdateClient for MockClient {
        fn unavailable(&self) -> Option<String> {
            self.unavailable.clone()
        }

        async fn run(&self, args: &[String], _timeout: Duration) -> Result<ClientOutput> {
            self.calls.lock().expect("calls").push(args.to_vec());
            let mut script = self.script.lock().expect("script");
            anyhow::ensure!(!script.is_empty(), "unexpected client call: {args:?}");
            let (verb, result) = script.remove(0);
            anyhow::ensure!(
                args.first() == Some(&verb),
                "expected `{verb}`, got {args:?}"
            );
            result.map_err(|reason| anyhow::anyhow!("{reason}"))
        }
    }

    /// Host recording every `update.lifecycle` write and serving a health
    /// tree the test controls.
    struct TestHost {
        recorded: StdMutex<Vec<Value>>,
        health: StdMutex<Value>,
    }

    impl TestHost {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                recorded: StdMutex::new(Vec::new()),
                health: StdMutex::new(json!({})),
            })
        }

        fn last(&self) -> Value {
            self.recorded
                .lock()
                .expect("recorded")
                .last()
                .cloned()
                .expect("at least one record")
        }

        fn set_health(&self, health: Value) {
            *self.health.lock().expect("health") = health;
        }
    }

    #[async_trait::async_trait]
    impl LifecycleHost for TestHost {
        async fn record(&self, lifecycle: Value) {
            self.recorded.lock().expect("recorded").push(lifecycle);
        }

        async fn health(&self) -> Value {
            self.health.lock().expect("health").clone()
        }
    }

    fn policy_file(dir: &tempfile::TempDir, body: &str) -> PolicyStore {
        let path = dir.path().join("update-policy.toml");
        std::fs::write(&path, body).expect("seed policy");
        PolicyStore::at(path)
    }

    fn lifecycle(
        client: MockClient,
        policy: PolicyStore,
        host: Arc<TestHost>,
    ) -> (Arc<UpdateLifecycle>, Arc<AtomicBool>) {
        let installing = Arc::new(AtomicBool::new(false));
        (
            Arc::new(UpdateLifecycle::new(
                Arc::new(client),
                policy,
                host,
                Arc::clone(&installing),
            )),
            installing,
        )
    }

    /// Poll the host until the recorded state leaves `busy` states.
    async fn settled(host: &TestHost) -> Value {
        for _ in 0..500 {
            let last = host.last();
            let state = last["state"].as_str().unwrap_or_default();
            if state != "checking" && state != "downloading" {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("lifecycle never settled: {}", host.last());
    }

    #[tokio::test]
    async fn a_check_syncs_then_checks_and_records_the_selection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, "[source]\nurl = \"http://mirror/tuf\"\n");
        let client = MockClient::new(vec![
            ("sync", Ok(output(0, "synced root v1 ...\n", ""))),
            (
                "check",
                Ok(output(
                    0,
                    "selected update-1.1.0.raucb version 1.1.0 channel stable (9 bytes)\n",
                    "",
                )),
            ),
        ]);
        let calls = Arc::clone(&client.calls);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));

        lifecycle.request_check("test").await.expect("accepted");
        let recorded = settled(&host).await;
        assert_eq!(recorded["state"], "idle");
        assert_eq!(recorded["available"]["version"], "1.1.0");
        assert!(recorded["last_check"].is_string());
        assert_eq!(recorded["client"]["available"], true);

        let calls = calls.lock().expect("calls").clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0][0], "sync");
        assert!(calls[0].contains(&"http://mirror/tuf".to_string()));
        assert_eq!(calls[1][0], "check");
        assert!(
            calls[1].contains(&"--channel".to_string()) && calls[1].contains(&"stable".to_string()),
            "check must carry the policy channel: {calls:?}"
        );
    }

    #[tokio::test]
    async fn a_fetch_records_the_verified_path_as_ready() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, "[source]\nurl = \"http://mirror/tuf\"\n");
        let client = MockClient::new(vec![(
            "fetch",
            Ok(output(
                0,
                "selected x version 1 channel stable (9 bytes)\n/data/x.raucb\n",
                "",
            )),
        )]);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));

        lifecycle.request_fetch("test").await.expect("accepted");
        let recorded = settled(&host).await;
        assert_eq!(recorded["state"], "ready");
        assert_eq!(recorded["bundle"], "/data/x.raucb");
    }

    #[tokio::test]
    async fn a_failed_check_is_a_failed_state_with_its_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, "[source]\nurl = \"http://mirror/tuf\"\n");
        let client = MockClient::new(vec![(
            "sync",
            Ok(output(1, "", "rauc-update: connection refused\n")),
        )]);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));

        lifecycle.request_check("test").await.expect("accepted");
        let recorded = settled(&host).await;
        assert_eq!(recorded["state"], "failed");
        assert!(
            recorded["reason"]
                .as_str()
                .expect("reason")
                .contains("connection refused"),
            "got: {recorded}"
        );
    }

    #[tokio::test]
    async fn every_policy_refusal_is_refused_and_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Offline: both verbs refused, nothing ever reaches the client.
        let policy = policy_file(
            &dir,
            "[source]\nurl = \"http://mirror/tuf\"\n[network]\nmode = \"offline\"\n",
        );
        let client = MockClient::new(vec![]);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
        let refusal = lifecycle.request_check("test").await.expect_err("refused");
        assert!(matches!(refusal, Refusal::Policy(_)), "got {refusal:?}");
        assert!(refusal.message().contains("offline"));
        let refusal = lifecycle.request_fetch("test").await.expect_err("refused");
        assert!(matches!(refusal, Refusal::Policy(_)));
        let recorded = host.last();
        assert!(
            recorded["last_refusal"]
                .as_str()
                .expect("refusal recorded")
                .contains("offline"),
            "got: {recorded}"
        );
        assert_eq!(recorded["policy"]["networkMode"], "offline");
    }

    #[tokio::test]
    async fn metered_mode_refuses_fetch_but_admits_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(
            &dir,
            "[source]\nurl = \"http://mirror/tuf\"\n[network]\nmode = \"metered\"\n",
        );
        let client = MockClient::new(vec![
            ("sync", Ok(output(0, "", ""))),
            ("check", Ok(output(2, "none\n", ""))),
        ]);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
        let refusal = lifecycle.request_fetch("test").await.expect_err("refused");
        assert!(refusal.message().contains("metered"));
        lifecycle
            .request_check("test")
            .await
            .expect("check admitted");
        let recorded = settled(&host).await;
        assert_eq!(recorded["state"], "idle");
        assert!(recorded.get("available").is_none(), "none selected");
    }

    #[tokio::test]
    async fn an_absent_client_is_reported_never_panicked_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, "[source]\nurl = \"http://mirror/tuf\"\n");
        let client = MockClient::absent("/usr/bin/rauc-update is not present on this image");
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
        let refusal = lifecycle.request_check("test").await.expect_err("refused");
        assert!(matches!(refusal, Refusal::Unavailable(_)));
        let recorded = host.last();
        assert_eq!(recorded["client"]["available"], false);
        assert!(
            recorded["client"]["reason"]
                .as_str()
                .expect("reason")
                .contains("not present"),
        );
    }

    #[tokio::test]
    async fn a_second_operation_is_refused_while_one_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, "[source]\nurl = \"http://mirror/tuf\"\n");
        // The sync never answers within the test: an Err after a long sleep
        // would leak; instead gate on a channel-free trick — a script entry
        // that sleeps far longer than the test's second request needs.
        struct SlowClient;
        #[async_trait::async_trait]
        impl UpdateClient for SlowClient {
            fn unavailable(&self) -> Option<String> {
                None
            }
            async fn run(&self, _args: &[String], _timeout: Duration) -> Result<ClientOutput> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                anyhow::bail!("never reached")
            }
        }
        let installing = Arc::new(AtomicBool::new(false));
        let host = TestHost::new();
        let lifecycle = Arc::new(UpdateLifecycle::new(
            Arc::new(SlowClient),
            policy,
            Arc::clone(&host) as Arc<dyn LifecycleHost>,
            Arc::clone(&installing),
        ));
        lifecycle
            .request_check("test")
            .await
            .expect("first accepted");
        let refusal = lifecycle.request_fetch("test").await.expect_err("busy");
        assert!(matches!(refusal, Refusal::Busy(_)));
        assert!(refusal.message().contains("checking"));
        // An install in flight refuses new client operations the same way.
        installing.store(true, Ordering::Release);
        let refusal = lifecycle.request_check("test").await.expect_err("busy");
        assert!(refusal.message().contains("install"));
    }

    #[tokio::test]
    async fn the_reboot_gate_blocks_lifts_and_expires() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, "");
        let client = MockClient::new(vec![]);
        let host = TestHost::new();
        let (lifecycle, installing) = lifecycle(client, policy, Arc::clone(&host));

        // Open by default.
        assert_eq!(lifecycle.reboot_refusal().await, None);

        // A blocking health report closes it.
        host.set_health(json!({
            "exporter": { "status": "blocking", "detail": "mid-transaction" }
        }));
        let refusal = lifecycle.reboot_refusal().await.expect("closed");
        assert!(refusal.contains("exporter"), "got: {refusal}");
        assert!(refusal.contains("SetRebootOverride"), "got: {refusal}");

        // A bounded override lifts the health block…
        assert!(matches!(
            lifecycle.set_reboot_override("op", 0).await,
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            lifecycle
                .set_reboot_override("op", update_policy::OVERRIDE_CEILING_SECONDS + 1)
                .await,
            Err(Refusal::Invalid(_))
        ));
        let rendered = lifecycle
            .set_reboot_override(":1.42", 600)
            .await
            .expect("armed");
        assert_eq!(rendered["requestedBy"], ":1.42");
        assert_eq!(lifecycle.reboot_refusal().await, None);
        let recorded = host.last();
        assert_eq!(recorded["reboot_gate"]["safe"], true);
        assert_eq!(recorded["reboot_gate"]["overridden"], true);
        assert!(recorded["reboot_gate"]["override"]["until"].is_string());

        // …but never an install in flight.
        installing.store(true, Ordering::Release);
        let refusal = lifecycle.reboot_refusal().await.expect("install blocks");
        assert!(refusal.contains("install"), "got: {refusal}");
        installing.store(false, Ordering::Release);

        // An expired override is pruned, closing the gate again.
        lifecycle.machine.lock().await.override_record = Some(OverrideRecord {
            expires_at: Utc::now() - chrono::Duration::seconds(1),
            requested_by: "op".to_string(),
        });
        assert!(lifecycle.reboot_refusal().await.is_some());
    }

    #[tokio::test]
    async fn refresh_derives_the_boot_phase_from_fresh_slots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, "");
        let client = MockClient::new(vec![]);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));

        let slots = [
            slot("rootfs.0", "booted", Some("good")),
            slot("rootfs.1", "inactive", Some("good")),
        ];
        lifecycle.refresh(&slots, Some("rootfs.1")).await;
        assert_eq!(host.last()["state"], "reboot-required");

        // The health gate's boot report flips the derived state.
        host.set_health(json!({ "boot": { "status": "ok", "detail": "" } }));
        lifecycle.refresh(&slots, Some("rootfs.0")).await;
        assert_eq!(host.last()["state"], "succeeded");
    }

    #[tokio::test]
    async fn an_install_refusal_follows_the_maintenance_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A window that can never contain "now": zero-width windows do not
        // exist (equal start/end wraps to 24 h), so pin an impossible day
        // combination instead — Monday 00:00-00:01 leaves 10079 refused
        // minutes a week; rather than race the clock, assert both sides
        // through the pure function and only the wiring here.
        let policy = policy_file(
            &dir,
            "[[maintenance.windows]]\ndays = [\"mon\"]\nstart = \"00:00\"\nend = \"00:01\"\n",
        );
        let client = MockClient::new(vec![]);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
        let now = Utc::now();
        let inside =
            now.weekday().num_days_from_monday() == 0 && now.hour() == 0 && now.minute() == 0;
        let refusal = lifecycle.install_refusal().await;
        if inside {
            assert_eq!(refusal, None);
        } else {
            assert!(refusal.expect("outside the window").contains("maintenance"),);
        }
    }

    #[tokio::test]
    async fn the_subprocess_client_runs_a_real_binary_and_reports_absence() {
        // A stub standing in for rauc-update, exercising the spawn, the
        // capture and the exit-code path — the parts a mock cannot vouch for.
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("rauc-update");
        std::fs::write(
            &stub,
            "#!/bin/sh\n\
             case \"$1\" in\n\
             check) echo 'selected u.raucb version 2.0 channel stable (5 bytes)'; exit 0 ;;\n\
             fetch) echo /data/u.raucb; exit 0 ;;\n\
             none) echo none; exit 2 ;;\n\
             *) echo 'boom' >&2; exit 1 ;;\n\
             esac\n",
        )
        .expect("write stub");
        let mut permissions = std::fs::metadata(&stub).expect("stat").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&stub, permissions).expect("chmod");

        let client = SubprocessClient::new(stub.clone());
        assert_eq!(client.unavailable(), None);
        let run = |arg: &str| {
            let args = vec![arg.to_string()];
            let client = SubprocessClient::new(stub.clone());
            async move { client.run(&args, Duration::from_secs(10)).await }
        };
        let checked = run("check").await.expect("check runs");
        assert_eq!(
            parse_check(&checked).expect("parses"),
            CheckOutcome::Selected(Available {
                name: "u.raucb".to_string(),
                version: "2.0".to_string(),
                channel: "stable".to_string(),
            })
        );
        let fetched = run("fetch").await.expect("fetch runs");
        assert_eq!(parse_fetch(&fetched), Ok(Some("/data/u.raucb".to_string())));
        let none = run("none")
            .await
            .expect("exit 2 is an output, not an error");
        assert_eq!(none.code, Some(2));
        let failed = run("explode").await.expect("exit 1 is an output too");
        assert_eq!(failed.code, Some(1));
        assert!(failed.stderr.contains("boom"));

        let absent = SubprocessClient::new(dir.path().join("gone"));
        assert!(
            absent
                .unavailable()
                .expect("absence reported")
                .contains("not present")
        );
    }
}

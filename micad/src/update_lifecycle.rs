//! Signed deployment acquisition, operator policy and boot lifecycle.
//! Native commands enforce trust, capacity and generation; this layer records
//! progress and applies the shared maintenance and reboot gates.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use micad_settings::configuration;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Mutex;

use crate::deployment::Status;
use crate::update_codes::{self, CodedReason};
use crate::update_policy::{
    self, EffectivePolicy, GateVerdict, LoadedPolicy, PolicyStore, Selection, Workspace,
};

/// Fixed native command shipped in the complete system image.
pub const DEFAULT_CLIENT_PATH: &str = "/usr/bin/mica-deploy";

/// The `/mos/updates` workspace the client acquires into (PLAN-061/063):
/// partials in `downloads/`, verified descriptors and objects in `verified/`. The client's
/// own default; restated here because this module decides what may be
/// recorded as `ready` and what may be installed.
pub const DEFAULT_WORKSPACE_ROOT: &str = "/mos/updates";

/// Bound on one `probe` subprocess: a handful of stat calls and one fsync;
/// a minute is a wedged disk, not a slow one.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bound on one `check` subprocess: metadata is a handful of files
/// capped at 1 MiB each, so ten minutes is generous slack for a slow link,
/// not an expected duration.
const CHECK_TIMEOUT: Duration = Duration::from_secs(600);

/// Bound on one `fetch` subprocess. Component payloads can be hundreds of MiB and the link
/// may be slow; four hours is a fault bound, and a fetch that dies there
/// resumes from its `.part` file on the next attempt.
const FETCH_TIMEOUT: Duration = Duration::from_secs(4 * 3600);

/// What one client invocation produced.
#[derive(Debug)]
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

/// Runs `mica-deploy` (or a stand-in) with an argument vector.
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

struct ClientProcessGroup(Option<rustix::process::Pid>);
impl Drop for ClientProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
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
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                anyhow::Error::new(ClientUnavailable(format!(
                    "{} is not present on this image",
                    self.binary.display()
                )))
            } else {
                anyhow::Error::from(err).context(format!("spawn {}", self.binary.display()))
            }
        })?;
        let group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
            .context("missing client process ID")?;
        let mut group = ClientProcessGroup(Some(group));
        let stdout = child.stdout.take().context("missing client stdout")?;
        let stderr = child.stderr.take().context("missing client stderr")?;
        let result = tokio::time::timeout(timeout, async {
            tokio::try_join!(
                read_client_output(stdout, 65536),
                read_client_output(stderr, 16384),
                async { Ok::<_, anyhow::Error>(child.wait().await?) }
            )
        })
        .await
        .context("update client exceeded its deadline")
        .and_then(|result| result);
        let (stdout, stderr, status) = match result {
            Ok(output) => output,
            Err(error) => {
                drop(group);
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(error);
            }
        };
        group.0 = None;
        Ok(ClientOutput {
            code: status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

async fn read_client_output(stream: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= limit, "update client output exceeds bound");
    Ok(bytes)
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
    pub deployment_id: String,
    pub version: String,
    pub channel: String,
}

/// What a finished `check` said.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    Selected(Available),
    /// The signed catalog selects no newer deployment for this device.
    NoneCompatible,
}

fn client_json(operation: &str, output: &ClientOutput) -> Result<Value, CodedReason> {
    if output.code != Some(0) {
        return Err(exit_reason(operation, output.code, &output.stderr));
    }
    serde_json::from_str(&output.stdout).map_err(|error| {
        CodedReason::new(
            update_codes::CLIENT_OUTPUT_UNPARSEABLE,
            format!("invalid {operation} JSON: {error}"),
        )
    })
}

/// The helper authenticates the catalog before returning its exact selection.
pub fn parse_check(output: &ClientOutput) -> Result<CheckOutcome, CodedReason> {
    let value = client_json("check", output)?;
    if value.get("selected") == Some(&Value::Null) {
        return Ok(CheckOutcome::NoneCompatible);
    }
    let parsed = (|| {
        let id = value.pointer("/selected/deploymentId")?.as_str()?;
        let version = value.pointer("/selected/deployment/version")?.as_str()?;
        let channel = value.get("channel")?.as_str()?;
        if !crate::deployment::valid_id(id)
            || version.is_empty()
            || !matches!(channel, "stable" | "beta" | "dev")
        {
            return None;
        }
        Some(Available {
            deployment_id: id.to_string(),
            version: version.into(),
            channel: channel.into(),
        })
    })();
    parsed.map(CheckOutcome::Selected).ok_or_else(|| {
        CodedReason::new(
            update_codes::CLIENT_OUTPUT_UNPARSEABLE,
            "check did not return a deployment selection",
        )
    })
}

/// What an awaited lifecycle operation did, in the four answers the recorded
/// state distinguishes.
///
/// `request_check` and `request_fetch` throw this away — an operator reads
/// the outcome back out of `update.lifecycle` — while the automatic driver
/// ([`crate::update_auto`]) has to decide what to do next, and deciding
/// needs the answer rather than the state string it renders to.
#[derive(Debug, PartialEq, Eq)]
pub enum Settled<T> {
    /// The operation produced its result: a selected candidate, a staged
    /// descriptor path.
    Done(T),
    /// Nothing published is compatible — the device is up to date, or the
    /// selected channel holds nothing newer than the running system.
    NoneCompatible,
    /// The `/mos/updates` workspace refused it before anything was acquired.
    Unready(Unready),
    /// It failed, with the same code and reason recorded beside the `failed`
    /// state.
    Failed(CodedReason),
}

/// Native workspace preflight refused acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unready {
    pub status: String,
    /// PLAN-076 B4: one of [`crate::update_codes`]'s five workspace codes, or
    /// `unknown` — never the word the client printed. `&'static str` is what
    /// makes that structural: this field cannot hold a client's string.
    pub kind: &'static str,
    pub detail: String,
}

impl Unready {
    /// The reason string recorded beside `update-unavailable`.
    pub fn reason(&self) -> String {
        format!("{} {}: {}", self.status, self.kind, self.detail)
    }
}

/// What a finished `probe` said.
#[derive(Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The workspace is ready; the bounded native JSON report is preserved.
    Ready(Value),
    Unready(Unready),
}

/// Parse the bounded native readiness report.
pub fn parse_probe(output: &ClientOutput) -> Result<ProbeOutcome, CodedReason> {
    if output.code != Some(0) {
        return Ok(ProbeOutcome::Unready(Unready {
            status: "degraded".into(),
            kind: update_codes::WORKSPACE_PROBE_FAILED,
            detail: output.stderr.trim().into(),
        }));
    }
    let value = client_json("probe", output)?;
    if value.get("status").and_then(Value::as_str) != Some("ready") {
        return Err(CodedReason::new(
            update_codes::CLIENT_OUTPUT_UNPARSEABLE,
            "probe did not report a ready workspace",
        ));
    }
    Ok(ProbeOutcome::Ready(value))
}

/// What a finished `fetch` said.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchOutcome {
    /// A verified descriptor at this path, inside `verified/`.
    Staged(String),
    /// No deployment was selected.
    NoneCompatible,
}

/// Accept only the descriptor and object directory returned by native acquisition.
pub fn parse_fetch(
    output: &ClientOutput,
    verified_dir: &Path,
) -> Result<FetchOutcome, CodedReason> {
    let value = client_json("fetch", output)?;
    if value.is_null() {
        return Ok(FetchOutcome::NoneCompatible);
    }
    let path = (|| {
        let id = value.get("id")?.as_str()?;
        let path = Path::new(value.get("path")?.as_str()?);
        let objects = Path::new(value.get("objects")?.as_str()?);
        if !crate::deployment::valid_id(id)
            || path != verified_dir.join(format!("{id}.json"))
            || objects != verified_dir.join("objects")
        {
            return None;
        }
        Some(path.to_string_lossy().into_owned())
    })();
    path.map(FetchOutcome::Staged).ok_or_else(|| {
        CodedReason::new(
            update_codes::UNVERIFIED_DEPLOYMENT_PATH,
            "fetch did not return a verified deployment descriptor",
        )
    })
}

/// How a client operation did not produce its outcome: the workspace refused
/// it (a state), or it failed (a reason).
enum Failure {
    Unready(Unready),
    Error(CodedReason),
}

/// A client invocation that ended badly, coded at the site.
///
/// The two codes are not the same fault and a fleet must be able to tell them
/// apart: an exit status is the client having run and judged something, while
/// a signal is the bound in this module (or the OOM killer) having stopped it
/// before it judged anything.
fn exit_reason(verb: &str, code: Option<i32>, stderr: &str) -> CodedReason {
    let reason = stderr.lines().last().unwrap_or("").trim();
    match code {
        Some(code) if !reason.is_empty() => CodedReason::new(
            update_codes::CLIENT_EXIT_FAILURE,
            format!("{verb} failed (exit {code}): {reason}"),
        ),
        Some(code) => CodedReason::new(
            update_codes::CLIENT_EXIT_FAILURE,
            format!("{verb} failed with exit {code}"),
        ),
        None if !reason.is_empty() => CodedReason::new(
            update_codes::CLIENT_SPAWN_FAILED,
            format!("{verb} was killed by a signal: {reason}"),
        ),
        None => CodedReason::new(
            update_codes::CLIENT_SPAWN_FAILED,
            format!("{verb} was killed by a signal"),
        ),
    }
}

/// Why the last automatic pass did not proceed (PLAN-071 §2, U5).
///
/// A deferral is a fact about the AUTOMATIC path, kept beside the lifecycle
/// state rather than replacing it: the device is still `ready` or
/// `reboot-required`, and what this adds is that something wanted to act and
/// did not. `since`/`count` are what make a permanently blocking application
/// distinguishable from a stuck update — PLAN-071 §2's whole point — because
/// "four automatic attempts were refused, the first one an hour ago" is the
/// sentence an operator opening the page after a week needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Deferral {
    /// The vocabulary PLAN-071 §2 names: `outside-window`,
    /// `reboot-gate-closed`, `clock-untrusted`, `version-suppressed`, plus
    /// the driver's own guards — resolved through
    /// [`crate::update_codes::deferral_code`], so this field holds a code and
    /// can hold nothing else.
    reason: &'static str,
    /// The refusing rule in words, verbatim from whoever refused.
    detail: String,
    /// When this reason first applied without interruption.
    since: DateTime<Utc>,
    /// The most recent attempt it applied to.
    at: DateTime<Utc>,
    /// How many attempts in a row it has applied to.
    count: u64,
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
    /// Why the last operation failed, with its code; cleared when the next
    /// one starts.
    failed: Option<CodedReason>,
    /// The workspace's refusal, when the last probe did not pass; cleared
    /// when the next operation starts and stays clear when its probe passes.
    unready: Option<Unready>,
    /// The last passing probe's report (`pool`, `free`, `used`, ...).
    workspace: Option<Value>,
    available: Option<Available>,
    /// The verified descriptor path the last fetch staged.
    descriptor: Option<String>,
    last_check: Option<String>,
    boot_phase: Option<(&'static str, String)>,
    override_record: Option<OverrideRecord>,
    /// Why the last automatic attempt did not proceed; `None` once one did.
    deferred: Option<Deferral>,
}

/// The lifecycle service: one per daemon, shared with the auto-check task.
pub struct UpdateLifecycle {
    client: Arc<dyn UpdateClient>,
    policy: PolicyStore,
    host: Arc<dyn LifecycleHost>,
    /// The bus layer's install-in-flight flag, shared so `installing` here
    /// and the install refusal there can never disagree.
    installing: Arc<AtomicBool>,
    /// The `/mos/updates` workspace root; `verified/` below it is the only
    /// place a recorded or installed descriptor may be.
    workspace_root: PathBuf,
    machine: Mutex<Machine>,
}

impl UpdateLifecycle {
    pub fn new(
        client: Arc<dyn UpdateClient>,
        policy: PolicyStore,
        host: Arc<dyn LifecycleHost>,
        installing: Arc<AtomicBool>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            client,
            policy,
            host,
            installing,
            workspace_root,
            machine: Mutex::new(Machine::default()),
        }
    }

    /// The client this lifecycle runs, for a rebuild around a new workspace.
    #[cfg(test)]
    pub fn client(&self) -> Arc<dyn UpdateClient> {
        Arc::clone(&self.client)
    }

    /// The policy store, for the same rebuild.
    pub fn policy(&self) -> PolicyStore {
        self.policy.clone()
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// `<workspace>/verified`: the one directory an installable descriptor is in.
    pub fn verified_dir(&self) -> PathBuf {
        self.workspace_root.join("verified")
    }

    /// Why `path` must not be handed to the native backend, or `Ok` when it is a regular
    /// file (not a symbolic link) directly inside `verified/` and not a
    /// `.part`. The bus layer's `InstallUpdate` asks this for every path,
    /// including an operator's explicit one: a partial, or a file outside
    /// `verified/`, is never installable by filename alone.
    pub fn installable(&self, path: &Path) -> Result<(), String> {
        let verified = self.verified_dir();
        if !path.is_absolute() || path.parent() != Some(verified.as_path()) {
            return Err(format!(
                "descriptor path `{}` is not inside the verified directory {}",
                path.display(),
                verified.display()
            ));
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| {
                name.strip_suffix(".json")
                    .is_none_or(|id| !crate::deployment::valid_id(id))
            })
        {
            return Err(format!(
                "path `{}` is not a deployment descriptor",
                path.display()
            ));
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_file() => Ok(()),
            Ok(_) => Err(format!(
                "descriptor path `{}` is not a regular file (a symbolic link is not followed)",
                path.display()
            )),
            Err(err) => Err(format!("descriptor path `{}`: {err}", path.display())),
        }
    }

    /// Start a metadata check (`sync` when a URL is configured, then
    /// `check`). Returns as soon as the work is handed to a background task;
    /// progress and the outcome land in `update.lifecycle`.
    pub async fn request_check(self: &Arc<Self>, sender: &str) -> Result<(), Refusal> {
        let policy = self.admit_check(sender).await?;
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let result = this.run_check(&policy).await;
            this.settle_check(result).await;
        });
        Ok(())
    }

    /// The same check, awaited to its outcome instead of spawned.
    ///
    /// Same admission, same subprocess, same recording — the difference is
    /// only that the caller learns what it found, which the automatic driver
    /// needs to decide its next step and an operator does not.
    pub async fn check_now(&self, sender: &str) -> Result<Settled<Available>, Refusal> {
        let policy = self.admit_check(sender).await?;
        let result = self.run_check(&policy).await;
        Ok(self.settle_check(result).await)
    }

    /// Admit a check: the policy refusal, the client, the busy slot. The
    /// policy it answers is the one the run must use, loaded once so the
    /// decision and the subprocess cannot read two different files.
    async fn admit_check(&self, sender: &str) -> Result<EffectivePolicy, Refusal> {
        let loaded = self.policy.load();
        if let Some(refusal) = update_policy::check_refusal(&loaded) {
            self.record_refusal("check", &refusal).await;
            return Err(Refusal::Policy(refusal.text));
        }
        if let Some(reason) = self.client.unavailable() {
            self.record_refusal(
                "check",
                &CodedReason::new(update_codes::REFUSED_CLIENT_UNAVAILABLE, reason.clone()),
            )
            .await;
            return Err(Refusal::Unavailable(reason));
        }
        self.begin("checking").await?;
        tracing::info!(sender, "update check requested");
        Ok(loaded.policy)
    }

    /// Record a finished check and answer what it found.
    async fn settle_check(&self, result: Result<CheckOutcome, Failure>) -> Settled<Available> {
        let mut machine = self.machine.lock().await;
        machine.operation = None;
        let settled = match result {
            Ok(CheckOutcome::Selected(available)) => {
                tracing::info!(
                    name = %available.deployment_id,
                    version = %available.version,
                    "update check selected a candidate"
                );
                machine.last_check = Some(now_rfc3339());
                machine.available = Some(available.clone());
                Settled::Done(available)
            }
            Ok(CheckOutcome::NoneCompatible) => {
                tracing::info!("update check found no compatible target");
                machine.last_check = Some(now_rfc3339());
                machine.available = None;
                Settled::NoneCompatible
            }
            Err(Failure::Unready(unready)) => {
                tracing::warn!(reason = %unready.reason(), "update workspace not ready; check not started");
                machine.unready = Some(unready.clone());
                Settled::Unready(unready)
            }
            Err(Failure::Error(failed)) => {
                tracing::warn!(
                    code = failed.code,
                    reason = failed.text,
                    "update check failed"
                );
                machine.last_check = Some(now_rfc3339());
                machine.failed = Some(failed.clone());
                Settled::Failed(failed)
            }
        };
        drop(machine);
        self.record_snapshot().await;
        settled
    }

    /// Start a descriptor fetch. Selection happens inside `mica-deploy fetch`
    /// itself, so a fetch does not require a prior check; on success the
    /// verified descriptor path is recorded and the state becomes `ready`.
    pub async fn request_fetch(self: &Arc<Self>, sender: &str) -> Result<(), Refusal> {
        let policy = self.admit_fetch(sender).await?;
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let result = this.run_fetch(&policy).await;
            this.settle_fetch(result).await;
        });
        Ok(())
    }

    /// The same fetch, awaited to its outcome instead of spawned; see
    /// [`Self::check_now`] for why the awaited form exists.
    pub async fn fetch_now(&self, sender: &str) -> Result<Settled<String>, Refusal> {
        let policy = self.admit_fetch(sender).await?;
        let result = self.run_fetch(&policy).await;
        Ok(self.settle_fetch(result).await)
    }

    /// Admit a fetch: the policy refusal (which includes every check
    /// refusal, plus metered), the client, the busy slot.
    async fn admit_fetch(&self, sender: &str) -> Result<EffectivePolicy, Refusal> {
        let loaded = self.policy.load();
        if let Some(refusal) = update_policy::fetch_refusal(&loaded) {
            self.record_refusal("fetch", &refusal).await;
            return Err(Refusal::Policy(refusal.text));
        }
        if let Some(reason) = self.client.unavailable() {
            self.record_refusal(
                "fetch",
                &CodedReason::new(update_codes::REFUSED_CLIENT_UNAVAILABLE, reason.clone()),
            )
            .await;
            return Err(Refusal::Unavailable(reason));
        }
        self.begin("downloading").await?;
        tracing::info!(sender, "update fetch requested");
        Ok(loaded.policy)
    }

    /// Record a finished fetch and answer what it staged.
    async fn settle_fetch(&self, result: Result<FetchOutcome, Failure>) -> Settled<String> {
        let mut machine = self.machine.lock().await;
        machine.operation = None;
        let settled = match result {
            Ok(FetchOutcome::Staged(path)) => {
                tracing::info!(descriptor = %path, "update fetch staged a verified descriptor");
                machine.descriptor = Some(path.clone());
                Settled::Done(path)
            }
            Ok(FetchOutcome::NoneCompatible) => {
                tracing::info!("update fetch found no compatible target");
                machine.available = None;
                Settled::NoneCompatible
            }
            Err(Failure::Unready(unready)) => {
                tracing::warn!(reason = %unready.reason(), "update workspace not ready; fetch not started");
                machine.unready = Some(unready.clone());
                Settled::Unready(unready)
            }
            Err(Failure::Error(failed)) => {
                tracing::warn!(
                    code = failed.code,
                    reason = failed.text,
                    "update fetch failed"
                );
                machine.failed = Some(failed.clone());
                Settled::Failed(failed)
            }
        };
        drop(machine);
        self.record_snapshot().await;
        settled
    }

    /// The candidate the last check selected, if it selected one.
    pub async fn available(&self) -> Option<Available> {
        self.machine.lock().await.available.clone()
    }

    /// The verified descriptor path the last fetch staged, if one is staged.
    pub async fn staged_descriptor(&self) -> Option<String> {
        self.machine.lock().await.descriptor.clone()
    }

    /// Discard all bounded acquisition files under the native transaction lock.
    pub async fn discard_descriptor(&self, why: &str) {
        if self.machine.lock().await.descriptor.is_none() {
            return;
        }
        if let Err(refusal) = self.begin("discarding").await {
            tracing::warn!(reason = refusal.message(), "workspace discard refused");
            return;
        }
        let result = self.client.run(&["discard".into()], PROBE_TIMEOUT).await;
        let mut machine = self.machine.lock().await;
        machine.operation = None;
        machine.descriptor = None;
        machine.available = None;
        let error = match result {
            Ok(output) => client_json("discard", &output).err(),
            Err(error) => Some(CodedReason::new(
                update_codes::CLIENT_SPAWN_FAILED,
                format!("discard: {error:#}"),
            )),
        };
        machine.failed = error;
        drop(machine);
        self.record_snapshot_with(Some(CodedReason::new(
            update_codes::NOTE_DEPLOYMENT_DISCARDED,
            format!("staged deployment discarded: {why}"),
        )))
        .await;
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
        machine.unready = None;
        drop(machine);
        self.record_snapshot().await;
        Ok(())
    }

    /// The PLAN-061 readiness probe, before any acquisition: `mica-deploy
    /// probe` against the policy's budget. A passing probe records the
    /// workspace report; a failing one is the `update-unavailable` state.
    async fn probe(&self, workspace: &Workspace) -> Result<(), Failure> {
        let args = vec![
            "--max-bytes".to_string(),
            workspace.max_bytes.to_string(),
            "probe".to_string(),
        ];
        let output = self.client.run(&args, PROBE_TIMEOUT).await.map_err(|err| {
            Failure::Error(CodedReason::new(
                update_codes::CLIENT_SPAWN_FAILED,
                format!("probe: {err:#}"),
            ))
        })?;
        match parse_probe(&output).map_err(Failure::Error)? {
            ProbeOutcome::Ready(report) => {
                self.machine.lock().await.workspace = Some(report);
                Ok(())
            }
            ProbeOutcome::Unready(unready) => Err(Failure::Unready(unready)),
        }
    }

    async fn run_check(&self, policy: &EffectivePolicy) -> Result<CheckOutcome, Failure> {
        self.probe(&policy.workspace).await?;
        let args = acquisition_args("check", selection_of(policy)?, &policy.workspace)?;
        let output = self
            .client
            .run(&args, CHECK_TIMEOUT)
            .await
            .map_err(|error| {
                Failure::Error(CodedReason::new(
                    update_codes::CLIENT_SPAWN_FAILED,
                    format!("check: {error:#}"),
                ))
            })?;
        parse_check(&output).map_err(Failure::Error)
    }

    async fn run_fetch(&self, policy: &EffectivePolicy) -> Result<FetchOutcome, Failure> {
        self.probe(&policy.workspace).await?;
        let args = acquisition_args("fetch", selection_of(policy)?, &policy.workspace)?;
        let output = self
            .client
            .run(&args, FETCH_TIMEOUT)
            .await
            .map_err(|error| {
                Failure::Error(CodedReason::new(
                    update_codes::CLIENT_SPAWN_FAILED,
                    format!("fetch: {error:#}"),
                ))
            })?;
        parse_fetch(&output, &self.verified_dir()).map_err(Failure::Error)
    }

    /// The status and the phase are derived from the same native observation.
    pub async fn refresh(&self, status: &Status) {
        self.machine.lock().await.boot_phase = Some(status.phase());
        self.record_snapshot().await;
    }

    /// Installing a staged candidate consumes its pending acquisition state.
    pub async fn installed(&self, status: &Status) {
        let mut machine = self.machine.lock().await;
        if status.state.candidate.is_some() {
            machine.descriptor = None;
            machine.available = None;
        }
        machine.boot_phase = Some(status.phase());
        drop(machine);
        self.record_snapshot().await;
    }

    /// Write the operator's update document (PLAN-071 §3, U11).
    ///
    /// **micad is the file's only writer and this is where it writes.** apid
    /// does not hold a path to `/mos/config/updates.json`; it asks over the
    /// bus, which is PLAN-070 §5.2's one-writer rule rather than a choice made
    /// here. The ordering — parse the patch, refuse an anchor by name, load
    /// the base, merge, validate, save atomically — is
    /// [`configuration::write_updates`]'s and exists once, so *the on-disk
    /// document is never replaced by one that would fail to load* is a shape
    /// and not a convention.
    ///
    /// Answers the saved document, absent and `null` still distinct.
    ///
    /// # Errors
    ///
    /// [`Refusal::Invalid`] for a patch or a resulting document the reader
    /// would refuse, with the offending field named — including §2's
    /// `auto`-requires-a-window rule, which fires here rather than hours
    /// later at the next check. [`Refusal::Policy`] when the document on the
    /// disk does not load, which is the same refusal every other action on it
    /// already gives. [`Refusal::Unavailable`] when the device could not store
    /// it.
    pub async fn write_config(&self, sender: &str, patch_json: &str) -> Result<Value, Refusal> {
        let Some(path) = self.policy.path() else {
            // The dry-run store, which was told to read no file. Refused
            // rather than defaulted to `/mos/config/`: a daemon that reads
            // nothing must not write the device's real configuration.
            return Err(Refusal::Unavailable(
                "this daemon has no update policy document".to_string(),
            ));
        };
        let document = configuration::write_updates(path, patch_json).map_err(|err| match err {
            configuration::WriteRefusal::Rejected(err) => Refusal::Invalid(err.to_string()),
            configuration::WriteRefusal::Unreadable(err) => Refusal::Policy(err.to_string()),
            configuration::WriteRefusal::Unwritable(err) => Refusal::Unavailable(err.to_string()),
        })?;
        let rendered = serde_json::to_value(&document).map_err(|err| {
            // Unreachable: the document was just serialized onto the disk by
            // the call above. Reported rather than unwrapped, because a
            // panic here would take the daemon down over a write that
            // succeeded.
            Refusal::Unavailable(format!("the saved document could not be rendered: {err}"))
        })?;
        // Record the actor and path; apid records the authenticated actor too.
        //
        // **The document is deliberately not in this line.** It carries no
        // secret by schema, but `source.url` is an operator-typed URL and a
        // URL can carry credentials in its userinfo; the journal is not where
        // that should be discovered. Who and where is what a support case
        // needs, and what the document now says is one authenticated read
        // away.
        tracing::warn!(sender, path = %path.display(), "update configuration written");
        // A fresh snapshot rather than a note: the policy is re-read here, so
        // the very next state read reports what was just written instead of
        // what the last action saw.
        self.record_snapshot().await;
        Ok(rendered)
    }

    /// Record why an automatic attempt did not proceed (U5).
    ///
    /// The same reason arriving again extends the existing fact rather than
    /// replacing it: `since` stays where it was and `count` rises, so the
    /// state answers "how long" and not only "why". A DIFFERENT reason
    /// starts a new fact, because the clock it would otherwise inherit
    /// belongs to a different refusal.
    ///
    /// PLAN-076 B4: `reason` is resolved to a code HERE rather than trusted,
    /// so the recorded set is closed at the writer and not merely by the
    /// convention of every caller. A word outside the vocabulary is recorded
    /// as `unknown` — the deferral is still visible, with its `detail` and its
    /// clock intact, and only the name is lost, to the log line below.
    pub async fn defer(&self, reason: &str, detail: &str) {
        let code = update_codes::deferral_code(reason);
        if code == update_codes::UNKNOWN {
            tracing::warn!(
                reason,
                detail,
                "automatic pass deferred for a reason outside the published vocabulary; \
                 recorded as `unknown`"
            );
        }
        let now = Utc::now();
        let mut machine = self.machine.lock().await;
        match &mut machine.deferred {
            Some(existing) if existing.reason == code => {
                existing.detail = detail.to_string();
                existing.at = now;
                existing.count = existing.count.saturating_add(1);
            }
            slot => {
                *slot = Some(Deferral {
                    reason: code,
                    detail: detail.to_string(),
                    since: now,
                    at: now,
                    count: 1,
                });
            }
        }
        drop(machine);
        self.record_snapshot().await;
    }

    /// Forget the last deferral: an automatic attempt proceeded.
    ///
    /// `only` clears just that reason, and exists because most successes
    /// supersede one specific refusal rather than every refusal: a check
    /// that finds a newer release ends `no-newer-release` and says nothing
    /// about whether the maintenance window is open. `None` clears whatever
    /// is there, for the two steps that ran the whole pass to its end.
    ///
    /// A no-op when nothing matches, so the common tick neither takes the
    /// lock twice nor re-records an unchanged entry.
    pub async fn resume(&self, only: Option<&str>) {
        {
            let mut machine = self.machine.lock().await;
            let matches = machine
                .deferred
                .as_ref()
                .is_some_and(|deferred| only.is_none_or(|reason| deferred.reason == reason));
            if !matches {
                return;
            }
            machine.deferred = None;
        }
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
    ///
    /// The code travels with the sentence rather than being recovered from it
    /// (PLAN-076 B4): every caller has one, because the predicate that
    /// refused minted it.
    async fn record_refusal(&self, action: &str, refusal: &CodedReason) {
        tracing::warn!(
            action,
            code = refusal.code,
            reason = refusal.text,
            "update action refused by policy"
        );
        self.record_snapshot_with(Some(CodedReason::new(
            refusal.code,
            format!("{action} refused: {}", refusal.text),
        )))
        .await;
    }

    async fn record_snapshot(&self) {
        self.record_snapshot_with(None).await;
    }

    /// Build the full `update.lifecycle` entry and hand it to the host.
    async fn record_snapshot_with(&self, last_refusal: Option<CodedReason>) {
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
            &self.workspace_root,
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

/// The selection an acquisition runs against, or the failure that says the
/// operator document did not load.
///
/// Unreachable through `request_check`/`request_fetch`, whose policy refusal
/// catches the same condition first; kept as an error rather than an unwrap
/// because "the document did not load" must never resolve to the baked
/// channel (PLAN-070 §5.1).
fn selection_of(policy: &EffectivePolicy) -> Result<&Selection, Failure> {
    policy.selection.as_ref().ok_or_else(|| {
        Failure::Error(CodedReason::new(
            update_codes::POLICY_NOT_LOADED,
            "the update policy document did not load, so there is no channel to check",
        ))
    })
}

fn acquisition_args(
    verb: &str,
    selection: &Selection,
    workspace: &Workspace,
) -> Result<Vec<String>, Failure> {
    let source = selection.url.as_ref().ok_or_else(|| {
        Failure::Error(CodedReason::new(
            update_codes::NO_SOURCE_CONFIGURED,
            "no update source configured",
        ))
    })?;
    Ok(vec![
        "--max-bytes".into(),
        workspace.max_bytes.to_string(),
        verb.into(),
        "--source".into(),
        source.clone(),
        "--channel".into(),
        selection.channel.clone(),
    ])
}

/// The one place the recorded entry is shaped, so the state precedence —
/// installing over a running client operation over an unready workspace
/// over a failure over a staged descriptor over the boot-derived phase over
/// idle — is written once.
#[allow(clippy::too_many_arguments)]
fn render_entry(
    machine: &Machine,
    loaded: &LoadedPolicy,
    gate: &GateVerdict,
    installing: bool,
    client_unavailable: Option<String>,
    policy_path: Option<&std::path::Path>,
    last_refusal: Option<CodedReason>,
    workspace_root: &Path,
) -> Value {
    // `code` is present exactly for the two states that report a FAILURE, and
    // absent for the rest (PLAN-076 B4). `ready` and the boot-derived phases
    // carry a `reason` too, but theirs describes a state that is already its
    // own enumerated word — coding "a verified descriptor is staged" would add a
    // second spelling of `ready` and nothing else.
    let (state, reason, code): (&str, Option<String>, Option<&'static str>) = if installing {
        ("installing", None, None)
    } else if let Some(operation) = machine.operation {
        (operation, None, None)
    } else if let Some(unready) = &machine.unready {
        (
            "update-unavailable",
            Some(unready.reason()),
            Some(unready.kind),
        )
    } else if let Some(failed) = &machine.failed {
        ("failed", Some(failed.text.clone()), Some(failed.code))
    } else if machine.descriptor.is_some() {
        (
            "ready",
            Some("a verified descriptor is staged for install".to_string()),
            None,
        )
    } else if let Some((phase, why)) = &machine.boot_phase {
        (*phase, Some(why.clone()), None)
    } else {
        ("idle", None, None)
    };
    let mut entry = serde_json::Map::new();
    entry.insert("state".into(), json!(state));
    if let Some(reason) = reason {
        entry.insert("reason".into(), json!(reason));
    }
    if let Some(code) = code {
        entry.insert("code".into(), json!(code));
    }
    if let Some(refusal) = last_refusal {
        entry.insert("last_refusal".into(), json!(refusal.text));
        entry.insert("last_refusal_code".into(), json!(refusal.code));
    }
    if let Some(available) = &machine.available {
        entry.insert(
            "available".into(),
            json!({
                "deploymentId": available.deployment_id,
                "version": available.version,
                "channel": available.channel,
            }),
        );
    }
    if let Some(descriptor) = &machine.descriptor {
        entry.insert(
            "deploymentId".into(),
            json!(Path::new(descriptor).file_stem().and_then(|id| id.to_str())),
        );
    }
    if let Some(last_check) = &machine.last_check {
        entry.insert("last_check".into(), json!(last_check));
    }
    // PLAN-071 §2: deferral must be VISIBLE, not silent. A permanently
    // blocking application permanently defers the reboot, which is correct
    // and is also indistinguishable from a stuck update unless the device
    // says so. `since`/`waitedSeconds`/`count` are the "how long" half of
    // that sentence; the state beside it (`ready`, `reboot-required`) is
    // still what the device IS, because a deferral is a fact about the
    // automatic path and not a state of the machine.
    if let Some(deferred) = &machine.deferred {
        entry.insert(
            "deferred".into(),
            json!({
                "reason": deferred.reason,
                "detail": deferred.detail,
                "since": deferred.since.to_rfc3339_opts(SecondsFormat::Secs, true),
                "at": deferred.at.to_rfc3339_opts(SecondsFormat::Secs, true),
                "waitedSeconds": (Utc::now() - deferred.since).num_seconds().max(0),
                "attempts": deferred.count,
            }),
        );
    }
    entry.insert(
        "client".into(),
        match &client_unavailable {
            None => json!({ "available": true }),
            Some(reason) => json!({ "available": false, "reason": reason }),
        },
    );
    // The workspace as the last probe saw it: `status` is the vocabulary
    // the storage surface shares (`ready`, `degraded`, `unavailable`), or
    // `unprobed` before any check/fetch has run. Capacity figures are the
    // DATA pool's, stated once.
    let mut workspace = serde_json::Map::new();
    workspace.insert("root".into(), json!(workspace_root.display().to_string()));
    match (&machine.unready, &machine.workspace) {
        (Some(unready), _) => {
            workspace.insert("status".into(), json!(unready.status));
            workspace.insert("kind".into(), json!(unready.kind));
            workspace.insert("detail".into(), json!(unready.detail));
        }
        (None, Some(report)) => {
            workspace.insert("status".into(), json!("ready"));
            if let Some(report) = report.as_object() {
                for (key, value) in report {
                    if key != "root" {
                        workspace.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (None, None) => {
            workspace.insert("status".into(), json!("unprobed"));
        }
    }
    entry.insert("workspace".into(), Value::Object(workspace));
    let policy = &loaded.policy;
    let selection = policy.selection.as_ref();
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
            // The four values PLAN-070 §5.1's precedence resolves, and `null`
            // for all four when the operator document did not load --
            // `policy_error` below is what distinguishes "unknown" from
            // "unset", and the baked default is deliberately NOT shown here as
            // a stand-in. F9 adds the baked/operator/effective reading to
            // `GET /api/v1/provisioning/status`, which is where §8 puts it.
            "policy": selection.map(|selection| selection.mode.as_str()),
            "checkIntervalMinutes": selection.map(|selection| selection.check_interval_minutes),
            "sourceUrl": selection.and_then(|selection| selection.url.clone()),
            "channel": selection.map(|selection| selection.channel.clone()),
            // Layer 2 owns this one outright, so it answers even when the
            // selection does not.
            "rebootPolicy": policy.reboot_policy.as_str(),
            "networkMode": match policy.network.mode {
                crate::update_policy::NetworkMode::Online => "online",
                crate::update_policy::NetworkMode::Metered => "metered",
                crate::update_policy::NetworkMode::Offline => "offline",
            },
            "meteredAllowsFetch": policy.network.metered_allows_fetch,
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

    fn selection_output() -> ClientOutput {
        output(
            0,
            &json!({"revision":1,"channel":"stable","selected":{
                "deploymentId":"a".repeat(64),"deployment":{"version":"1.1.0"}}
            })
            .to_string(),
            "",
        )
    }

    fn fetch_output(path: &str) -> ClientOutput {
        output(
            0,
            &json!({"id":"a".repeat(64),"path":path,
                "objects":"/mos/updates/verified/objects","version":"1.1.0","generation":3
            })
            .to_string(),
            "",
        )
    }

    fn staged_path() -> String {
        format!("/mos/updates/verified/{}.json", "a".repeat(64))
    }

    fn no_selection() -> ClientOutput {
        output(
            0,
            r#"{"revision":1,"channel":"stable","selected":null}"#,
            "",
        )
    }

    /// A client killed by a signal: no exit code at all.
    fn output_signalled(stderr: &str) -> ClientOutput {
        ClientOutput {
            code: None,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn check_output_parses_native_json_and_reports_failure_codes() {
        assert!(matches!(
            parse_check(&selection_output()),
            Ok(CheckOutcome::Selected(_))
        ));
        assert_eq!(
            parse_check(&no_selection()),
            Ok(CheckOutcome::NoneCompatible)
        );
        assert_eq!(
            parse_check(&output(1, "", "catalog is expired"))
                .unwrap_err()
                .code,
            update_codes::CLIENT_EXIT_FAILURE
        );
        for malformed in ["selected only-a-name", "{}", r#"{"selected":{}}"#] {
            assert_eq!(
                parse_check(&output(0, malformed, "")).unwrap_err().code,
                update_codes::CLIENT_OUTPUT_UNPARSEABLE
            );
        }
        assert_eq!(
            parse_check(&output_signalled("")).unwrap_err().code,
            update_codes::CLIENT_SPAWN_FAILED
        );
    }

    #[test]
    fn fetch_output_is_an_identified_verified_descriptor_or_nothing() {
        let verified = Path::new("/mos/updates/verified");
        assert_eq!(
            parse_fetch(&fetch_output(&staged_path()), verified),
            Ok(FetchOutcome::Staged(staged_path()))
        );
        assert_eq!(
            parse_fetch(&output(0, "null", ""), verified),
            Ok(FetchOutcome::NoneCompatible)
        );
        for outside in [
            "/mos/updates/downloads/a.json",
            "/mos/updates/verified/a.json",
            "/tmp/a.json",
            "/mos/updates/verified/a.json.partial",
        ] {
            assert_eq!(
                parse_fetch(&fetch_output(outside), verified)
                    .unwrap_err()
                    .code,
                update_codes::UNVERIFIED_DEPLOYMENT_PATH
            );
        }
        let mut wrong: Value = serde_json::from_str(&fetch_output(&staged_path()).stdout).unwrap();
        wrong["objects"] = json!("/tmp/objects");
        assert!(parse_fetch(&output(0, &wrong.to_string(), ""), verified).is_err());
        assert_eq!(
            parse_fetch(&output(1, "", "budget exceeded"), verified)
                .unwrap_err()
                .code,
            update_codes::CLIENT_EXIT_FAILURE
        );
    }

    #[test]
    fn probe_output_requires_ready_json_and_preserves_native_refusals() {
        let (_, ready) = ready_probe();
        let ProbeOutcome::Ready(report) = parse_probe(&ready.unwrap()).unwrap() else {
            panic!("ready expected")
        };
        assert_eq!(report["freeBytes"], 1_000_000_000u64);
        for detail in [
            "DATA is not mounted",
            "read-only filesystem",
            "insufficient workspace capacity",
        ] {
            let ProbeOutcome::Unready(unready) = parse_probe(&output(1, "", detail)).unwrap()
            else {
                panic!("refusal expected")
            };
            assert_eq!(unready.kind, update_codes::WORKSPACE_PROBE_FAILED);
            assert_eq!(unready.detail, detail);
        }
        for malformed in ["ready free=123", "{}", r#"{"status":"unknown"}"#] {
            assert_eq!(
                parse_probe(&output(0, malformed, "")).unwrap_err().code,
                update_codes::CLIENT_OUTPUT_UNPARSEABLE
            );
        }
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
                args.get(usize::from(args.first().is_some_and(|arg| arg == "--max-bytes")) * 2)
                    == Some(&verb),
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
        let path = dir.path().join("updates.json");
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
                PathBuf::from(DEFAULT_WORKSPACE_ROOT),
            )),
            installing,
        )
    }

    /// The probe answer of a healthy workspace, first in every script.
    fn ready_probe() -> (&'static str, Result<ClientOutput, String>) {
        (
            "probe",
            Ok(output(
                0,
                r#"{"status":"ready","root":"/mos/updates","freeBytes":1000000000,"maxBytes":500000000,"freeInodes":10000}"#,
                "",
            )),
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
    async fn a_check_probes_then_checks_the_signed_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_file(&dir, r#"{"source":{"url":"https://updates.example"}}"#);
        let client = MockClient::new(vec![ready_probe(), ("check", Ok(selection_output()))]);
        let calls = Arc::clone(&client.calls);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
        lifecycle.request_check("test").await.unwrap();
        let recorded = settled(&host).await;
        assert_eq!(recorded["state"], "idle");
        assert_eq!(recorded["available"]["version"], "1.1.0");
        assert_eq!(recorded["workspace"]["freeBytes"], 1_000_000_000u64);
        assert!(recorded["last_check"].is_string());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], ["--max-bytes", "500000000", "probe"]);
        assert_eq!(
            calls[1],
            [
                "--max-bytes",
                "500000000",
                "check",
                "--source",
                "https://updates.example",
                "--channel",
                "stable"
            ]
        );
    }

    #[tokio::test]
    async fn a_fetch_records_the_verified_descriptor_as_ready() {
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_file(&dir, r#"{"source":{"url":"https://updates.example"}}"#);
        let client = MockClient::new(vec![
            ready_probe(),
            ("fetch", Ok(fetch_output(&staged_path()))),
        ]);
        let calls = Arc::clone(&client.calls);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
        lifecycle.request_fetch("test").await.unwrap();
        let recorded = settled(&host).await;
        assert_eq!(recorded["state"], "ready");
        assert_eq!(recorded["deploymentId"], "a".repeat(64));
        assert_eq!(
            calls.lock().unwrap()[1],
            [
                "--max-bytes",
                "500000000",
                "fetch",
                "--source",
                "https://updates.example",
                "--channel",
                "stable"
            ]
        );
    }

    #[tokio::test]
    async fn a_fetch_path_outside_verified_is_never_recorded_as_ready() {
        for printed in [
            "/mos/updates/downloads/x.json.part",
            "/mos/updates/downloads/x.json",
            "/mos/updates/verified/x.json.part",
            "/tmp/outside/x.json",
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let policy = policy_file(&dir, r#"{"source": {"url": "http://mirror/tuf"}}"#);
            let client = MockClient::new(vec![ready_probe(), ("fetch", Ok(fetch_output(printed)))]);
            let host = TestHost::new();
            let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
            lifecycle.request_fetch("test").await.expect("accepted");
            let recorded = settled(&host).await;
            assert_eq!(recorded["state"], "failed", "{printed}: {recorded}");
            assert!(
                recorded.get("deploymentId").is_none(),
                "{printed}: {recorded}"
            );
            assert!(
                recorded["reason"]
                    .as_str()
                    .expect("reason")
                    .contains("verified"),
                "{printed}: {recorded}"
            );
            assert_eq!(
                recorded["code"],
                update_codes::UNVERIFIED_DEPLOYMENT_PATH,
                "{printed}: {recorded}"
            );
        }
    }

    #[tokio::test]
    async fn an_unready_workspace_refuses_acquisition_until_a_probe_passes() {
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_file(&dir, r#"{"source":{"url":"https://updates.example"}}"#);
        let client = MockClient::new(vec![
            ("probe", Ok(output(1, "", "DATA is read-only"))),
            ready_probe(),
            ("check", Ok(no_selection())),
        ]);
        let calls = Arc::clone(&client.calls);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
        lifecycle.request_check("test").await.unwrap();
        let recorded = settled(&host).await;
        assert_eq!(recorded["state"], "update-unavailable");
        assert_eq!(recorded["code"], update_codes::WORKSPACE_PROBE_FAILED);
        assert_eq!(recorded["workspace"]["detail"], "DATA is read-only");
        assert_eq!(calls.lock().unwrap().len(), 1);
        lifecycle.request_check("test").await.unwrap();
        let recorded = settled(&host).await;
        assert_eq!(recorded["state"], "idle");
        assert_eq!(recorded["workspace"]["status"], "ready");
        assert!(recorded.get("code").is_none());
        assert_eq!(calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_fetch_failure_never_records_a_descriptor_as_ready() {
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_file(&dir, r#"{"source":{"url":"https://updates.example"}}"#);
        let client = MockClient::new(vec![
            ready_probe(),
            ("fetch", Ok(output(1, "", "component digest mismatch"))),
        ]);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
        lifecycle.request_fetch("test").await.unwrap();
        let recorded = settled(&host).await;
        assert_eq!(recorded["state"], "failed");
        assert_eq!(recorded["code"], update_codes::CLIENT_EXIT_FAILURE);
        assert!(recorded.get("deploymentId").is_none());
    }

    #[test]
    fn only_a_verified_regular_file_is_installable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("updates");
        let verified = root.join("verified");
        std::fs::create_dir_all(&verified).expect("verified/");
        std::fs::create_dir_all(root.join("downloads")).expect("downloads/");
        let lifecycle = UpdateLifecycle::new(
            Arc::new(MockClient::new(vec![])),
            PolicyStore::defaults(),
            TestHost::new(),
            Arc::new(AtomicBool::new(false)),
            root.clone(),
        );
        assert_eq!(lifecycle.verified_dir(), verified);

        let good = verified.join(format!("{}.json", "a".repeat(64)));
        std::fs::write(&good, b"verified bytes").expect("seed");
        assert_eq!(lifecycle.installable(&good), Ok(()));

        let refused = |path: &Path, needle: &str| {
            let err = lifecycle.installable(path).expect_err(needle);
            assert!(err.contains(needle), "{}: {err}", path.display());
        };
        let part = verified.join("invalid.json.part");
        std::fs::write(&part, b"partial").expect("seed");
        refused(&part, "not a deployment descriptor");
        let in_downloads = root.join("downloads").join("invalid.json");
        std::fs::write(&in_downloads, b"unverified").expect("seed");
        refused(&in_downloads, "not inside");
        let elsewhere = dir.path().join("invalid.json");
        std::fs::write(&elsewhere, b"unverified").expect("seed");
        refused(&elsewhere, "not inside");
        let link = verified.join(format!("{}.json", "b".repeat(64)));
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        refused(&link, "not a regular file");
        refused(
            &verified.join(format!("{}.json", "e".repeat(64))),
            "No such file",
        );
        refused(Path::new("relative.json"), "not inside");
    }

    #[tokio::test]
    async fn a_failed_check_is_a_failed_state_with_its_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, r#"{"source": {"url": "http://mirror/tuf"}}"#);
        let client = MockClient::new(vec![
            ready_probe(),
            ("check", Ok(output(1, "", "connection refused\n"))),
        ]);
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
        // PLAN-076 B4: the sentence stays for the operator, and the class is
        // beside it for everything that is not one.
        assert_eq!(recorded["code"], update_codes::CLIENT_EXIT_FAILURE);
    }

    // The three remaining `failed` codes, each driven to the recorded
    // document rather than asserted at the parser it is minted in.
    //
    // `client-spawn-failed` has two producers and only one of them is a
    // parse: this is the other one, the invocation that never returned an
    // exit status at all.
    #[tokio::test]
    async fn a_client_that_cannot_run_and_one_that_will_not_speak_are_two_codes() {
        for (result, code) in [
            (
                Err("spawn failed".into()),
                update_codes::CLIENT_SPAWN_FAILED,
            ),
            (
                Ok(output(0, "hello", "")),
                update_codes::CLIENT_OUTPUT_UNPARSEABLE,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let policy = policy_file(&dir, r#"{"source":{"url":"https://updates.example"}}"#);
            let host = TestHost::new();
            let (lifecycle, _) = lifecycle(
                MockClient::new(vec![ready_probe(), ("check", result)]),
                policy,
                Arc::clone(&host),
            );
            lifecycle.request_check("test").await.unwrap();
            let recorded = settled(&host).await;
            assert_eq!(recorded["state"], "failed");
            assert_eq!(recorded["code"], code);
        }
    }

    // The two members of the `last_refusal` set that are not refusals, driven
    // through the calls that write them. Both share the coded member with the
    // refusals, so both need a code; neither would be reachable from a
    // refusal test.
    #[tokio::test]
    async fn discarding_a_staged_deployment_uses_native_workspace_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_file(&dir, r#"{"source":{"url":"https://updates.example"}}"#);
        let client = MockClient::new(vec![
            ready_probe(),
            ("fetch", Ok(fetch_output(&staged_path()))),
            ("discard", Ok(output(0, "{\"removed\":3}", ""))),
        ]);
        let calls = Arc::clone(&client.calls);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
        lifecycle.fetch_now("test").await.unwrap();
        lifecycle.discard_descriptor("release withdrawn").await;
        assert_eq!(calls.lock().unwrap().last().unwrap(), &["discard"]);
        assert!(lifecycle.staged_descriptor().await.is_none());
        assert_eq!(
            host.last()["last_refusal_code"],
            update_codes::NOTE_DEPLOYMENT_DISCARDED
        );
    }

    #[tokio::test]
    async fn every_policy_refusal_is_refused_and_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Offline: both verbs refused, nothing ever reaches the client.
        let policy = policy_file(
            &dir,
            r#"{"source": {"url": "http://mirror/tuf"}, "network": {"mode": "offline"}}"#,
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
        assert_eq!(
            recorded["last_refusal_code"],
            update_codes::REFUSED_NETWORK_OFFLINE,
            "got: {recorded}"
        );
        assert_eq!(recorded["policy"]["networkMode"], "offline");
    }

    // PLAN-076 B4, the deferral half. Every reason the automatic driver mints
    // is driven through the recording path an operator polls, and a word from
    // outside the vocabulary is driven through the same path to show what
    // happens to it.
    //
    // THE SEAM, AND WHY IT IS HERE. `AutoDriver` keys its cadence on
    // `std::time::Instant`, which `tokio::time::pause` does not move, so the
    // driver cannot be ticked in a unit test today (RFCT-341 adds that seam).
    // The recording site can be, and it is the site that matters for this
    // gate: `defer` is what decides what reaches the document, so a driver
    // test would prove which reason is chosen and this proves what the wire
    // is allowed to carry. What is NOT proven here is the mapping from a
    // failure to its reason inside the driver — see RFCT-339's notes.
    #[tokio::test]
    async fn every_deferral_reaches_the_document_as_a_code_and_nothing_else_does() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, r#"{"source": {"url": "http://mirror/tuf"}}"#);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(MockClient::new(vec![]), policy, Arc::clone(&host));

        for reason in [
            update_codes::DEFER_CHECK_REFUSED,
            update_codes::DEFER_NO_NEWER_RELEASE,
            update_codes::DEFER_FETCH_REFUSED,
            update_codes::DEFER_CLOCK_UNTRUSTED,
            update_codes::DEFER_OUTSIDE_WINDOW,
            update_codes::DEFER_DEPLOYMENT_STATUS_UNKNOWN,
            update_codes::DEFER_REBOOT_PENDING,
            update_codes::DEFER_WORKSPACE_UNREADY,
            update_codes::DEFER_RECHECK_FAILED,
            update_codes::DEFER_RECHECK_REFUSED,
            update_codes::DEFER_SUPERSEDED,
            update_codes::DEFER_INSTALL_REFUSED,
            update_codes::DEFER_REBOOT_GATE_CLOSED,
        ] {
            lifecycle.defer(reason, "the refusing rule, in words").await;
            let recorded = host.last();
            assert_eq!(recorded["deferred"]["reason"], reason, "{recorded}");
            assert_eq!(
                recorded["deferred"]["detail"], "the refusing rule, in words",
                "{recorded}"
            );
            assert_eq!(recorded["deferred"]["attempts"], 1, "{reason}: {recorded}");
            lifecycle.resume(None).await;
        }

        // A reason outside the vocabulary. The deferral is still recorded —
        // an invisible refusal is the defect PLAN-071 §2 exists to stop — and
        // its detail, clock and attempt count are untouched. What does not
        // survive is the word: the document says `unknown`, and it does NOT
        // say `no-newer-releases`.
        lifecycle
            .defer("no-newer-releases", "a plausible misspelling")
            .await;
        let recorded = host.last();
        assert_eq!(recorded["deferred"]["reason"], update_codes::UNKNOWN);
        assert_eq!(recorded["deferred"]["detail"], "a plausible misspelling");
        assert_eq!(recorded["deferred"]["attempts"], 1);
        assert!(
            !recorded.to_string().contains("no-newer-releases"),
            "the rejected word must not reach the document by any member: {recorded}"
        );

        // The clamp does not merge two different unknown reasons into one
        // fact by accident of sharing a code: they DO share it, deliberately,
        // which is the information the fallback gives up. `attempts` rising
        // is what says so out loud rather than silently.
        lifecycle
            .defer("another-invention", "and another rule")
            .await;
        let recorded = host.last();
        assert_eq!(recorded["deferred"]["reason"], update_codes::UNKNOWN);
        assert_eq!(recorded["deferred"]["detail"], "and another rule");
        assert_eq!(recorded["deferred"]["attempts"], 2);

        // `resume` names a reason, so it clears by code too: an unmatched
        // word clears nothing rather than clearing the wrong thing.
        lifecycle.resume(Some("another-invention")).await;
        assert_eq!(
            host.last()["deferred"]["reason"],
            update_codes::UNKNOWN,
            "a word that is not the recorded code must not clear it"
        );
        lifecycle.resume(Some(update_codes::UNKNOWN)).await;
        assert!(
            host.last().get("deferred").is_none(),
            "the recorded code clears it"
        );
    }

    #[tokio::test]
    async fn metered_mode_refuses_fetch_but_admits_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(
            &dir,
            r#"{"source": {"url": "http://mirror/tuf"}, "network": {"mode": "metered"}}"#,
        );
        let client = MockClient::new(vec![ready_probe(), ("check", Ok(no_selection()))]);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(client, policy, Arc::clone(&host));
        let refusal = lifecycle.request_fetch("test").await.expect_err("refused");
        assert!(refusal.message().contains("metered"));
        assert_eq!(
            host.last()["last_refusal_code"],
            update_codes::REFUSED_NETWORK_METERED
        );
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
        let policy = policy_file(&dir, r#"{"source": {"url": "http://mirror/tuf"}}"#);
        let client = MockClient::absent("/usr/bin/mica-deploy is not present on this image");
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
        // `client.available: false` is itself the enumerated fact — a boolean
        // needs no code — but the refusal it produced is on the coded member.
        assert_eq!(
            recorded["last_refusal_code"],
            update_codes::REFUSED_CLIENT_UNAVAILABLE,
            "got: {recorded}"
        );
    }

    #[tokio::test]
    async fn a_second_operation_is_refused_while_one_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, r#"{"source": {"url": "http://mirror/tuf"}}"#);
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
            PathBuf::from(DEFAULT_WORKSPACE_ROOT),
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
        let policy = policy_file(&dir, "{}");
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
                .set_reboot_override(
                    "op",
                    micad_settings::configuration::OVERRIDE_CEILING_SECONDS + 1,
                )
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
    async fn native_status_drives_boot_phase_and_consumes_an_installed_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(
            MockClient::new(vec![]),
            policy_file(&dir, "{}"),
            Arc::clone(&host),
        );
        let status = Status::parse(&crate::deployment::tests::fixture().to_string()).unwrap();
        lifecycle.refresh(&status).await;
        assert_eq!(host.last()["state"], "succeeded");
        let mut pending = status;
        pending.state.candidate = Some("e".repeat(64));
        lifecycle.machine.lock().await.descriptor = Some(staged_path());
        lifecycle.installed(&pending).await;
        assert_eq!(host.last()["state"], "reboot-required");
        assert!(lifecycle.staged_descriptor().await.is_none());
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
            r#"{"maintenance": {"windows": [{"days": ["mon"], "start": "00:00", "end": "00:01"}]}}"#,
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
    async fn the_subprocess_client_bounds_output_and_stops_its_transport_group() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("transport");
        let pid = dir.path().join("child.pid");
        std::fs::write(&stub, "#!/bin/sh\nif [ \"$1\" = large ]; then head -c 70000 /dev/zero; else sleep 60 & echo $! > \"$1\"; wait; fi\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let client = Arc::new(SubprocessClient::new(stub));
        assert!(
            client
                .run(&["large".into()], Duration::from_secs(5))
                .await
                .is_err()
        );
        assert!(
            client
                .run(
                    &[pid.to_string_lossy().into_owned()],
                    Duration::from_millis(150)
                )
                .await
                .is_err()
        );
        let child = std::fs::read_to_string(pid).unwrap();
        assert_transport_stopped(child.trim()).await;
        let cancelled_pid = dir.path().join("cancelled.pid");
        let args = vec![cancelled_pid.to_string_lossy().into_owned()];
        let task = tokio::spawn(async move { client.run(&args, Duration::from_secs(30)).await });
        for _ in 0..100 {
            if cancelled_pid.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let child = std::fs::read_to_string(cancelled_pid).unwrap();
        task.abort();
        let _ = task.await;
        assert_transport_stopped(child.trim()).await;
    }

    async fn assert_transport_stopped(pid: &str) {
        // SIGKILL delivery precedes the child's final scheduler transition.
        // Bound that transition instead of racing one immediate /proc read.
        for _ in 0..100 {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            if stat.is_err() || stat.unwrap().split_once(") ").unwrap().1.starts_with('Z') {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("transport child survived client termination");
    }

    #[tokio::test]
    async fn the_subprocess_client_runs_a_real_native_protocol_and_reports_absence() {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("mica-deploy");
        std::fs::write(&stub, format!("#!/bin/sh\ncase \"$1\" in\ncheck) echo '{}' ;;\nfetch) echo '{}' ;;\nprobe) echo 'DATA is not mounted' >&2; exit 1 ;;\n*) echo boom >&2; exit 1 ;;\nesac\n", selection_output().stdout, fetch_output(&staged_path()).stdout)).unwrap();
        std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let client = SubprocessClient::new(stub);
        assert_eq!(client.unavailable(), None);
        let checked = client
            .run(&["check".into()], Duration::from_secs(10))
            .await
            .unwrap();
        assert!(matches!(
            parse_check(&checked),
            Ok(CheckOutcome::Selected(_))
        ));
        let fetched = client
            .run(&["fetch".into()], Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(
            parse_fetch(&fetched, Path::new("/mos/updates/verified")),
            Ok(FetchOutcome::Staged(staged_path()))
        );
        let probed = client
            .run(&["probe".into()], Duration::from_secs(10))
            .await
            .unwrap();
        assert!(matches!(parse_probe(&probed), Ok(ProbeOutcome::Unready(_))));
        let absent = SubprocessClient::new(dir.path().join("gone"));
        assert!(absent.unavailable().is_some());
        assert!(
            absent
                .run(&["status".into()], Duration::from_secs(10))
                .await
                .unwrap_err()
                .is::<ClientUnavailable>()
        );
    }

    /// PLAN-071 §2's `since`/`attempts`, the half a code vocabulary does not
    /// settle: an unbroken refusal must keep its first instant and count its
    /// attempts, and a DIFFERENT refusal must not inherit the clock of the one
    /// it replaced.
    ///
    /// `every_deferral_reaches_the_document_as_a_code_and_nothing_else_does`
    /// covers what the recording site may carry and the clearing-by-code
    /// rules; what is here is the replacement path it resumes past. `since` is
    /// rendered to the second, so a same-second repeat cannot distinguish
    /// "kept" from "reset" on its own — the attempt counter is what pins it.
    #[tokio::test]
    async fn a_repeated_deferral_counts_its_attempts_and_a_changed_reason_starts_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = policy_file(&dir, r#"{"source": {"url": "http://mirror/tuf"}}"#);
        let host = TestHost::new();
        let (lifecycle, _) = lifecycle(MockClient::new(vec![]), policy, Arc::clone(&host));

        lifecycle
            .defer(
                update_codes::DEFER_REBOOT_GATE_CLOSED,
                "exporter reports blocking",
            )
            .await;
        let first = host.last();
        assert_eq!(first["deferred"]["attempts"], 1);
        assert_eq!(first["deferred"]["since"], first["deferred"]["at"]);

        lifecycle
            .defer(
                update_codes::DEFER_REBOOT_GATE_CLOSED,
                "exporter still reports blocking",
            )
            .await;
        let again = host.last();
        assert_eq!(
            again["deferred"]["attempts"], 2,
            "the same reason again extends the fact rather than replacing it"
        );
        assert_eq!(again["deferred"]["since"], first["deferred"]["since"]);
        assert_eq!(
            again["deferred"]["detail"], "exporter still reports blocking",
            "the newest wording of the refusing rule is the one an operator reads"
        );
        assert!(again["deferred"]["waitedSeconds"].is_number());

        // A different reason is a different refusal, and inheriting the first
        // one's clock would report a wait that never happened. No `resume`
        // between the two: this is the replacement path, not the cleared one.
        lifecycle
            .defer(
                update_codes::DEFER_OUTSIDE_WINDOW,
                "outside every configured maintenance window",
            )
            .await;
        let changed = host.last();
        assert_eq!(
            changed["deferred"]["reason"],
            update_codes::DEFER_OUTSIDE_WINDOW
        );
        assert_eq!(changed["deferred"]["attempts"], 1);
        assert_eq!(changed["deferred"]["since"], changed["deferred"]["at"]);
    }
    #[test]
    fn native_acquisition_json_selects_and_stages_only_a_deployment_descriptor() {
        let id = "a".repeat(64);
        let checked = ClientOutput { code: Some(0), stderr: String::new(), stdout: json!({
            "revision":1,"channel":"stable","selected":{"deploymentId":id,"deployment":{"version":"1.0"}}
        }).to_string() };
        assert_eq!(
            parse_check(&checked).unwrap(),
            CheckOutcome::Selected(Available {
                deployment_id: id.to_string(),
                version: "1.0".into(),
                channel: "stable".into(),
            })
        );
        let none = ClientOutput {
            code: Some(0),
            stderr: String::new(),
            stdout: "{\"revision\":2,\"channel\":\"stable\",\"selected\":null}".into(),
        };
        assert_eq!(parse_check(&none).unwrap(), CheckOutcome::NoneCompatible);
        let ready = ClientOutput { code:Some(0),stderr:String::new(),stdout:json!({"id":id,
            "path":format!("/mos/updates/verified/{id}.json"),"objects":"/mos/updates/verified/objects","version":"1.0","generation":3}).to_string() };
        assert_eq!(
            parse_fetch(&ready, Path::new("/mos/updates/verified")).unwrap(),
            FetchOutcome::Staged(format!("/mos/updates/verified/{id}.json"))
        );
        let probe = ClientOutput {
            code: Some(0),
            stderr: String::new(),
            stdout: "{\"status\":\"ready\",\"freeBytes\":200000000}".into(),
        };
        assert!(matches!(
            parse_probe(&probe).unwrap(),
            ProbeOutcome::Ready(_)
        ));
    }
}

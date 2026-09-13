//! zbus client for micad, implementing [`SettingsApi`].
//!
//! APID uses only the `com.mica.micad1` management interface. Application item
//! trees may use direct `com.mica.<class>[.<suffix>]` service names, but enter
//! MQTT only through exact package-owned enrollment and policy.

use std::future::Future;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;
use zbus::export::futures_core::Stream;

use crate::access_cache::{self, AccessCache};
use crate::audit::Audit;
use crate::config::BusKind;
use crate::settings_api::{InvalidTaskPayload, SettingsApi};
use crate::task_registry::{TaskRecord, TaskRegistry};

/// Upper bound for one connection attempt or method call to micad.
///
/// Five seconds is deliberately a fault-containment bound rather than normal
/// flow control: queued writes return promptly, while reads and actions get a
/// finite answer when the bus or daemon wedges.
const MICAD_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// A call crossed [`MICAD_CALL_TIMEOUT`].
///
/// Kept as a concrete error inside `anyhow` so the HTTP boundary can separate
/// "the outcome was not confirmed" (504) from "the daemon was unreachable"
/// (503). In particular, dropping the future does not cancel work already
/// accepted by micad.
#[derive(Debug)]
pub(crate) struct MicadCallTimeout {
    operation: &'static str,
    timeout: Duration,
}

impl std::fmt::Display for MicadCallTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} did not answer within {:?}; the operation may still be running",
            self.operation, self.timeout
        )
    }
}

impl std::error::Error for MicadCallTimeout {}

impl MicadCallTimeout {
    pub(crate) fn new(operation: &'static str, timeout: Duration) -> Self {
        Self { operation, timeout }
    }
}

#[zbus::proxy(
    interface = "com.mica.micad1",
    default_service = "com.mica.micad",
    default_path = "/com/mica/micad"
)]
trait Micad {
    fn get_settings(&self, path: &str) -> zbus::Result<String>;
    fn set_settings(&self, path: &str, value_json: &str) -> zbus::Result<String>;
    fn get_task(&self, id: &str) -> zbus::Result<String>;
    fn get_state(&self, path: &str) -> zbus::Result<String>;
    fn get_network_state(&self) -> zbus::Result<String>;
    fn get_time_status(&self) -> zbus::Result<String>;
    fn get_storage_status(&self) -> zbus::Result<String>;
    fn get_system_info(&self) -> zbus::Result<String>;
    fn get_telemetry(&self) -> zbus::Result<String>;
    fn get_observed_network(&self) -> zbus::Result<String>;
    fn get_failure_evidence(&self) -> zbus::Result<String>;
    fn set_transient_root_password(&self, password: &str) -> zbus::Result<String>;
    fn rotate_wireguard_key(&self, iface: &str) -> zbus::Result<String>;
    fn reboot(&self) -> zbus::Result<()>;
    fn power_off(&self) -> zbus::Result<()>;
    fn get_update_state(&self) -> zbus::Result<String>;
    fn check_update(&self) -> zbus::Result<()>;
    fn fetch_update(&self) -> zbus::Result<()>;
    fn install_update(&self, deployment_id: &str) -> zbus::Result<()>;
    fn confirm_deployment(&self, deployment_id: &str) -> zbus::Result<()>;
    fn reject_deployment(&self, deployment_id: &str) -> zbus::Result<()>;
    fn rollback_deployment(&self, deployment_id: &str) -> zbus::Result<()>;
    fn set_reboot_override(&self, seconds: u32) -> zbus::Result<String>;

    fn set_update_config(&self, patch_json: &str) -> zbus::Result<String>;
    /// Emitted by micad after every successful settings write, with the
    /// changed dot-path and its new JSON-encoded value. The subscriber
    /// ([`watch_settings_changed`]) feeds the auth gate's access cache: the
    /// events invalidate it, which is what makes caching anything read from
    /// micad sound at all.
    #[zbus(signal)]
    fn settings_changed(&self, path: &str, value_json: &str) -> zbus::Result<()>;
    /// Emitted on every apply-task lifecycle transition.
    #[zbus(signal)]
    fn task_changed(&self, task_json: &str) -> zbus::Result<()>;
}

/// How long to wait after a lapsed `SettingsChanged` subscription before
/// dialling again. Short, because while it runs the gate pays a bus round
/// trip per unauthenticated request — the fallback is correct, just costly.
const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(1);

/// Keep `cache` honest against micad's `SettingsChanged` for the daemon's
/// lifetime: subscribe, mark the cache synchronised while the stream is
/// live, and on any lapse drop it back to direct reads and dial again.
///
/// Spawned once by `main.rs`. It holds its own connection rather than
/// sharing [`BusSettings`]'s: the client drops its proxy cache on every
/// failed call, and a signal stream must not die because an unrelated
/// request hit an error.
pub async fn watch_settings_changed(bus: BusKind, cache: Arc<AccessCache>) {
    loop {
        let result = async {
            let connection = match bus {
                BusKind::System => zbus::Connection::system().await,
                BusKind::Session => zbus::Connection::session().await,
            }?;
            watch_connection(&connection, &cache).await
        }
        .await;
        // Unreachable on Ok: the pump only returns by failing. Matched
        // anyway so a future refactor cannot silently turn "stream ended"
        // into "stop watching".
        if let Err(err) = result {
            tracing::warn!(error = %err, "SettingsChanged subscription lapsed");
        }
        cache.lapsed();
        tokio::time::sleep(RESUBSCRIBE_DELAY).await;
    }
}

/// Keep the task registry synchronised from micad's `TaskChanged` stream.
/// A lapse immediately disables memory reads; callers fall back to `GetTask`
/// until a fresh subscription is established.
pub async fn watch_tasks(bus: BusKind, registry: Arc<TaskRegistry>, audit: Arc<Audit>) {
    loop {
        let result = async {
            let connection = match bus {
                BusKind::System => zbus::Connection::system().await,
                BusKind::Session => zbus::Connection::session().await,
            }?;
            watch_task_connection(&connection, &registry, Some(&audit)).await
        }
        .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, "TaskChanged subscription lapsed");
        }
        registry.lapsed();
        tokio::time::sleep(RESUBSCRIBE_DELAY).await;
    }
}

/// Pump one `SettingsChanged` subscription on `connection` until the stream
/// ends, invalidating `cache` on every change that can touch `access`.
///
/// The cache is marked synchronised only AFTER the subscription is
/// established, so no event can fall between the two; the caller owns
/// marking the lapse, whatever way this returns.
pub(crate) async fn watch_connection(
    connection: &zbus::Connection,
    cache: &AccessCache,
) -> anyhow::Result<()> {
    let proxy = MicadProxy::new(connection).await?;
    let stream = proxy.receive_settings_changed().await?;
    cache.subscribed();
    let mut stream = pin!(stream);
    while let Some(signal) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        let args = signal.args()?;
        if access_cache::touches_access(args.path()) {
            cache.invalidate();
        }
    }
    anyhow::bail!("the SettingsChanged stream ended")
}

/// Pump one `TaskChanged` subscription until it ends.
pub(crate) async fn watch_task_connection(
    connection: &zbus::Connection,
    registry: &TaskRegistry,
    audit: Option<&Audit>,
) -> anyhow::Result<()> {
    let proxy = MicadProxy::new(connection).await?;
    let stream = proxy.receive_task_changed().await?;
    registry.subscribed();
    let mut stream = pin!(stream);
    while let Some(signal) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        let args = signal.args()?;
        let task: TaskRecord = serde_json::from_str(args.task_json())?;
        registry.update(task.clone());
        if task.terminal()
            && let Some(audit) = audit
        {
            audit.record(
                "apply-task",
                task.outcome.as_deref().unwrap_or("unknown"),
                &task.source,
            );
        }
    }
    anyhow::bail!("the TaskChanged stream ended")
}

/// Lazily-connected micad client. The proxy is built on first use and cached;
/// any call error drops the cache so the next request reconnects. micad not
/// being up yet therefore surfaces as per-request errors (502 pages), never
/// as an apid crash.
pub struct BusSettings {
    bus: BusKind,
    proxy: Mutex<Option<MicadProxy<'static>>>,
}

impl BusSettings {
    /// Client for the given bus; no connection is attempted yet.
    pub fn new(bus: BusKind) -> Self {
        Self {
            bus,
            proxy: Mutex::new(None),
        }
    }

    /// Return the cached proxy, connecting first when necessary.
    async fn proxy(&self) -> anyhow::Result<MicadProxy<'static>> {
        if let Some(proxy) = self.proxy.lock().await.as_ref() {
            return Ok(proxy.clone());
        }

        // Never hold the cache lock while dialling. Two first requests may
        // build two connections; the second one to publish simply drops its
        // duplicate, which is cheaper and safer than queueing every request
        // behind a stuck connect.
        let connect = async {
            let connection = match self.bus {
                BusKind::System => zbus::Connection::system().await,
                BusKind::Session => zbus::Connection::session().await,
            }?;
            anyhow::Ok(MicadProxy::new(&connection).await?)
        };
        let proxy = tokio::time::timeout(MICAD_CALL_TIMEOUT, connect)
            .await
            .map_err(|_| MicadCallTimeout::new("connect to micad", MICAD_CALL_TIMEOUT))??;

        let mut cached = self.proxy.lock().await;
        if let Some(existing) = cached.as_ref() {
            return Ok(existing.clone());
        }
        *cached = Some(proxy.clone());
        Ok(proxy)
    }

    /// Drop the cached proxy after a failed call.
    async fn reset(&self) {
        *self.proxy.lock().await = None;
    }

    /// Run one proxy call under the shared bound and invalidate the cached
    /// proxy after either an error or an elapsed bound.
    async fn call<T, F>(&self, operation: &'static str, call: F) -> anyhow::Result<T>
    where
        F: Future<Output = zbus::Result<T>>,
    {
        match tokio::time::timeout(MICAD_CALL_TIMEOUT, call).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => {
                self.reset().await;
                Err(err.into())
            }
            Err(_) => {
                self.reset().await;
                Err(MicadCallTimeout::new(operation, MICAD_CALL_TIMEOUT).into())
            }
        }
    }
}

#[cfg(test)]
impl BusSettings {
    /// Client that talks over `connection` instead of dialling for itself, for
    /// the tests that serve a fake micad on a private bus.
    ///
    /// The proxy cache is seeded, so no connect is ever attempted. A failed
    /// call still empties the cache exactly as in production, and the next
    /// call would then dial for real — a test wanting a second call after a
    /// failure needs a second client.
    pub async fn with_connection(connection: &zbus::Connection) -> anyhow::Result<Self> {
        let client = Self::new(BusKind::Session);
        *client.proxy.lock().await = Some(MicadProxy::new(connection).await?);
        Ok(client)
    }
}

#[async_trait::async_trait]
impl SettingsApi for BusSettings {
    async fn get_settings(&self, path: &str) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self.call("GetSettings", proxy.get_settings(path)).await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn set_settings(&self, path: &str, value: &Value) -> anyhow::Result<String> {
        let proxy = self.proxy().await?;
        self.call("SetSettings", proxy.set_settings(path, &value.to_string()))
            .await
    }

    async fn get_task(&self, id: &str) -> anyhow::Result<TaskRecord> {
        let proxy = self.proxy().await?;
        let json = self.call("GetTask", proxy.get_task(id)).await?;
        serde_json::from_str(&json).map_err(|err| InvalidTaskPayload(err).into())
    }

    async fn get_state(&self, path: &str) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self.call("GetState", proxy.get_state(path)).await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn get_network_state(&self) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self
            .call("GetNetworkState", proxy.get_network_state())
            .await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn get_time_status(&self) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self.call("GetTimeStatus", proxy.get_time_status()).await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn get_storage_status(&self) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self
            .call("GetStorageStatus", proxy.get_storage_status())
            .await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn get_system_info(&self) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self.call("GetSystemInfo", proxy.get_system_info()).await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn get_telemetry(&self) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self.call("GetTelemetry", proxy.get_telemetry()).await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn get_observed_network(&self) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self
            .call("GetObservedNetwork", proxy.get_observed_network())
            .await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn get_failure_evidence(&self) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self
            .call("GetFailureEvidence", proxy.get_failure_evidence())
            .await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn reboot(&self) -> anyhow::Result<()> {
        let proxy = self.proxy().await?;
        self.call("Reboot", proxy.reboot()).await
    }

    async fn power_off(&self) -> anyhow::Result<()> {
        let proxy = self.proxy().await?;
        self.call("PowerOff", proxy.power_off()).await
    }

    async fn set_transient_root_password(&self, password: &str) -> anyhow::Result<String> {
        let proxy = self.proxy().await?;
        // The error is returned as micad raised it. micad's own contract is
        // that no message it raises here carries the password, and nothing
        // is added to it on the way back.
        self.call(
            "SetTransientRootPassword",
            proxy.set_transient_root_password(password),
        )
        .await
    }

    /// The answer is micad's, verbatim: the base64 public half of the key it
    /// just drew. There is no private half in the reply and no method on this
    /// proxy that would fetch one.
    async fn rotate_wireguard_key(&self, iface: &str) -> anyhow::Result<String> {
        let proxy = self.proxy().await?;
        self.call("RotateWireguardKey", proxy.rotate_wireguard_key(iface))
            .await
    }

    async fn get_update_state(&self) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self
            .call("GetUpdateState", proxy.get_update_state())
            .await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn check_update(&self) -> anyhow::Result<()> {
        let proxy = self.proxy().await?;
        self.call("CheckUpdate", proxy.check_update()).await
    }

    async fn fetch_update(&self) -> anyhow::Result<()> {
        let proxy = self.proxy().await?;
        self.call("FetchUpdate", proxy.fetch_update()).await
    }

    async fn install_update(&self, bundle: &str) -> anyhow::Result<()> {
        let proxy = self.proxy().await?;
        self.call("InstallUpdate", proxy.install_update(bundle))
            .await
    }

    async fn confirm_deployment(&self, deployment_id: &str) -> anyhow::Result<()> {
        let proxy = self.proxy().await?;
        self.call("ConfirmDeployment", proxy.confirm_deployment(deployment_id))
            .await
    }

    async fn reject_deployment(&self, deployment_id: &str) -> anyhow::Result<()> {
        let proxy = self.proxy().await?;
        self.call("RejectDeployment", proxy.reject_deployment(deployment_id))
            .await
    }

    async fn rollback_deployment(&self, deployment_id: &str) -> anyhow::Result<()> {
        let proxy = self.proxy().await?;
        self.call(
            "RollbackDeployment",
            proxy.rollback_deployment(deployment_id),
        )
        .await
    }

    async fn set_reboot_override(&self, seconds: u32) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self
            .call("SetRebootOverride", proxy.set_reboot_override(seconds))
            .await?;
        Ok(serde_json::from_str(&json)?)
    }

    async fn set_update_config(&self, patch: &Value) -> anyhow::Result<Value> {
        let proxy = self.proxy().await?;
        let json = self
            .call(
                "SetUpdateConfig",
                proxy.set_update_config(&patch.to_string()),
            )
            .await?;
        Ok(serde_json::from_str(&json)?)
    }
}

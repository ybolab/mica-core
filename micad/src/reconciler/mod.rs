//! Reconciler contract shared by all micad reconcilers.

mod container;
mod hostname;
mod mqtt;
/// Visible to the daemon rather than to this module alone: the WireGuard key
/// rotation the bus surfaces ([`network::WireguardRotate`]) is this
/// reconciler's mechanism reached from outside a reconcile.
pub mod network;
mod sshd;
mod systemd;
mod time;
mod wifi_ap;
mod wifi_client;

use micad_settings::Settings;

#[async_trait::async_trait]
pub trait Reconciler: Send + Sync {
    /// Stable name; also this reconciler's key in the live-state tree (e.g. "hostname", "network").
    fn name(&self) -> &'static str;
    /// Dot-path prefix of the settings subtree this reconciler watches (e.g. "hostname", "network").
    fn subtree(&self) -> &'static str;
    /// Apply `settings` to the system; return the applied live-state as JSON.
    async fn apply(&self, settings: &Settings) -> anyhow::Result<serde_json::Value>;
}

/// All reconcilers compiled into micad with production executors.
///
/// Safe to call anywhere: executors connect to the system bus lazily, so
/// nothing touches the host until a reconciler's `apply` runs.
pub fn all() -> Vec<Box<dyn Reconciler>> {
    vec![
        Box::new(hostname::HostnameReconciler::new(
            hostname::Hostnamed::production(),
        )),
        Box::new(network::NetworkReconciler::production()),
        Box::new(sshd::SshdReconciler::production()),
        Box::new(wifi_client::WifiClientReconciler::production()),
        Box::new(wifi_ap::WifiApReconciler::production()),
        Box::new(container::ContainerReconciler::production()),
        Box::new(mqtt::MqttReconciler::production()),
        Box::new(time::TimeReconciler::production()),
    ]
}

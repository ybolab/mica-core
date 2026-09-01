//! Read-only observation of the network state owned by systemd-networkd.
//!
//! Configuration remains in the settings tree. This module reports what the
//! running network manager actually sees, so callers never have to infer link
//! health from generated `.network` files.

use std::time::Duration;

use anyhow::Context;
use serde_json::{Map, Value};

const OBSERVE_TIMEOUT: Duration = Duration::from_secs(3);

#[async_trait::async_trait]
pub trait NetworkState: Send + Sync {
    async fn describe(&self) -> anyhow::Result<Value>;
}

/// Safe default for tests and dry-run daemons.
pub struct UnavailableNetworkState;

#[async_trait::async_trait]
impl NetworkState for UnavailableNetworkState {
    async fn describe(&self) -> anyhow::Result<Value> {
        anyhow::bail!("network observation is not configured")
    }
}

/// Production observer backed by `org.freedesktop.network1.Manager.Describe`.
pub struct SystemdNetworkState;

#[zbus::proxy(
    interface = "org.freedesktop.network1.Manager",
    default_service = "org.freedesktop.network1",
    default_path = "/org/freedesktop/network1"
)]
trait NetworkManager {
    fn describe(&self) -> zbus::Result<String>;
}

#[async_trait::async_trait]
impl NetworkState for SystemdNetworkState {
    async fn describe(&self) -> anyhow::Result<Value> {
        tokio::time::timeout(OBSERVE_TIMEOUT, async {
            let connection = zbus::Connection::system()
                .await
                .context("connect to the system bus for networkd")?;
            let proxy = NetworkManagerProxy::new(&connection)
                .await
                .context("connect to systemd-networkd")?;
            let json = proxy.describe().await.context("networkd Describe")?;
            let value: Value = serde_json::from_str(&json).context("parse networkd Describe")?;
            normalize(value)
        })
        .await
        .context("networkd observation timed out")?
    }
}

fn normalize(value: Value) -> anyhow::Result<Value> {
    let interfaces = value
        .get("Interfaces")
        .and_then(Value::as_array)
        .context("networkd Describe has no Interfaces array")?;
    let interfaces: Vec<Value> = interfaces.iter().filter_map(normalize_interface).collect();
    Ok(serde_json::json!({
        "interfaceCount": interfaces.len(),
        "interfaces": interfaces,
    }))
}

fn normalize_interface(source: &Value) -> Option<Value> {
    let source = source.as_object()?;
    let mut target = Map::new();
    for (from, to) in [
        ("Index", "index"),
        ("Name", "name"),
        ("Kind", "kind"),
        ("Type", "type"),
        ("Driver", "driver"),
        ("AdministrativeState", "administrativeState"),
        ("OperationalState", "operationalState"),
        ("CarrierState", "carrierState"),
        ("AddressState", "addressState"),
        ("IPv4AddressState", "ipv4AddressState"),
        ("IPv6AddressState", "ipv6AddressState"),
        ("OnlineState", "onlineState"),
        ("MTU", "mtu"),
        ("HardwareAddress", "hardwareAddress"),
        ("Addresses", "addresses"),
        ("DNS", "dns"),
        ("Routes", "routes"),
    ] {
        if let Some(value) = source.get(from) {
            target.insert(to.to_string(), value.clone());
        }
    }
    Some(Value::Object(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_is_reduced_to_stable_interface_details() {
        let normalized = normalize(serde_json::json!({
            "Interfaces": [{
                "Index": 2,
                "Name": "eth0",
                "OperationalState": "routable",
                "CarrierState": "carrier",
                "Addresses": [{"Address": [192, 0, 2, 10], "PrefixLength": 24}],
                "UnstableFutureField": "ignored"
            }]
        }))
        .unwrap();
        assert_eq!(normalized["interfaceCount"], 1);
        assert_eq!(normalized["interfaces"][0]["name"], "eth0");
        assert_eq!(normalized["interfaces"][0]["operationalState"], "routable");
        assert!(normalized["interfaces"][0]["UnstableFutureField"].is_null());
    }
}

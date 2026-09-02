//! Read-only observation of the network state owned by systemd-networkd,
//! wpa_supplicant and systemd-resolved.
//!
//! Configuration remains in the settings tree. This module reports what the
//! running network stack actually sees, so callers never have to infer link
//! health from generated `.network` files. Two reads are served:
//!
//! - [`NetworkState::describe`], the reduced per-interface view the
//!   `/api/v1/network` overview has always carried beside the desired map.
//! - [`NetworkState::observe`], the OBSERVED state PLAN-052 asks for as its
//!   own surface (`GetObservedNetwork`, `GET /api/v1/network/status`):
//!   link/carrier, addresses, the DHCP lease, the default routes, DNS
//!   reachability, Wi-Fi association, and the radio and modem capabilities
//!   this image does and does not support. Desired settings never appear in
//!   it; the two are distinct types on distinct routes, by design.
//!
//! **Absence is data.** networkd not answering leaves `interfaces` absent
//! with the reason and still reports the radios; a wireless interface with no
//! control socket reports that it could not be asked; a board with no radio
//! says `supported: false`. Nothing is inferred to be healthy.
//!
//! Every parser here is pure and driven by fixtures; the production observer
//! is rooted at a path prefix so the sysfs and control-socket reads can be
//! driven over a fixture tree too. The DNS probe is the one thing that
//! touches the network: a single bounded lookup of [`DNS_PROBE_NAME`] through
//! resolved, a name the image already depends on (timesyncd's fallback pool).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use serde_json::{Map, Value, json};

/// How long networkd's `Describe` may take.
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long one wpa_supplicant control-socket exchange may take.
pub const WPA_TIMEOUT: Duration = Duration::from_secs(1);
/// How long the DNS probe may take, end to end.
pub const DNS_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// The name the DNS probe resolves: timesyncd's fallback pool, an outbound
/// dependency the image already has, so the probe adds no new one.
pub const DNS_PROBE_NAME: &str = "0.debian.pool.ntp.org";
/// wpa_supplicant's control-socket directory, relative to the root.
pub const WPA_CONTROL_DIR: &str = "run/wpa_supplicant";
/// Network interfaces, relative to the root.
pub const NET_CLASS_DIR: &str = "sys/class/net";
/// Bluetooth adapters, relative to the root.
pub const BLUETOOTH_CLASS_DIR: &str = "sys/class/bluetooth";
/// The most wireless interfaces asked about; each costs a socket exchange.
pub const MAX_WIFI_INTERFACES: usize = 4;

#[async_trait::async_trait]
pub trait NetworkState: Send + Sync {
    /// The reduced per-interface view networkd's `Describe` yields.
    async fn describe(&self) -> anyhow::Result<Value>;
    /// The observed network state, rendered ([`observed_json`]).
    async fn observe(&self) -> anyhow::Result<Value>;
}

/// Safe default for tests and dry-run daemons.
pub struct UnavailableNetworkState;

#[async_trait::async_trait]
impl NetworkState for UnavailableNetworkState {
    async fn describe(&self) -> anyhow::Result<Value> {
        anyhow::bail!("network observation is not configured")
    }

    async fn observe(&self) -> anyhow::Result<Value> {
        anyhow::bail!("network observation is not configured")
    }
}

/// Production observer backed by `org.freedesktop.network1.Manager.Describe`,
/// wpa_supplicant's control sockets and resolved.
pub struct SystemdNetworkState {
    root: PathBuf,
    probe_dns: bool,
}

impl SystemdNetworkState {
    /// The observer `main.rs` attaches on a device: rooted at `/`, DNS
    /// probe on.
    #[must_use]
    pub fn production() -> Self {
        Self::at("/").with_dns_probe(true)
    }

    /// An observer rooted at `root`, with NO DNS probe: nothing here touches
    /// the network until the probe is switched on, so a fixture tree cannot
    /// make a test resolve a name against the machine running it.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            probe_dns: false,
        }
    }

    /// Whether [`NetworkState::observe`] runs the resolved probe.
    #[must_use]
    pub fn with_dns_probe(mut self, probe: bool) -> Self {
        self.probe_dns = probe;
        self
    }

    async fn observe_wifi(&self, interfaces: &[String]) -> WifiEvidence {
        let control_dir = self.root.join(WPA_CONTROL_DIR);
        let mut evidence = WifiEvidence {
            control_dir_present: control_dir.is_dir(),
            ..WifiEvidence::default()
        };
        for interface in interfaces.iter().take(MAX_WIFI_INTERFACES) {
            let dir = control_dir.clone();
            let name = interface.clone();
            let status =
                tokio::task::spawn_blocking(move || wpa_query(&dir, &name, "STATUS", WPA_TIMEOUT))
                    .await
                    .map_err(|err| std::io::Error::other(err.to_string()))
                    .and_then(|inner| inner);
            match status {
                Ok(text) => {
                    let mut association = parse_wpa_status(interface, &text);
                    let dir = control_dir.clone();
                    let name = interface.clone();
                    if let Ok(Ok(poll)) = tokio::task::spawn_blocking(move || {
                        wpa_query(&dir, &name, "SIGNAL_POLL", WPA_TIMEOUT)
                    })
                    .await
                    {
                        apply_signal_poll(&mut association, &poll);
                    }
                    evidence.associations.push(association);
                }
                Err(err) => evidence.associations.push(WifiAssociation {
                    interface: interface.clone(),
                    detail: Some(format!("wpa_supplicant could not be asked: {err}")),
                    ..WifiAssociation::default()
                }),
            }
        }
        if let Ok(text) = std::fs::read_to_string(self.root.join("proc/net/wireless")) {
            let levels = parse_proc_net_wireless(&text);
            for association in &mut evidence.associations {
                if association.rssi_dbm.is_none() {
                    association.rssi_dbm = levels.get(&association.interface).copied();
                }
            }
        }
        evidence
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.network1.Manager",
    default_service = "org.freedesktop.network1",
    default_path = "/org/freedesktop/network1"
)]
trait NetworkManager {
    fn describe(&self) -> zbus::Result<String>;
}

/// networkd's `Describe`, parsed and otherwise untouched.
async fn describe_raw() -> anyhow::Result<Value> {
    tokio::time::timeout(OBSERVE_TIMEOUT, async {
        let connection = zbus::Connection::system()
            .await
            .context("connect to the system bus for networkd")?;
        let proxy = NetworkManagerProxy::new(&connection)
            .await
            .context("connect to systemd-networkd")?;
        let json = proxy.describe().await.context("networkd Describe")?;
        serde_json::from_str(&json).context("parse networkd Describe")
    })
    .await
    .context("networkd observation timed out")?
}

#[async_trait::async_trait]
impl NetworkState for SystemdNetworkState {
    async fn describe(&self) -> anyhow::Result<Value> {
        normalize(describe_raw().await?)
    }

    async fn observe(&self) -> anyhow::Result<Value> {
        let describe = describe_raw().await.map_err(|err| format!("{err:#}"));
        let radios = radio_evidence(&self.root);
        let wifi = self.observe_wifi(&radios.wifi_interfaces).await;
        let dns = if self.probe_dns {
            Some(observe_dns().await)
        } else {
            None
        };
        Ok(observed_json(
            describe.as_ref().map_err(String::as_str),
            &wifi,
            dns.as_ref(),
            &radios,
        ))
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

// ---- the observed surface -------------------------------------------------

/// One wireless interface's association, as wpa_supplicant reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WifiAssociation {
    /// The interface name.
    pub interface: String,
    /// `wpa_state`: `COMPLETED`, `SCANNING`, `DISCONNECTED`, ...
    pub state: Option<String>,
    /// The associated network's SSID.
    pub ssid: Option<String>,
    /// The associated access point.
    pub bssid: Option<String>,
    /// The channel frequency in MHz.
    pub frequency_mhz: Option<u32>,
    /// `key_mgmt`: `WPA2-PSK`, `SAE`, ...
    pub key_management: Option<String>,
    /// The received signal strength, dBm.
    pub rssi_dbm: Option<i32>,
    /// The current link speed, Mb/s.
    pub link_speed_mbps: Option<u32>,
    /// Why nothing could be observed, when nothing could.
    pub detail: Option<String>,
}

/// What the wireless observation produced.
#[derive(Debug, Clone, Default)]
pub struct WifiEvidence {
    /// Whether wpa_supplicant's control directory exists at all.
    pub control_dir_present: bool,
    /// One entry per wireless interface asked.
    pub associations: Vec<WifiAssociation>,
}

/// Parse wpa_supplicant's `STATUS` reply (`key=value` lines) into an
/// association. Only the association facts are read: the interface's own
/// hardware address (`address=`) is networkd's to report, and nothing here
/// carries a credential.
#[must_use]
pub fn parse_wpa_status(interface: &str, text: &str) -> WifiAssociation {
    let mut association = WifiAssociation {
        interface: interface.to_string(),
        ..WifiAssociation::default()
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "wpa_state" => association.state = Some(value.to_string()),
            "ssid" => association.ssid = Some(value.to_string()),
            "bssid" => association.bssid = Some(value.to_string()),
            "freq" => association.frequency_mhz = value.parse().ok(),
            "key_mgmt" => association.key_management = Some(value.to_string()),
            _ => {}
        }
    }
    if association.state.is_none() {
        association.detail = Some("wpa_supplicant answered without a wpa_state".to_string());
    }
    association
}

/// Fold wpa_supplicant's `SIGNAL_POLL` reply into `association`.
pub fn apply_signal_poll(association: &mut WifiAssociation, text: &str) {
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "RSSI" => association.rssi_dbm = value.trim().parse().ok(),
            "LINKSPEED" => association.link_speed_mbps = value.trim().parse().ok(),
            "FREQUENCY" if association.frequency_mhz.is_none() => {
                association.frequency_mhz = value.trim().parse().ok();
            }
            _ => {}
        }
    }
}

/// Parse `/proc/net/wireless` into interface → signal level (dBm).
///
/// The level column is the third numeric field after the interface name
/// (status, link, level); the kernel prints it with a trailing `.`.
#[must_use]
pub fn parse_proc_net_wireless(text: &str) -> BTreeMap<String, i32> {
    let mut levels = BTreeMap::new();
    for line in text.lines().skip(2) {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        let name = name.trim_end_matches(':');
        let level = fields
            .nth(2)
            .and_then(|field| field.trim_end_matches('.').parse::<f64>().ok());
        if let Some(level) = level {
            levels.insert(name.to_string(), level as i32);
        }
    }
    levels
}

static CLIENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One request/reply exchange on wpa_supplicant's control socket for
/// `interface`, blocking, under `timeout`.
///
/// The control interface is a Unix datagram socket that replies to the
/// SENDER's address, so the client has to bind a socket of its own; it lives
/// in the temporary directory for the duration of the exchange and is
/// removed afterwards whether or not the exchange succeeded.
fn wpa_query(
    control_dir: &Path,
    interface: &str,
    command: &str,
    timeout: Duration,
) -> std::io::Result<String> {
    use std::os::unix::net::UnixDatagram;
    let client_path = std::env::temp_dir().join(format!(
        "mosd-wpa-{}-{}",
        std::process::id(),
        CLIENT_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&client_path);
    let socket = UnixDatagram::bind(&client_path)?;
    let exchange = (|| {
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        socket.connect(control_dir.join(interface))?;
        socket.send(command.as_bytes())?;
        let mut buffer = vec![0u8; 4096];
        let received = socket.recv(&mut buffer)?;
        Ok(String::from_utf8_lossy(&buffer[..received]).into_owned())
    })();
    let _ = std::fs::remove_file(&client_path);
    exchange
}

/// The radios and modems sysfs shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RadioEvidence {
    /// Interfaces backed by a `phy80211` device.
    pub wifi_interfaces: Vec<String>,
    /// Adapters under `/sys/class/bluetooth`.
    pub bluetooth_adapters: Vec<String>,
    /// Interfaces the kernel types `wwan`.
    pub wwan_interfaces: Vec<String>,
}

fn sorted_entries(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// Discover the radios under `root`.
#[must_use]
pub fn radio_evidence(root: &Path) -> RadioEvidence {
    let net = root.join(NET_CLASS_DIR);
    let mut evidence = RadioEvidence::default();
    for name in sorted_entries(&net) {
        let iface = net.join(&name);
        if iface.join("phy80211").exists() || iface.join("wireless").exists() {
            evidence.wifi_interfaces.push(name.clone());
        }
        let is_wwan = name.starts_with("wwan")
            || std::fs::read_to_string(iface.join("uevent"))
                .is_ok_and(|uevent| uevent.lines().any(|line| line == "DEVTYPE=wwan"));
        if is_wwan {
            evidence.wwan_interfaces.push(name);
        }
    }
    evidence.bluetooth_adapters = sorted_entries(&root.join(BLUETOOTH_CLASS_DIR));
    evidence
}

/// What the DNS probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsOutcome {
    /// The name resolved to this many addresses.
    Resolved { addresses: usize },
    /// resolved answered with an error, named.
    Failed(String),
    /// The probe crossed [`DNS_PROBE_TIMEOUT`].
    TimedOut,
}

/// What resolved reported.
#[derive(Debug, Clone, Default)]
pub struct DnsEvidence {
    /// Whether `org.freedesktop.resolve1` answered at all.
    pub resolver_reachable: bool,
    /// The servers resolved currently uses, formatted.
    pub servers: Vec<String>,
    /// The probe's name and outcome, when the probe ran.
    pub probe: Option<(String, DnsOutcome)>,
}

/// resolved's `DNS` property and `ResolveHostname`, each soft.
async fn observe_dns() -> DnsEvidence {
    let mut evidence = DnsEvidence::default();
    let Ok(connection) = zbus::Connection::system().await else {
        return evidence;
    };
    let Ok(manager) = zbus::Proxy::new(
        &connection,
        "org.freedesktop.resolve1",
        "/org/freedesktop/resolve1",
        "org.freedesktop.resolve1.Manager",
    )
    .await
    else {
        return evidence;
    };
    if let Ok(servers) = manager
        .get_property::<Vec<(i32, i32, Vec<u8>)>>("DNS")
        .await
    {
        evidence.resolver_reachable = true;
        evidence.servers = servers
            .iter()
            .filter_map(|(_, family, bytes)| format_address(Some(i64::from(*family)), bytes))
            .collect();
    }
    let probe = tokio::time::timeout(
        DNS_PROBE_TIMEOUT,
        manager.call::<_, _, (Vec<(i32, i32, Vec<u8>)>, String, u64)>(
            "ResolveHostname",
            &(0i32, DNS_PROBE_NAME, 0i32, 0u64),
        ),
    )
    .await;
    let outcome = match probe {
        Err(_) => DnsOutcome::TimedOut,
        Ok(Ok((addresses, _, _))) => {
            evidence.resolver_reachable = true;
            DnsOutcome::Resolved {
                addresses: addresses.len(),
            }
        }
        Ok(Err(err)) => {
            if let zbus::Error::MethodError(name, message, _) = &err {
                evidence.resolver_reachable = true;
                DnsOutcome::Failed(format!("{name}: {}", message.clone().unwrap_or_default()))
            } else {
                DnsOutcome::Failed(err.to_string())
            }
        }
    };
    evidence.probe = Some((DNS_PROBE_NAME.to_string(), outcome));
    evidence
}

/// Format an address family and raw bytes the way `networkctl` prints them.
///
/// The family is optional because the byte length alone names it when the
/// source omits the family (networkd's `HardwareAddress` has none).
#[must_use]
pub fn format_address(family: Option<i64>, bytes: &[u8]) -> Option<String> {
    match (family, bytes.len()) {
        (Some(2) | None, 4) => {
            Some(std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string())
        }
        (Some(10) | None, 16) => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Some(std::net::Ipv6Addr::from(octets).to_string())
        }
        _ => None,
    }
}

/// A byte array as JSON numbers → bytes.
fn bytes_of(value: &Value) -> Option<Vec<u8>> {
    value
        .as_array()?
        .iter()
        .map(|item| u8::try_from(item.as_u64()?).ok())
        .collect()
}

fn family_name(family: Option<i64>, bytes_len: usize) -> &'static str {
    match (family, bytes_len) {
        (Some(2), _) | (None, 4) => "ipv4",
        (Some(10), _) | (None, 16) => "ipv6",
        _ => "unknown",
    }
}

/// networkd's `HardwareAddress` (an array of octets) as colon-separated hex.
fn hardware_address(link: &Value) -> Option<String> {
    let bytes = bytes_of(link.get("HardwareAddress")?)?;
    if bytes.is_empty() {
        return None;
    }
    Some(
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// An address the way `Addresses` and a lease spell it: `Family` + `Address`
/// bytes, or just bytes.
fn address_of(entry: &Value, key: &str) -> Option<(String, &'static str)> {
    let bytes = bytes_of(entry.get(key)?)?;
    let family = entry.get("Family").and_then(Value::as_i64);
    let name = family_name(family, bytes.len());
    Some((format_address(family, &bytes)?, name))
}

fn address_json(entry: &Value) -> Option<Value> {
    let (address, family) = address_of(entry, "Address")?;
    let mut root = Map::new();
    root.insert("family".to_string(), json!(family));
    root.insert("address".to_string(), json!(address));
    if let Some(prefix) = entry.get("PrefixLength").and_then(Value::as_u64) {
        root.insert("prefixLength".to_string(), json!(prefix));
    }
    if let Some(scope) = entry.get("ScopeString").and_then(Value::as_str) {
        root.insert("scope".to_string(), json!(scope));
    }
    if let Some(source) = entry.get("ConfigSource").and_then(Value::as_str) {
        root.insert("configSource".to_string(), json!(source));
    }
    Some(Value::Object(root))
}

/// Whether a route is a default route: destination prefix length zero.
fn is_default_route(route: &Value) -> bool {
    match route.get("DestinationPrefixLength").and_then(Value::as_u64) {
        Some(length) => length == 0,
        None => route
            .get("Destination")
            .and_then(bytes_of)
            .is_some_and(|bytes| bytes.iter().all(|byte| *byte == 0)),
    }
}

fn default_route_json(route: &Value, interface: Option<&str>, index: Option<u64>) -> Value {
    let family = route.get("Family").and_then(Value::as_i64);
    let gateway = route
        .get("Gateway")
        .and_then(bytes_of)
        .and_then(|bytes| format_address(family, &bytes));
    let mut root = Map::new();
    root.insert(
        "family".to_string(),
        json!(family_name(
            family,
            route
                .get("Gateway")
                .and_then(bytes_of)
                .map_or(0, |b| b.len())
        )),
    );
    root.insert("gateway".to_string(), json!(gateway));
    root.insert("interface".to_string(), json!(interface));
    root.insert("interfaceIndex".to_string(), json!(index));
    if let Some(metric) = route.get("Priority").and_then(Value::as_u64) {
        root.insert("metric".to_string(), json!(metric));
    }
    if let Some(protocol) = route.get("ProtocolString").and_then(Value::as_str) {
        root.insert("protocol".to_string(), json!(protocol));
    }
    if let Some(table) = route.get("TableString").and_then(Value::as_str) {
        root.insert("table".to_string(), json!(table));
    }
    if let Some(source) = route.get("ConfigSource").and_then(Value::as_str) {
        root.insert("configSource".to_string(), json!(source));
    }
    Value::Object(root)
}

fn absent(detail: impl Into<String>) -> Value {
    json!({ "available": false, "detail": detail.into() })
}

/// The `dhcp` member of one interface: the DHCPv4 client's state and lease
/// when networkd reports a client, else — because the `Addresses` list says
/// where each address came from — an inferred lease from a DHCPv4-sourced
/// address, marked as inferred, else absent.
fn dhcp_json(link: &Value) -> Value {
    if let Some(client) = link.get("DHCPv4Client").and_then(Value::as_object) {
        let mut root = Map::new();
        root.insert("available".to_string(), json!(true));
        root.insert("inferred".to_string(), json!(false));
        if let Some(state) = client.get("State").and_then(Value::as_str) {
            root.insert("state".to_string(), json!(state));
        }
        match client.get("Lease").and_then(Value::as_object) {
            Some(lease) => {
                let mut lease_json = Map::new();
                let lease_value = Value::Object(lease.clone());
                if let Some((address, _)) = address_of(&lease_value, "Address") {
                    lease_json.insert("address".to_string(), json!(address));
                }
                if let Some(prefix) = lease.get("PrefixLength").and_then(Value::as_u64) {
                    lease_json.insert("prefixLength".to_string(), json!(prefix));
                }
                if let Some((server, _)) = address_of(&lease_value, "ServerAddress") {
                    lease_json.insert("server".to_string(), json!(server));
                }
                if let Some(router) = lease
                    .get("Router")
                    .and_then(Value::as_array)
                    .and_then(|routers| routers.first())
                    .and_then(bytes_of)
                    .and_then(|bytes| format_address(None, &bytes))
                {
                    lease_json.insert("router".to_string(), json!(router));
                }
                if let Some(lifetime) = lease.get("LifetimeUSec").and_then(Value::as_u64) {
                    lease_json.insert("lifetimeSeconds".to_string(), json!(lifetime / 1_000_000));
                }
                root.insert("lease".to_string(), Value::Object(lease_json));
            }
            None => {
                root.insert(
                    "lease".to_string(),
                    absent("the DHCPv4 client holds no lease"),
                );
            }
        }
        return Value::Object(root);
    }
    let leased = link
        .get("Addresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|entry| entry.get("ConfigSource").and_then(Value::as_str) == Some("DHCPv4"));
    match leased {
        Some(entry) => {
            let mut lease = Map::new();
            if let Some((address, _)) = address_of(entry, "Address") {
                lease.insert("address".to_string(), json!(address));
            }
            if let Some(prefix) = entry.get("PrefixLength").and_then(Value::as_u64) {
                lease.insert("prefixLength".to_string(), json!(prefix));
            }
            if let Some(server) = entry
                .get("ConfigProvider")
                .and_then(bytes_of)
                .and_then(|bytes| format_address(None, &bytes))
            {
                lease.insert("server".to_string(), json!(server));
            }
            json!({
                "available": true,
                "inferred": true,
                "detail": "networkd reports no DHCPv4 client object; the lease is inferred from a DHCPv4-sourced address",
                "lease": Value::Object(lease),
            })
        }
        None => absent("no DHCPv4 client and no DHCPv4-sourced address on this interface"),
    }
}

fn wifi_json(association: &WifiAssociation) -> Value {
    let mut root = Map::new();
    root.insert("interface".to_string(), json!(association.interface));
    match &association.detail {
        Some(detail) if association.state.is_none() => {
            root.insert("available".to_string(), json!(false));
            root.insert("detail".to_string(), json!(detail));
            return Value::Object(root);
        }
        _ => {}
    }
    root.insert("available".to_string(), json!(true));
    root.insert("state".to_string(), json!(association.state));
    root.insert(
        "associated".to_string(),
        json!(association.state.as_deref() == Some("COMPLETED")),
    );
    if let Some(ssid) = &association.ssid {
        root.insert("ssid".to_string(), json!(ssid));
    }
    if let Some(bssid) = &association.bssid {
        root.insert("bssid".to_string(), json!(bssid));
    }
    if let Some(frequency) = association.frequency_mhz {
        root.insert("frequencyMhz".to_string(), json!(frequency));
    }
    if let Some(key_management) = &association.key_management {
        root.insert("keyManagement".to_string(), json!(key_management));
    }
    if let Some(rssi) = association.rssi_dbm {
        root.insert("rssiDbm".to_string(), json!(rssi));
    }
    if let Some(speed) = association.link_speed_mbps {
        root.insert("linkSpeedMbps".to_string(), json!(speed));
    }
    Value::Object(root)
}

fn interface_json(link: &Value, wifi: Option<&WifiAssociation>) -> Value {
    let mut root = Map::new();
    for (from, to) in [
        ("Name", "name"),
        ("Index", "index"),
        ("Kind", "kind"),
        ("Type", "type"),
        ("Driver", "driver"),
        ("MTU", "mtu"),
    ] {
        if let Some(value) = link.get(from) {
            root.insert(to.to_string(), value.clone());
        }
    }
    let carrier = link.get("CarrierState").and_then(Value::as_str);
    root.insert(
        "link".to_string(),
        json!({
            "administrativeState": link.get("AdministrativeState"),
            "operationalState": link.get("OperationalState"),
            "carrierState": carrier,
            "carrier": carrier.map(|state| state == "carrier" || state == "enslaved"),
            "onlineState": link.get("OnlineState"),
            "addressState": link.get("AddressState"),
        }),
    );
    if let Some(address) = hardware_address(link) {
        root.insert("hardwareAddress".to_string(), json!(address));
    }
    let addresses: Vec<Value> = link
        .get("Addresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(address_json)
        .collect();
    root.insert("addresses".to_string(), Value::Array(addresses));
    root.insert("dhcp".to_string(), dhcp_json(link));
    let dns: Vec<Value> = link
        .get("DNS")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| address_of(entry, "Address").map(|(address, _)| json!(address)))
        .collect();
    root.insert("dns".to_string(), Value::Array(dns));
    if let Some(association) = wifi {
        root.insert("wifi".to_string(), wifi_json(association));
    }
    Value::Object(root)
}

/// The `dns` member: the per-link servers networkd reports, the resolver's
/// own list and the probe.
fn dns_json(evidence: Option<&DnsEvidence>, link_servers: Vec<String>) -> Value {
    let mut root = Map::new();
    root.insert("linkServers".to_string(), json!(link_servers));
    match evidence {
        None => {
            root.insert("available".to_string(), json!(false));
            root.insert(
                "detail".to_string(),
                json!("the resolver was not asked: this observer runs no DNS probe"),
            );
        }
        Some(evidence) => {
            root.insert("available".to_string(), json!(evidence.resolver_reachable));
            if !evidence.resolver_reachable {
                root.insert(
                    "detail".to_string(),
                    json!("systemd-resolved is not reachable on the bus"),
                );
            }
            root.insert("resolverServers".to_string(), json!(evidence.servers));
            root.insert(
                "probe".to_string(),
                match &evidence.probe {
                    None => absent("no probe ran"),
                    Some((name, outcome)) => {
                        let (reachable, result, detail) = match outcome {
                            DnsOutcome::Resolved { addresses } => {
                                (true, "resolved", format!("{addresses} address(es)"))
                            }
                            DnsOutcome::Failed(detail) => (false, "failed", detail.clone()),
                            DnsOutcome::TimedOut => (
                                false,
                                "timeout",
                                format!("no answer within {DNS_PROBE_TIMEOUT:?}"),
                            ),
                        };
                        json!({
                            "available": true,
                            "name": name,
                            "reachable": reachable,
                            "result": result,
                            "detail": detail,
                        })
                    }
                },
            );
        }
    }
    Value::Object(root)
}

/// The observed network state, rendered.
///
/// `describe` is networkd's raw `Describe` or the reason it is absent;
/// `wifi`, `dns` and `radios` are what the other observers produced. Every
/// member carries `available`; an unavailable one carries the reason.
#[must_use]
pub fn observed_json(
    describe: Result<&Value, &str>,
    wifi: &WifiEvidence,
    dns: Option<&DnsEvidence>,
    radios: &RadioEvidence,
) -> Value {
    let links: Vec<&Value> = describe
        .ok()
        .and_then(|value| value.get("Interfaces"))
        .and_then(Value::as_array)
        .map(|interfaces| interfaces.iter().collect())
        .unwrap_or_default();
    let association_for = |name: Option<&str>| {
        name.and_then(|name| {
            wifi.associations
                .iter()
                .find(|association| association.interface == name)
        })
    };
    let interfaces = match describe {
        Err(detail) => absent(format!("systemd-networkd did not answer: {detail}")),
        Ok(_) => {
            let entries: Vec<Value> = links
                .iter()
                .map(|link| {
                    interface_json(
                        link,
                        association_for(link.get("Name").and_then(Value::as_str)),
                    )
                })
                .collect();
            json!({ "available": true, "count": entries.len(), "entries": entries })
        }
    };
    let default_routes = match describe {
        Err(detail) => absent(format!("systemd-networkd did not answer: {detail}")),
        Ok(_) => {
            let entries: Vec<Value> = links
                .iter()
                .flat_map(|link| {
                    let name = link.get("Name").and_then(Value::as_str);
                    let index = link.get("Index").and_then(Value::as_u64);
                    link.get("Routes")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|route| is_default_route(route))
                        .map(move |route| default_route_json(route, name, index))
                })
                .collect();
            json!({ "available": true, "count": entries.len(), "entries": entries })
        }
    };
    let link_servers: Vec<String> = links
        .iter()
        .flat_map(|link| {
            link.get("DNS")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|entry| address_of(entry, "Address").map(|(address, _)| address))
        })
        .collect();
    let wifi_member = if radios.wifi_interfaces.is_empty() {
        absent("no wireless interface on this board")
    } else if !wifi.control_dir_present {
        json!({
            "available": false,
            "detail": "wpa_supplicant's control directory is absent: no station is running",
            "interfaces": radios.wifi_interfaces,
        })
    } else {
        json!({
            "available": true,
            "associations": wifi.associations.iter().map(wifi_json).collect::<Vec<_>>(),
        })
    };
    json!({
        "interfaces": interfaces,
        "defaultRoutes": default_routes,
        "dns": dns_json(dns, link_servers),
        "wifi": wifi_member,
        "capabilities": {
            "wifi": {
                "supported": !radios.wifi_interfaces.is_empty(),
                "interfaces": radios.wifi_interfaces,
                "detail": if radios.wifi_interfaces.is_empty() {
                    "no phy80211 device under /sys/class/net"
                } else {
                    "station and access point are driven by the wifi reconcilers"
                },
            },
            "bluetooth": {
                "supported": !radios.bluetooth_adapters.is_empty(),
                "adapters": radios.bluetooth_adapters,
                "detail": "adapter presence only; no Bluetooth state is observed by this surface",
            },
            "cellular": {
                "supported": false,
                "interfaces": radios.wwan_interfaces,
                "detail": "no cellular modem support in this image: SKU-specific per PLAN-052 and not selected",
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A networkd `Describe` the way systemd 257 prints it: a loopback, a
    /// DHCP-configured Ethernet port with a default route, and a wireless
    /// interface with a static address and no default route.
    fn describe_fixture() -> Value {
        json!({
            "Interfaces": [
                {
                    "Index": 1, "Name": "lo", "Type": "loopback",
                    "AdministrativeState": "unmanaged", "OperationalState": "carrier",
                    "CarrierState": "carrier", "OnlineState": null,
                    "Addresses": [{"Family": 2, "Address": [127,0,0,1], "PrefixLength": 8, "ScopeString": "host", "ConfigSource": "foreign"}],
                    "Routes": [{"Family": 2, "Destination": [127,0,0,0], "DestinationPrefixLength": 8, "TableString": "local"}]
                },
                {
                    "Index": 2, "Name": "eth0", "Type": "ether", "Driver": "stmmac", "MTU": 1500,
                    "HardwareAddress": [0x02, 0x42, 0xac, 0x11, 0x00, 0x02],
                    "AdministrativeState": "configured", "OperationalState": "routable",
                    "CarrierState": "carrier", "OnlineState": "online", "AddressState": "routable",
                    "Addresses": [
                        {"Family": 2, "Address": [192,0,2,10], "PrefixLength": 24, "ScopeString": "global", "ConfigSource": "DHCPv4", "ConfigProvider": [192,0,2,1]},
                        {"Family": 10, "Address": [0xfe,0x80,0,0,0,0,0,0,0,0x42,0xac,0xff,0xfe,0x11,0,2], "PrefixLength": 64, "ScopeString": "link", "ConfigSource": "foreign"}
                    ],
                    "DNS": [{"Family": 2, "Address": [192,0,2,1], "ConfigSource": "DHCPv4"}],
                    "Routes": [
                        {"Family": 2, "Destination": [0,0,0,0], "DestinationPrefixLength": 0, "Gateway": [192,0,2,1], "Priority": 1024, "ProtocolString": "dhcp", "TableString": "main", "ConfigSource": "DHCPv4"},
                        {"Family": 2, "Destination": [192,0,2,0], "DestinationPrefixLength": 24, "ProtocolString": "kernel", "TableString": "main"}
                    ],
                    "DHCPv4Client": {
                        "State": "bound",
                        "Lease": {"Address": [192,0,2,10], "PrefixLength": 24, "ServerAddress": [192,0,2,1], "Router": [[192,0,2,1]], "LifetimeUSec": 86_400_000_000u64}
                    }
                },
                {
                    "Index": 3, "Name": "wlan0", "Type": "wlan", "Driver": "aic8800",
                    "AdministrativeState": "configured", "OperationalState": "no-carrier",
                    "CarrierState": "no-carrier", "OnlineState": "offline",
                    "Addresses": [{"Family": 2, "Address": [10,0,0,5], "PrefixLength": 24, "ScopeString": "global", "ConfigSource": "static"}],
                    "Routes": [],
                    "UnstableFutureField": "ignored"
                }
            ]
        })
    }

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

    /// The observed surface over the fixture: carrier, formatted addresses,
    /// the lease from the DHCP client object, the one default route, the
    /// per-link DNS, and nothing from the desired settings anywhere.
    #[test]
    fn the_observed_surface_carries_link_address_lease_route_and_dns() {
        let describe = describe_fixture();
        let observed = observed_json(
            Ok(&describe),
            &WifiEvidence::default(),
            None,
            &RadioEvidence::default(),
        );

        assert_eq!(observed["interfaces"]["available"], true);
        assert_eq!(observed["interfaces"]["count"], 3);
        let eth0 = &observed["interfaces"]["entries"][1];
        assert_eq!(eth0["name"], "eth0");
        assert_eq!(eth0["link"]["carrier"], true);
        assert_eq!(eth0["link"]["operationalState"], "routable");
        assert_eq!(eth0["hardwareAddress"], "02:42:ac:11:00:02");
        assert_eq!(eth0["addresses"][0]["address"], "192.0.2.10");
        assert_eq!(eth0["addresses"][0]["family"], "ipv4");
        assert_eq!(eth0["addresses"][0]["prefixLength"], 24);
        assert_eq!(eth0["addresses"][0]["configSource"], "DHCPv4");
        assert_eq!(eth0["addresses"][1]["address"], "fe80::42:acff:fe11:2");
        assert_eq!(eth0["addresses"][1]["family"], "ipv6");
        assert_eq!(eth0["dhcp"]["available"], true);
        assert_eq!(eth0["dhcp"]["inferred"], false);
        assert_eq!(eth0["dhcp"]["state"], "bound");
        assert_eq!(eth0["dhcp"]["lease"]["address"], "192.0.2.10");
        assert_eq!(eth0["dhcp"]["lease"]["server"], "192.0.2.1");
        assert_eq!(eth0["dhcp"]["lease"]["router"], "192.0.2.1");
        assert_eq!(eth0["dhcp"]["lease"]["lifetimeSeconds"], 86_400);
        assert_eq!(eth0["dns"], json!(["192.0.2.1"]));
        assert!(eth0.get("wifi").is_none());

        let wlan0 = &observed["interfaces"]["entries"][2];
        assert_eq!(wlan0["link"]["carrier"], false);
        assert_eq!(wlan0["dhcp"]["available"], false);
        assert!(wlan0["dhcp"]["detail"].is_string());
        assert!(wlan0.get("UnstableFutureField").is_none());

        assert_eq!(observed["defaultRoutes"]["available"], true);
        assert_eq!(observed["defaultRoutes"]["count"], 1);
        let route = &observed["defaultRoutes"]["entries"][0];
        assert_eq!(route["gateway"], "192.0.2.1");
        assert_eq!(route["interface"], "eth0");
        assert_eq!(route["interfaceIndex"], 2);
        assert_eq!(route["metric"], 1024);
        assert_eq!(route["protocol"], "dhcp");
        assert_eq!(route["family"], "ipv4");

        assert_eq!(observed["dns"]["linkServers"], json!(["192.0.2.1"]));
        assert_eq!(observed["dns"]["available"], false);
        assert!(
            observed["dns"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("no DNS probe"))
        );

        // Desired settings are not here under any name: no `configured`
        // member, no interface kinds from the settings map, no static
        // addressing block. (`administrativeState: "configured"` is
        // networkd's word for its own state and is observed, not desired.)
        let text = observed.to_string();
        for desired in ["\"configured\":", "\"kind\":\"wireguard\"", "\"static\":"] {
            assert!(
                !text.contains(desired),
                "{desired} leaked into the observed surface"
            );
        }
    }

    /// networkd absent: the interface and route members say so, the radios
    /// are still reported, and nothing reads as healthy.
    #[test]
    fn an_unanswering_networkd_is_absent_not_empty() {
        let radios = RadioEvidence {
            wifi_interfaces: vec!["wlan0".to_string()],
            ..RadioEvidence::default()
        };
        let observed = observed_json(
            Err("networkd observation timed out"),
            &WifiEvidence::default(),
            None,
            &radios,
        );
        assert_eq!(observed["interfaces"]["available"], false);
        assert!(
            observed["interfaces"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("timed out"))
        );
        assert_eq!(observed["defaultRoutes"]["available"], false);
        assert_eq!(observed["capabilities"]["wifi"]["supported"], true);
        assert_eq!(observed["wifi"]["available"], false);
        assert!(
            observed["wifi"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("control directory"))
        );
    }

    /// A DHCP lease inferred from the address list when networkd reports no
    /// client object, marked as inferred.
    #[test]
    fn a_lease_is_inferred_from_a_dhcp_sourced_address_and_says_so() {
        let mut describe = describe_fixture();
        describe["Interfaces"][1]
            .as_object_mut()
            .unwrap()
            .remove("DHCPv4Client");
        let observed = observed_json(
            Ok(&describe),
            &WifiEvidence::default(),
            None,
            &RadioEvidence::default(),
        );
        let dhcp = &observed["interfaces"]["entries"][1]["dhcp"];
        assert_eq!(dhcp["available"], true);
        assert_eq!(dhcp["inferred"], true);
        assert_eq!(dhcp["lease"]["address"], "192.0.2.10");
        assert_eq!(dhcp["lease"]["server"], "192.0.2.1");
    }

    /// No default route at all is real evidence: count zero, available.
    #[test]
    fn no_default_route_is_reported_as_zero_not_absent() {
        let mut describe = describe_fixture();
        describe["Interfaces"][1]["Routes"] = json!([]);
        let observed = observed_json(
            Ok(&describe),
            &WifiEvidence::default(),
            None,
            &RadioEvidence::default(),
        );
        assert_eq!(observed["defaultRoutes"]["available"], true);
        assert_eq!(observed["defaultRoutes"]["count"], 0);
    }

    #[test]
    fn wpa_status_and_signal_poll_parse_into_an_association() {
        let status = "bssid=aa:bb:cc:dd:ee:ff\nfreq=5180\nssid=Lab\nid=0\nmode=station\n\
            pairwise_cipher=CCMP\ngroup_cipher=CCMP\nkey_mgmt=WPA2-PSK\nwpa_state=COMPLETED\n\
            ip_address=10.0.0.5\naddress=11:22:33:44:55:66\nuuid=abc\n";
        let mut association = parse_wpa_status("wlan0", status);
        assert_eq!(association.state.as_deref(), Some("COMPLETED"));
        assert_eq!(association.ssid.as_deref(), Some("Lab"));
        assert_eq!(association.bssid.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(association.frequency_mhz, Some(5180));
        assert_eq!(association.key_management.as_deref(), Some("WPA2-PSK"));
        assert_eq!(association.detail, None);
        apply_signal_poll(
            &mut association,
            "RSSI=-51\nLINKSPEED=433\nNOISE=9999\nFREQUENCY=5180\n",
        );
        assert_eq!(association.rssi_dbm, Some(-51));
        assert_eq!(association.link_speed_mbps, Some(433));

        let rendered = wifi_json(&association);
        assert_eq!(rendered["associated"], true);
        assert_eq!(rendered["ssid"], "Lab");
        assert_eq!(rendered["rssiDbm"], -51);
        // The interface's own MAC and IP are not association facts.
        assert!(rendered.get("address").is_none());
        assert!(rendered.to_string().contains("11:22:33").not());

        let disconnected = parse_wpa_status(
            "wlan0",
            "wpa_state=DISCONNECTED\naddress=11:22:33:44:55:66\n",
        );
        assert_eq!(wifi_json(&disconnected)["associated"], false);
        assert_eq!(wifi_json(&disconnected)["available"], true);

        let garbage = parse_wpa_status("wlan0", "FAIL\n");
        assert_eq!(wifi_json(&garbage)["available"], false);
    }

    trait Not {
        fn not(self) -> bool;
    }

    impl Not for bool {
        fn not(self) -> bool {
            !self
        }
    }

    #[test]
    fn proc_net_wireless_yields_the_level_per_interface() {
        let text = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n \
            face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\n \
            wlan0: 0000   58.  -52.  -256        0      0      0      0      0        0\n";
        let levels = parse_proc_net_wireless(text);
        assert_eq!(levels.get("wlan0"), Some(&-52));
        assert!(parse_proc_net_wireless("").is_empty());
    }

    #[test]
    fn addresses_format_by_family_or_by_length() {
        assert_eq!(
            format_address(Some(2), &[192, 0, 2, 7]).as_deref(),
            Some("192.0.2.7")
        );
        assert_eq!(
            format_address(None, &[192, 0, 2, 7]).as_deref(),
            Some("192.0.2.7")
        );
        let mut v6 = [0u8; 16];
        v6[15] = 1;
        assert_eq!(format_address(Some(10), &v6).as_deref(), Some("::1"));
        assert_eq!(format_address(Some(2), &[1, 2]), None);
        assert_eq!(format_address(Some(99), &[0; 4]), None);
    }

    /// Radios from a fixture sysfs: a phy80211 interface, a Bluetooth
    /// adapter, a wwan interface typed by uevent — and the cellular answer
    /// is `supported: false` whether or not a modem is plugged in.
    #[test]
    fn radios_are_discovered_and_cellular_is_explicitly_unsupported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("sys/class/net/eth0")).unwrap();
        std::fs::create_dir_all(root.join("sys/class/net/wlan0/phy80211")).unwrap();
        std::fs::create_dir_all(root.join("sys/class/net/wwx0")).unwrap();
        std::fs::write(
            root.join("sys/class/net/wwx0/uevent"),
            "INTERFACE=wwx0\nDEVTYPE=wwan\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("sys/class/bluetooth/hci0")).unwrap();
        let radios = radio_evidence(root);
        assert_eq!(radios.wifi_interfaces, vec!["wlan0"]);
        assert_eq!(radios.bluetooth_adapters, vec!["hci0"]);
        assert_eq!(radios.wwan_interfaces, vec!["wwx0"]);

        let observed = observed_json(Err("x"), &WifiEvidence::default(), None, &radios);
        assert_eq!(observed["capabilities"]["cellular"]["supported"], false);
        assert_eq!(
            observed["capabilities"]["cellular"]["interfaces"],
            json!(["wwx0"])
        );
        assert_eq!(observed["capabilities"]["bluetooth"]["supported"], true);
        assert_eq!(observed["capabilities"]["wifi"]["supported"], true);

        // And the empty board says every radio is unsupported.
        let none = observed_json(
            Err("x"),
            &WifiEvidence::default(),
            None,
            &RadioEvidence::default(),
        );
        assert_eq!(none["capabilities"]["wifi"]["supported"], false);
        assert_eq!(none["capabilities"]["bluetooth"]["supported"], false);
        assert_eq!(none["capabilities"]["cellular"]["supported"], false);
        assert_eq!(none["wifi"]["available"], false);
    }

    /// The DNS member across the three probe outcomes, and the resolver
    /// being unreachable.
    #[test]
    fn the_dns_member_reports_reachability_and_the_probe_outcome() {
        let resolved = DnsEvidence {
            resolver_reachable: true,
            servers: vec!["192.0.2.1".to_string()],
            probe: Some((
                DNS_PROBE_NAME.to_string(),
                DnsOutcome::Resolved { addresses: 4 },
            )),
        };
        let member = dns_json(Some(&resolved), Vec::new());
        assert_eq!(member["available"], true);
        assert_eq!(member["resolverServers"], json!(["192.0.2.1"]));
        assert_eq!(member["probe"]["reachable"], true);
        assert_eq!(member["probe"]["result"], "resolved");
        assert_eq!(member["probe"]["name"], DNS_PROBE_NAME);

        let failed = DnsEvidence {
            probe: Some((
                "n".to_string(),
                DnsOutcome::Failed("org.freedesktop.resolve1.NoNameServers: x".to_string()),
            )),
            ..resolved.clone()
        };
        assert_eq!(
            dns_json(Some(&failed), Vec::new())["probe"]["reachable"],
            false
        );
        assert_eq!(
            dns_json(Some(&failed), Vec::new())["probe"]["result"],
            "failed"
        );

        let timed_out = DnsEvidence {
            probe: Some(("n".to_string(), DnsOutcome::TimedOut)),
            ..resolved
        };
        assert_eq!(
            dns_json(Some(&timed_out), Vec::new())["probe"]["result"],
            "timeout"
        );

        let unreachable = dns_json(Some(&DnsEvidence::default()), vec!["1.1.1.1".to_string()]);
        assert_eq!(unreachable["available"], false);
        assert_eq!(unreachable["linkServers"], json!(["1.1.1.1"]));
        assert_eq!(unreachable["probe"]["available"], false);
    }

    /// The control-socket seam, driven end to end against a fake
    /// wpa_supplicant listening on a datagram socket in a fixture root: the
    /// observer finds the wireless interface in sysfs, asks STATUS and
    /// SIGNAL_POLL, and renders the association.
    #[tokio::test]
    async fn the_control_socket_is_asked_for_each_wireless_interface() {
        use std::os::unix::net::UnixDatagram;
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("sys/class/net/wlan0/phy80211")).unwrap();
        std::fs::create_dir_all(root.join(WPA_CONTROL_DIR)).unwrap();
        let server = UnixDatagram::bind(root.join(WPA_CONTROL_DIR).join("wlan0")).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let fake = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for _ in 0..2 {
                let mut buffer = [0u8; 256];
                let (received, peer) = server.recv_from(&mut buffer).unwrap();
                let command = String::from_utf8_lossy(&buffer[..received]).to_string();
                let reply = match command.as_str() {
                    "STATUS" => {
                        "wpa_state=COMPLETED\nssid=Lab\nbssid=aa:bb:cc:dd:ee:ff\nfreq=2437\nkey_mgmt=SAE\n"
                    }
                    "SIGNAL_POLL" => "RSSI=-60\nLINKSPEED=72\n",
                    _ => "FAIL\n",
                };
                server
                    .send_to(reply.as_bytes(), peer.as_pathname().unwrap())
                    .unwrap();
                asked.push(command);
            }
            asked
        });

        let observer = SystemdNetworkState::at(root);
        let radios = radio_evidence(root);
        let wifi = observer.observe_wifi(&radios.wifi_interfaces).await;
        assert_eq!(fake.join().unwrap(), vec!["STATUS", "SIGNAL_POLL"]);
        assert!(wifi.control_dir_present);
        assert_eq!(wifi.associations.len(), 1);
        let association = &wifi.associations[0];
        assert_eq!(association.ssid.as_deref(), Some("Lab"));
        assert_eq!(association.key_management.as_deref(), Some("SAE"));
        assert_eq!(association.rssi_dbm, Some(-60));
        assert_eq!(association.link_speed_mbps, Some(72));

        let observed = observed_json(Err("no networkd in this test"), &wifi, None, &radios);
        assert_eq!(observed["wifi"]["available"], true);
        assert_eq!(observed["wifi"]["associations"][0]["associated"], true);
        assert_eq!(observed["wifi"]["associations"][0]["frequencyMhz"], 2437);
    }

    /// A wireless interface whose socket nobody serves is reported as not
    /// askable, with the reason, and the observation returns within the
    /// bound.
    #[tokio::test]
    async fn an_unserved_control_socket_is_absent_with_the_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("sys/class/net/wlan0/phy80211")).unwrap();
        std::fs::create_dir_all(root.join(WPA_CONTROL_DIR)).unwrap();
        let observer = SystemdNetworkState::at(root);
        let started = std::time::Instant::now();
        let wifi = observer.observe_wifi(&["wlan0".to_string()]).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(wifi.associations.len(), 1);
        assert!(
            wifi.associations[0]
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("could not be asked"))
        );
        let rendered = wifi_json(&wifi.associations[0]);
        assert_eq!(rendered["available"], false);
    }

    #[tokio::test]
    async fn the_unavailable_observer_is_an_error() {
        assert!(UnavailableNetworkState.observe().await.is_err());
        assert!(UnavailableNetworkState.describe().await.is_err());
    }
}

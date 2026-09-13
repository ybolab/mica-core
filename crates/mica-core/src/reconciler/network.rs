//! Network reconciler: renders systemd-networkd `.network` units and reloads
//! networkd.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use micad_settings::{IfaceKind, IfaceSettings, Settings, WireguardConfig};
use serde_json::json;

use super::Reconciler;
use crate::wgkeys::Keystore;

/// Directory networkd reads runtime unit files from.
const DEFAULT_NETWORK_DIR: &str = "/run/systemd/network";
/// Environment variable overriding the networkd unit directory.
const NETWORK_DIR_ENV: &str = "MICAD_NETWORK_DIR";
/// Environment variable naming the settings file, whose directory is the
/// STATE-backed directory the WireGuard keys live under.
///
/// The same variable the access-point reconciler reads for the same reason: a
/// test that redirects it redirects the secrets with it, and never writes a key
/// into the host's `/var/lib/mica`.
const SETTINGS_PATH_ENV: &str = "MICAD_SETTINGS_PATH";

/// Asks the network stack to pick up freshly rendered unit files.
#[async_trait::async_trait]
pub trait NetworkReload: Send + Sync {
    /// Reload network configuration.
    async fn reload(&self) -> anyhow::Result<()>;
}

/// Production reloader calling `org.freedesktop.network1` `Manager.Reload` on
/// the system bus.
///
/// The bus connection is created lazily inside the call, so constructing this
/// reloader never touches the host.
pub struct Networkd;

#[async_trait::async_trait]
impl NetworkReload for Networkd {
    async fn reload(&self) -> anyhow::Result<()> {
        let connection = zbus::Connection::system().await?;
        connection
            .call_method(
                Some("org.freedesktop.network1"),
                "/org/freedesktop/network1",
                Some("org.freedesktop.network1.Manager"),
                "Reload",
                &(),
            )
            .await?;
        Ok(())
    }
}

/// Deletes a virtual network device from the kernel.
///
/// Removing a `.netdev` file and reloading does not delete the device networkd
/// built from it: networkd creates virtual devices, it does not reap them. So
/// a deleted VLAN would keep passing traffic until the next boot, and a VLAN
/// whose id changed would keep the old id, because netdev properties are
/// applied only when the device is created. Both need the device gone first,
/// which is a thing only an explicit delete does.
#[async_trait::async_trait]
pub trait LinkDelete: Send + Sync {
    /// Delete the kernel device named `iface`.
    async fn delete_link(&self, iface: &str) -> anyhow::Result<()>;
}

/// Production deleter running `networkctl delete <iface>`.
///
/// `networkctl` ships with the systemd the image already runs networkd from,
/// and its `delete` verb is one RTM_DELLINK: the image carries no iproute2,
/// and hand-rolled netlink would put a second engine next to the networkd this
/// reconciler otherwise speaks through.
pub struct NetworkctlDelete;

impl NetworkctlDelete {
    fn command(iface: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("networkctl");
        command.args(["delete", iface]);
        command
    }
}

#[async_trait::async_trait]
impl LinkDelete for NetworkctlDelete {
    async fn delete_link(&self, iface: &str) -> anyhow::Result<()> {
        let output = Self::command(iface).output().await?;
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "networkctl delete {iface} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }
}

/// A deleter that deletes nothing, for tests that exercise this reconciler for
/// its file handling alone.
///
/// `cfg(test)` rather than an allow: it exists only for tests, and an allow
/// would also silence the day it stops being used at all (the argument
/// `Hostnamed::with_path` makes).
#[cfg(test)]
pub struct NoDelete;

#[cfg(test)]
#[async_trait::async_trait]
impl LinkDelete for NoDelete {
    async fn delete_link(&self, _iface: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Reconciler for the `network` settings subtree.
pub struct NetworkReconciler<R: NetworkReload, D: LinkDelete> {
    target_dir: PathBuf,
    reloader: R,
    deleter: D,
    keys: Keystore,
}

impl<R: NetworkReload, D: LinkDelete> NetworkReconciler<R, D> {
    /// Create a network reconciler rendering units into `target_dir`,
    /// reloading through `reloader`, tearing devices down through `deleter`
    /// and taking WireGuard keys from `keys`.
    ///
    /// The key store is a parameter rather than a default for the same reason
    /// the other three are, only more so: a default would be a real path on
    /// the host, and a test that reconciled a tunnel against it would draw a
    /// real private key onto the machine running the tests.
    pub fn new(target_dir: PathBuf, reloader: R, deleter: D, keys: Keystore) -> Self {
        Self {
            target_dir,
            reloader,
            deleter,
            keys,
        }
    }
}

impl NetworkReconciler<Networkd, NetworkctlDelete> {
    /// Production reconciler: target directory from `MICAD_NETWORK_DIR` if
    /// set, else the networkd runtime directory; keys under the directory of
    /// [`SETTINGS_PATH_ENV`].
    pub fn production() -> Self {
        let dir = std::env::var(NETWORK_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_NETWORK_DIR));
        Self::new(dir, Networkd, NetworkctlDelete, production_keystore())
    }
}

/// The production key store: WireGuard keys under the directory holding the
/// settings file, which is the STATE-backed directory on a device.
///
/// Shared by the reconciler and by [`KeyRotation`], so the two cannot end up
/// reading and writing different key files.
fn production_keystore() -> Keystore {
    let state_dir = std::env::var(SETTINGS_PATH_ENV)
        .ok()
        .and_then(|path| {
            Path::new(&path)
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| PathBuf::from(crate::identity::DEFAULT_STATE_DIR));
    Keystore::production(&state_dir)
}

/// Drawing a WireGuard interface a new private key.
///
/// A rotation is not a settings write — there is no key field in the settings
/// tree to write — so it is its own operation rather than a value the tree
/// carries, and this is the port the bus method calls through.
#[async_trait::async_trait]
pub trait WireguardRotate: Send + Sync {
    /// Replace `iface`'s private key and return the new public key.
    async fn rotate_key(&self, iface: &str) -> anyhow::Result<String>;
}

/// The rotation this daemon performs: a new key in the key store, and the
/// device holding the old one deleted.
pub struct KeyRotation<D: LinkDelete> {
    keys: Keystore,
    deleter: D,
}

impl<D: LinkDelete> KeyRotation<D> {
    /// Rotate keys in `keys`, deleting devices through `deleter`.
    pub fn new(keys: Keystore, deleter: D) -> Self {
        Self { keys, deleter }
    }
}

impl KeyRotation<NetworkctlDelete> {
    /// Production rotation, against the same key store the production
    /// reconciler renders from.
    pub fn production() -> Self {
        Self::new(production_keystore(), NetworkctlDelete)
    }
}

#[async_trait::async_trait]
impl<D: LinkDelete> WireguardRotate for KeyRotation<D> {
    /// Write a new private key, then delete the device carrying the old one.
    ///
    /// networkd reads `PrivateKeyFile=` when it creates the device and never
    /// again, so a rotation that only rewrote the file would change what the
    /// public key says without changing what the tunnel uses. The reconcile
    /// the caller runs next re-renders the unit and reloads, and networkd
    /// builds the device back with the key now on disk.
    ///
    /// A failed delete is an error here, unlike in the reconciler's sweep: the
    /// key on disk and the key in the kernel have diverged, and the caller is
    /// about to be handed a public key whose private half the tunnel is not
    /// using yet.
    ///
    /// # Errors
    ///
    /// Returns an error when `iface` is not a name, when the key cannot be
    /// written, or when the device cannot be deleted.
    async fn rotate_key(&self, iface: &str) -> anyhow::Result<String> {
        // The name reaches a file path and a command argument, exactly as it
        // does on the render path.
        validate_iface_name(iface)?;
        let public_key = self.keys.rotate(iface)?;
        self.deleter.delete_link(iface).await?;
        Ok(public_key)
    }
}

/// Linux `IFNAMSIZ` minus the terminator: the longest name an interface can
/// actually have.
const MAX_IFACE_LEN: usize = 15;

/// Refuse an interface name the kernel could not have and the renderer must
/// not see.
///
/// The settings file is editable by anything that can write STATE, so the
/// reconciler is the security boundary (the same argument `sshd.rs` makes for
/// its parser). The name is used twice, and both uses need this: it becomes
/// part of a file name under the networkd directory (a `/` or `..` would
/// escape it), and it is interpolated into `Name=` (an embedded newline would
/// smuggle in arbitrary networkd directives).
///
/// # Errors
///
/// Returns an error when the name is empty, longer than [`MAX_IFACE_LEN`],
/// a directory self-reference, or contains anything but ASCII alphanumerics
/// and `.`, `-`, `_`, `:`.
fn validate_iface_name(iface: &str) -> anyhow::Result<()> {
    if iface.is_empty() {
        return Err(anyhow::anyhow!("network interface name is empty"));
    }
    if iface.len() > MAX_IFACE_LEN {
        return Err(anyhow::anyhow!(
            "network interface {iface:?} is longer than {MAX_IFACE_LEN} characters"
        ));
    }
    if iface == "." || iface == ".." {
        return Err(anyhow::anyhow!("network interface {iface:?} is not a name"));
    }
    if !iface
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
    {
        return Err(anyhow::anyhow!(
            "network interface {iface:?} contains a character an interface name cannot have"
        ));
    }
    Ok(())
}

/// True when `value` parses as an IP address with an optional `/prefix`.
fn is_ip_or_cidr(value: &str) -> bool {
    let (addr, prefix) = match value.split_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix)),
        None => (value, None),
    };
    let Ok(addr) = addr.parse::<std::net::IpAddr>() else {
        return false;
    };
    match prefix {
        None => true,
        Some(prefix) => prefix
            .parse::<u8>()
            .is_ok_and(|p| p <= if addr.is_ipv4() { 32 } else { 128 }),
    }
}

/// Refuse a static configuration whose values could not be addresses.
///
/// `render_unit` interpolates these verbatim onto networkd directive lines,
/// where a newline is a new directive; requiring each value to parse as an
/// address makes injection structurally impossible rather than filtering for
/// it. apid validates the address on its write path, but the settings file is
/// writable without apid, so the boundary must hold here.
fn validate_static(iface: &str, cfg: &micad_settings::StaticConfig) -> anyhow::Result<()> {
    if !is_ip_or_cidr(&cfg.address) {
        return Err(anyhow::anyhow!(
            "network.{iface} static address {:?} is not an IP address or CIDR",
            cfg.address
        ));
    }
    if let Some(gateway) = &cfg.gateway
        && gateway.parse::<std::net::IpAddr>().is_err()
    {
        return Err(anyhow::anyhow!(
            "network.{iface} gateway {gateway:?} is not an IP address"
        ));
    }
    for dns in &cfg.dns {
        if dns.parse::<std::net::IpAddr>().is_err() {
            return Err(anyhow::anyhow!(
                "network.{iface} DNS server {dns:?} is not an IP address"
            ));
        }
    }
    Ok(())
}

/// The spelling a kind has in the settings file, which is also the name of the
/// block that belongs to it.
fn kind_name(kind: IfaceKind) -> &'static str {
    match kind {
        IfaceKind::Physical => "physical",
        IfaceKind::Vlan => "vlan",
        IfaceKind::Bridge => "bridge",
        IfaceKind::Wireguard => "wireguard",
    }
}

/// Refuse an entry whose optional blocks disagree with its `kind`.
///
/// A block belonging to another kind is a statement about the link that the
/// render would silently ignore, and a kind with no block of its own is a link
/// with no parameters; both are refused rather than papered over. The rule for
/// a `wireguard` entry waited for the code that renders the tunnel, and now
/// that the tunnel is rendered it is the same rule as the other two: the block
/// carries the peers, and a tunnel with no peers block is a link that could
/// never carry a packet.
fn validate_kind_blocks(iface: &str, cfg: &IfaceSettings) -> anyhow::Result<()> {
    let kind = kind_name(cfg.kind);
    let own = (!matches!(cfg.kind, IfaceKind::Physical)).then_some(kind);
    for (block, present) in [
        ("vlan", cfg.vlan.is_some()),
        ("bridge", cfg.bridge.is_some()),
        ("wireguard", cfg.wireguard.is_some()),
    ] {
        if present && own != Some(block) {
            return Err(anyhow::anyhow!(
                "network.{iface} is kind {kind} but carries a {block} block"
            ));
        }
    }
    let missing = match cfg.kind {
        IfaceKind::Vlan => cfg.vlan.is_none(),
        IfaceKind::Bridge => cfg.bridge.is_none(),
        IfaceKind::Wireguard => cfg.wireguard.is_none(),
        IfaceKind::Physical => false,
    };
    if missing {
        return Err(anyhow::anyhow!(
            "network.{iface} is kind {kind} but carries no {kind} block"
        ));
    }
    Ok(())
}

/// True when `value` is the `host:port` a peer's `endpoint` has to be.
///
/// Same discipline as [`is_ip_or_cidr`]: the value lands verbatim on an
/// `Endpoint=` line, so it is parsed rather than filtered. A bracketed host is
/// read as the IPv6 literal networkd requires there, and an unbracketed one as
/// a DNS name, whose charset excludes every character that could start a new
/// directive.
fn is_host_port(value: &str) -> bool {
    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    if port.parse::<u16>().is_err() {
        return false;
    }
    if let Some(inner) = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return inner.parse::<std::net::Ipv6Addr>().is_ok();
    }
    !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// Refuse a tunnel whose peers the renderer must not see.
///
/// Every peer value is re-validated by parse-and-re-render, the injection
/// discipline [`validate_static`] already applies to addresses: a public key
/// must decode to exactly 32 bytes, an allowed IP must parse as an address or
/// CIDR, and an endpoint must parse as `host:port`.
///
/// A rejected peer is named by its index and never by its key. A public key is
/// public, but an operator who pasted a private key into the field would find
/// it in a bus error and in the journal, and a peer's position identifies it
/// just as well.
///
/// # Errors
///
/// Returns an error when a peer's public key, allowed IPs or endpoint could not
/// be what they claim to be.
fn validate_wireguard(iface: &str, cfg: &WireguardConfig) -> anyhow::Result<()> {
    for (index, peer) in cfg.peers.iter().enumerate() {
        if !crate::wgkeys::is_key(&peer.public_key) {
            return Err(anyhow::anyhow!(
                "network.{iface} peer {index} has a public key that is not a WireGuard key"
            ));
        }
        for allowed in &peer.allowed_ips {
            if !is_ip_or_cidr(allowed) {
                return Err(anyhow::anyhow!(
                    "network.{iface} peer {index} allowed IP {allowed:?} is not an IP address or CIDR"
                ));
            }
        }
        if let Some(endpoint) = &peer.endpoint
            && !is_host_port(endpoint)
        {
            return Err(anyhow::anyhow!(
                "network.{iface} peer {index} endpoint {endpoint:?} is not host:port"
            ));
        }
    }
    Ok(())
}

/// Refuse a `network` subtree the renderer must not see, before any of it is
/// written.
///
/// Every entry is checked before the first file is created, which is what
/// makes a rejected tree leave the directory as it found it. The relational
/// rules -- a VLAN's parent and a bridge's ports name declared entries -- are
/// fail-closed on purpose: networkd creates a VLAN only when its parent's
/// `.network` names it, and this reconciler renders that line only for an
/// entry it knows about, so an undeclared parent is a VLAN that would never
/// come up. Enforcing it here rather than in apid is the same argument
/// `validate_static` makes: the settings file is writable without apid.
///
/// # Errors
///
/// Returns an error when a name, address, or kind/block pairing is invalid,
/// when a VLAN parent or a bridge port is not itself a declared entry, when a
/// bridge port carries addressing of its own, or when two bridges claim the
/// same port.
fn validate_network(network: &BTreeMap<String, IfaceSettings>) -> anyhow::Result<()> {
    for (iface, cfg) in network {
        validate_iface_name(iface)?;
        if let Some(static_cfg) = &cfg.static_ {
            validate_static(iface, static_cfg)?;
        }
        validate_kind_blocks(iface, cfg)?;
        if let Some(wireguard) = &cfg.wireguard {
            validate_wireguard(iface, wireguard)?;
        }
    }
    // Which bridge claimed each port, so the second claim on one port is an
    // error rather than a race between two `Bridge=` lines for the same file.
    let mut claimed_by: BTreeMap<&str, &str> = BTreeMap::new();
    for (iface, cfg) in network {
        if let Some(vlan) = &cfg.vlan
            && !network.contains_key(&vlan.parent)
        {
            return Err(anyhow::anyhow!(
                "network.{iface} has VLAN parent {:?}, which is not a declared network entry",
                vlan.parent
            ));
        }
        let Some(bridge) = &cfg.bridge else {
            continue;
        };
        for port in &bridge.ports {
            let Some(port_cfg) = network.get(port) else {
                return Err(anyhow::anyhow!(
                    "network.{iface} has bridge port {port:?}, which is not a declared network entry"
                ));
            };
            if port_cfg.dhcp || port_cfg.static_.is_some() {
                return Err(anyhow::anyhow!(
                    "network.{port} is a port of bridge {iface} and must not carry addressing of its own"
                ));
            }
            if let Some(other) = claimed_by.insert(port, iface) {
                return Err(anyhow::anyhow!(
                    "network.{port} is claimed as a port by both bridge {other} and bridge {iface}"
                ));
            }
        }
    }
    Ok(())
}

/// Which bridge each declared port belongs to.
fn bridge_ports(network: &BTreeMap<String, IfaceSettings>) -> BTreeMap<&str, &str> {
    let mut ports = BTreeMap::new();
    for (iface, cfg) in network {
        if let Some(bridge) = &cfg.bridge {
            for port in &bridge.ports {
                ports.insert(port.as_str(), iface.as_str());
            }
        }
    }
    ports
}

/// The declared VLAN children of each parent.
fn vlan_children(network: &BTreeMap<String, IfaceSettings>) -> BTreeMap<&str, Vec<&str>> {
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (iface, cfg) in network {
        if let Some(vlan) = &cfg.vlan {
            children
                .entry(vlan.parent.as_str())
                .or_default()
                .push(iface.as_str());
        }
    }
    children
}

/// Render the `[WireGuard]` and `[WireGuardPeer]` sections of a tunnel's
/// netdev.
///
/// `PrivateKeyFile=` names the key rather than carrying it: the unit file lives
/// in networkd's runtime directory, which is world-readable, and the key file
/// it points at is not.
fn render_wireguard(cfg: &WireguardConfig, key_path: &Path) -> String {
    let mut out = format!("\n[WireGuard]\nPrivateKeyFile={}\n", key_path.display());
    if let Some(port) = cfg.listen_port {
        out.push_str(&format!("ListenPort={port}\n"));
    }
    for peer in &cfg.peers {
        out.push_str(&format!(
            "\n[WireGuardPeer]\nPublicKey={}\n",
            peer.public_key
        ));
        if !peer.allowed_ips.is_empty() {
            out.push_str(&format!("AllowedIPs={}\n", peer.allowed_ips.join(",")));
        }
        if let Some(endpoint) = &peer.endpoint {
            out.push_str(&format!("Endpoint={endpoint}\n"));
        }
        if let Some(keepalive) = peer.persistent_keepalive {
            out.push_str(&format!("PersistentKeepalive={keepalive}\n"));
        }
    }
    out
}

/// Render the `.netdev` unit that creates `iface`, for a kind that needs one.
///
/// `None` for a physical entry, whose device the kernel already has. A
/// wireguard entry's unit names its key file in `keys`, which is a path the
/// store answers for whether or not a key has been drawn yet — the key itself
/// is drawn on the way past in [`NetworkReconciler::apply`].
fn render_netdev(iface: &str, cfg: &IfaceSettings, keys: &Keystore) -> Option<String> {
    match (cfg.kind, &cfg.vlan, &cfg.wireguard) {
        (IfaceKind::Vlan, Some(vlan), _) => Some(format!(
            "[NetDev]\nName={iface}\nKind=vlan\n\n[VLAN]\nId={}\n",
            vlan.id
        )),
        (IfaceKind::Bridge, _, _) => Some(format!("[NetDev]\nName={iface}\nKind=bridge\n")),
        (IfaceKind::Wireguard, _, Some(wireguard)) => Some(format!(
            "[NetDev]\nName={iface}\nKind=wireguard\n{}",
            render_wireguard(wireguard, &keys.key_path(iface))
        )),
        _ => None,
    }
}

/// Render one networkd unit for `iface`.
///
/// `master` is the bridge that claimed this interface as a port, and `vlans`
/// the VLAN children declared on top of it: networkd creates a VLAN only when
/// the parent's `.network` names it, so the child's existence is a fact about
/// the PARENT's unit.
fn render_unit(iface: &str, cfg: &IfaceSettings, master: Option<&str>, vlans: &[&str]) -> String {
    let mut out = format!("[Match]\nName={iface}\n\n[Network]\n");
    if let Some(bridge) = master {
        // A port's addressing is the bridge's; validation has already refused
        // an entry that tried to keep its own.
        out.push_str(&format!("Bridge={bridge}\n"));
    } else if cfg.dhcp {
        out.push_str("DHCP=yes\n");
    } else if let Some(static_cfg) = &cfg.static_ {
        out.push_str(&format!("Address={}\n", static_cfg.address));
        if let Some(gateway) = &static_cfg.gateway {
            out.push_str(&format!("Gateway={gateway}\n"));
        }
        for dns in &static_cfg.dns {
            out.push_str(&format!("DNS={dns}\n"));
        }
    }
    for child in vlans {
        out.push_str(&format!("VLAN={child}\n"));
    }
    out
}

/// Whether `file_name` is one this reconciler wrote: `50-mica-<iface>.network`
/// or, for a virtual link, `50-mica-<iface>.netdev`.
///
/// Anchored to the exact prefix, not `contains("-mica-")`: the wifi reconcilers
/// embed the interface name in their unit names, and an interface like
/// `a-mica-b` (legal — `-` is a valid name character) would otherwise make this
/// sweep delete a sibling reconciler's unit on every pass, in a permanent
/// delete/re-render flap.
fn is_mica_managed(file_name: &str) -> bool {
    file_name.starts_with("50-mica-")
        && (file_name.ends_with(".network") || file_name.ends_with(".netdev"))
}

/// The interface a swept `50-mica-<iface>.netdev` created, and nothing for any
/// other file.
///
/// The swept file names are the exact set of virtual devices this reconciler
/// is giving up, which is what makes them the exact set to delete.
fn mica_netdev_iface(file_name: &str) -> Option<&str> {
    file_name
        .strip_prefix("50-mica-")
        .and_then(|rest| rest.strip_suffix(".netdev"))
}

#[async_trait::async_trait]
impl<R: NetworkReload, D: LinkDelete> Reconciler for NetworkReconciler<R, D> {
    fn name(&self) -> &'static str {
        "network"
    }

    fn subtree(&self) -> &'static str {
        "network"
    }

    async fn apply(&self, settings: &Settings) -> anyhow::Result<serde_json::Value> {
        validate_network(&settings.network)?;
        let masters = bridge_ports(&settings.network);
        let children = vlan_children(&settings.network);
        std::fs::create_dir_all(&self.target_dir)?;
        let mut rendered = BTreeSet::new();
        let mut state = serde_json::Map::new();
        // Devices whose netdev properties changed. They apply at creation
        // only, so the device has to go and be built again.
        let mut recreate = BTreeSet::new();
        for (iface, cfg) in &settings.network {
            let file_name = format!("50-mica-{iface}.network");
            let unit = render_unit(
                iface,
                cfg,
                masters.get(iface.as_str()).copied(),
                children.get(iface.as_str()).map_or(&[][..], Vec::as_slice),
            );
            std::fs::write(self.target_dir.join(&file_name), unit)?;
            let mut entry = json!({
                "file": file_name,
                "dhcp": cfg.dhcp,
                "kind": kind_name(cfg.kind),
            });
            if matches!(cfg.kind, IfaceKind::Wireguard) {
                // Lazily, on the first pass that sees the tunnel: a key file
                // that is already there is kept, so this generates exactly
                // once per interface. Only the public half is published — it
                // is what the far end needs, and it is public by definition.
                entry["publicKey"] = json!(self.keys.ensure(iface)?);
            }
            state.insert(iface.clone(), entry);
            rendered.insert(file_name);
            if let Some(netdev) = render_netdev(iface, cfg, &self.keys) {
                let netdev_name = format!("50-mica-{iface}.netdev");
                let path = self.target_dir.join(&netdev_name);
                if std::fs::read_to_string(&path).is_ok_and(|previous| previous != netdev) {
                    recreate.insert(iface.clone());
                }
                std::fs::write(&path, netdev)?;
                rendered.insert(netdev_name);
            }
        }
        let mut torn_down = BTreeSet::new();
        for entry in std::fs::read_dir(&self.target_dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if is_mica_managed(file_name) && !rendered.contains(file_name) {
                if let Some(iface) = mica_netdev_iface(file_name) {
                    torn_down.insert(iface.to_string());
                }
                std::fs::remove_file(entry.path())?;
            }
        }
        // Before the reload, so networkd builds the recreated devices back on
        // the same pass that deleted them. A failure is logged and not
        // returned: the unit file is already gone, the usual cause is a device
        // that was never created, and failing the apply here would report
        // every interface that did converge as unconverged.
        for iface in torn_down.union(&recreate) {
            if let Err(error) = self.deleter.delete_link(iface).await {
                tracing::warn!(iface, %error, "could not delete network device");
            }
        }
        self.reloader.reload().await?;
        Ok(serde_json::Value::Object(state))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use micad_settings::{BridgeConfig, StaticConfig, VlanConfig, WireguardPeer};

    use super::*;

    #[test]
    fn the_production_delete_is_networkctl_delete() {
        let command = NetworkctlDelete::command("wg0");
        let command = command.as_std();
        assert_eq!(command.get_program(), "networkctl");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["delete", "wg0"]);
    }

    const GOLDEN_DHCP: &str = "[Match]\nName=eth0\n\n[Network]\nDHCP=yes\n";
    const GOLDEN_STATIC: &str = "[Match]\nName=eth1\n\n[Network]\n\
        Address=192.168.1.10/24\nGateway=192.168.1.1\nDNS=1.1.1.1\nDNS=9.9.9.9\n";
    const GOLDEN_EMPTY: &str = "[Match]\nName=eth2\n\n[Network]\n";
    const GOLDEN_VLAN_NETDEV: &str = "[NetDev]\nName=eth0.100\nKind=vlan\n\n[VLAN]\nId=100\n";
    const GOLDEN_VLAN_PARENT: &str = "[Match]\nName=eth0\n\n[Network]\nDHCP=yes\nVLAN=eth0.100\n";
    const GOLDEN_VLAN_CHILD: &str =
        "[Match]\nName=eth0.100\n\n[Network]\nAddress=192.168.100.2/24\n";
    const GOLDEN_BRIDGE_NETDEV: &str = "[NetDev]\nName=br0\nKind=bridge\n";
    const GOLDEN_BRIDGE_PORT: &str = "[Match]\nName=eth1\n\n[Network]\nBridge=br0\n";

    struct MockReload {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl NetworkReload for MockReload {
        async fn reload(&self) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push("reload".to_string());
            Ok(())
        }
    }

    /// Recording [`LinkDelete`], so a test can say which devices the
    /// reconciler asked the kernel to drop, and where those requests sit
    /// against the reload.
    struct MockLink {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl LinkDelete for MockLink {
        async fn delete_link(&self, iface: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("del {iface}"));
            Ok(())
        }
    }

    /// A [`LinkDelete`] that records the request and then fails it, for the
    /// path where a delete cannot be done.
    struct FailingLink {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl LinkDelete for FailingLink {
        async fn delete_link(&self, iface: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("del {iface}"));
            Err(anyhow::anyhow!(
                "networkctl delete {iface} failed: no device"
            ))
        }
    }

    /// The captured output of a tracing subscriber, so a test can assert what
    /// this reconciler did and did not write to the journal.
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl LogCapture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).expect("utf8 log")
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for LogCapture {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    /// A key store under `dir`, which every test in this module is a temporary
    /// directory: a key drawn anywhere else would be a real private key on the
    /// machine running the tests.
    fn keystore_in(dir: &std::path::Path) -> Keystore {
        Keystore::under(dir, None)
    }

    /// A reconciler in `dir` whose reload and device deletions share one call
    /// log, so their order is part of what a test can assert.
    fn reconciler_in(
        dir: &std::path::Path,
    ) -> (
        NetworkReconciler<MockReload, MockLink>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reconciler = NetworkReconciler::new(
            dir.to_path_buf(),
            MockReload {
                calls: Arc::clone(&calls),
            },
            MockLink {
                calls: Arc::clone(&calls),
            },
            keystore_in(dir),
        );
        (reconciler, calls)
    }

    /// A tunnel entry with `peers`, addressed statically the way a WireGuard
    /// client is.
    fn wireguard_iface(listen_port: Option<u16>, peers: Vec<WireguardPeer>) -> IfaceSettings {
        IfaceSettings {
            kind: IfaceKind::Wireguard,
            dhcp: false,
            static_: Some(StaticConfig {
                address: "10.8.0.2/24".to_string(),
                gateway: None,
                dns: Vec::new(),
            }),
            wireguard: Some(WireguardConfig { listen_port, peers }),
            ..IfaceSettings::default()
        }
    }

    /// A peer's public key: 32 bytes of a fixed pattern in base64, so the
    /// golden units are stable and no key is drawn to write a test with.
    fn peer_key(byte: u8) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode([byte; 32])
    }

    fn peer(byte: u8) -> WireguardPeer {
        WireguardPeer {
            public_key: peer_key(byte),
            allowed_ips: vec!["10.8.0.0/24".to_string()],
            endpoint: Some("vpn.example.net:51820".to_string()),
            persistent_keepalive: Some(25),
        }
    }

    fn vlan_iface(parent: &str, id: u16) -> IfaceSettings {
        IfaceSettings {
            kind: IfaceKind::Vlan,
            dhcp: false,
            static_: Some(StaticConfig {
                address: "192.168.100.2/24".to_string(),
                gateway: None,
                dns: Vec::new(),
            }),
            vlan: Some(VlanConfig {
                parent: parent.to_string(),
                id,
            }),
            ..IfaceSettings::default()
        }
    }

    fn bridge_iface(ports: &[&str]) -> IfaceSettings {
        IfaceSettings {
            kind: IfaceKind::Bridge,
            dhcp: true,
            bridge: Some(BridgeConfig {
                ports: ports.iter().map(|port| (*port).to_string()).collect(),
            }),
            ..IfaceSettings::default()
        }
    }

    /// An entry with no addressing of its own, which is what a bridge port has
    /// to be.
    fn port_iface() -> IfaceSettings {
        IfaceSettings {
            dhcp: false,
            ..IfaceSettings::default()
        }
    }

    fn dhcp_iface() -> IfaceSettings {
        IfaceSettings {
            dhcp: true,
            ..IfaceSettings::default()
        }
    }

    fn static_iface() -> IfaceSettings {
        IfaceSettings {
            dhcp: false,
            static_: Some(StaticConfig {
                address: "192.168.1.10/24".to_string(),
                gateway: Some("192.168.1.1".to_string()),
                dns: vec!["1.1.1.1".to_string(), "9.9.9.9".to_string()],
            }),
            ..IfaceSettings::default()
        }
    }

    fn settings_with(network: &[(&str, IfaceSettings)]) -> Settings {
        Settings {
            network: network
                .iter()
                .map(|(iface, cfg)| ((*iface).to_string(), cfg.clone()))
                .collect(),
            ..Settings::default()
        }
    }

    #[tokio::test]
    async fn renders_dhcp_iface() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("eth0", dhcp_iface())]);

        reconciler.apply(&settings).await.unwrap();

        let rendered = std::fs::read_to_string(dir.path().join("50-mica-eth0.network")).unwrap();
        assert_eq!(rendered, GOLDEN_DHCP);
    }

    #[tokio::test]
    async fn renders_static_iface() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("eth1", static_iface())]);

        reconciler.apply(&settings).await.unwrap();

        let rendered = std::fs::read_to_string(dir.path().join("50-mica-eth1.network")).unwrap();
        assert_eq!(rendered, GOLDEN_STATIC);
    }

    #[tokio::test]
    async fn renders_static_less_non_dhcp_iface_with_empty_network_section() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[(
            "eth2",
            IfaceSettings {
                dhcp: false,
                ..IfaceSettings::default()
            },
        )]);

        reconciler.apply(&settings).await.unwrap();

        let rendered = std::fs::read_to_string(dir.path().join("50-mica-eth2.network")).unwrap();
        assert_eq!(rendered, GOLDEN_EMPTY);
    }

    #[tokio::test]
    async fn renders_both_ifaces_and_returns_live_state() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("eth0", dhcp_iface()), ("eth1", static_iface())]);

        let state = reconciler.apply(&settings).await.unwrap();

        let dhcp = std::fs::read_to_string(dir.path().join("50-mica-eth0.network")).unwrap();
        let static_ = std::fs::read_to_string(dir.path().join("50-mica-eth1.network")).unwrap();
        assert_eq!(dhcp, GOLDEN_DHCP);
        assert_eq!(static_, GOLDEN_STATIC);
        assert_eq!(
            state,
            json!({
                "eth0": { "file": "50-mica-eth0.network", "dhcp": true, "kind": "physical" },
                "eth1": { "file": "50-mica-eth1.network", "dhcp": false, "kind": "physical" },
            })
        );
        assert_eq!(*calls.lock().unwrap(), vec!["reload".to_string()]);
        assert_eq!(reconciler.name(), "network");
        assert_eq!(reconciler.subtree(), "network");
    }

    #[tokio::test]
    async fn removes_stale_mica_managed_files_but_keeps_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        std::fs::write(dir.path().join("50-mica-eth9.network"), "stale").unwrap();
        std::fs::write(dir.path().join("80-dhcp.network"), "foreign").unwrap();
        let settings = settings_with(&[("eth0", dhcp_iface())]);

        reconciler.apply(&settings).await.unwrap();

        assert!(!dir.path().join("50-mica-eth9.network").exists());
        assert!(dir.path().join("80-dhcp.network").exists());
        assert!(dir.path().join("50-mica-eth0.network").exists());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_an_iface_name_that_would_escape_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("../evil", dhcp_iface())]);

        let err = reconciler.apply(&settings).await.unwrap_err();

        assert!(err.to_string().contains("interface"), "{err}");
        // Nothing was rendered and networkd was never told to reload: the
        // reconcile aborted before any I/O it would have to undo.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_a_gateway_that_is_not_an_address() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let mut cfg = static_iface();
        // A newline here would land verbatim on the Gateway= line, where it
        // starts a new networkd directive.
        cfg.static_.as_mut().unwrap().gateway = Some("192.168.1.1\nDNS=6.6.6.6".to_string());
        let settings = settings_with(&[("eth1", cfg)]);

        let err = reconciler.apply(&settings).await.unwrap_err();

        assert!(err.to_string().contains("gateway"), "{err}");
        assert!(!dir.path().join("50-mica-eth1.network").exists());
    }

    #[tokio::test]
    async fn sweep_spares_a_wifi_unit_whose_iface_embeds_mica() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        // The wifi reconcilers embed the interface in their unit names, and
        // `a-mica-b` is a legal interface name; the sweep must only ever eat
        // its own `50-mica-*` namespace.
        std::fs::write(dir.path().join("90-wifi-client-a-mica-b.network"), "wifi").unwrap();
        let settings = settings_with(&[("eth0", dhcp_iface())]);

        reconciler.apply(&settings).await.unwrap();

        assert!(dir.path().join("90-wifi-client-a-mica-b.network").exists());
    }

    #[tokio::test]
    async fn renders_a_vlan_netdev_and_names_the_child_in_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        let settings = settings_with(&[
            ("eth0", dhcp_iface()),
            ("eth0.100", vlan_iface("eth0", 100)),
        ]);

        reconciler.apply(&settings).await.unwrap();

        let netdev = std::fs::read_to_string(dir.path().join("50-mica-eth0.100.netdev")).unwrap();
        let parent = std::fs::read_to_string(dir.path().join("50-mica-eth0.network")).unwrap();
        let child = std::fs::read_to_string(dir.path().join("50-mica-eth0.100.network")).unwrap();
        assert_eq!(netdev, GOLDEN_VLAN_NETDEV);
        // networkd creates the VLAN only because the parent's unit names it.
        assert_eq!(parent, GOLDEN_VLAN_PARENT);
        assert_eq!(child, GOLDEN_VLAN_CHILD);
        // A device that did not exist before is not deleted on the way in.
        assert_eq!(*calls.lock().unwrap(), vec!["reload".to_string()]);
    }

    #[tokio::test]
    async fn renders_a_bridge_netdev_and_gives_its_port_only_the_master() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("br0", bridge_iface(&["eth1"])), ("eth1", port_iface())]);

        reconciler.apply(&settings).await.unwrap();

        let netdev = std::fs::read_to_string(dir.path().join("50-mica-br0.netdev")).unwrap();
        let port = std::fs::read_to_string(dir.path().join("50-mica-eth1.network")).unwrap();
        let bridge = std::fs::read_to_string(dir.path().join("50-mica-br0.network")).unwrap();
        assert_eq!(netdev, GOLDEN_BRIDGE_NETDEV);
        assert_eq!(port, GOLDEN_BRIDGE_PORT);
        // The bridge itself carries the addressing its ports gave up.
        assert_eq!(bridge, "[Match]\nName=br0\n\n[Network]\nDHCP=yes\n");
        // A bridge has no netdev-less port file left behind and no VLAN line.
        assert!(!dir.path().join("50-mica-eth1.netdev").exists());
    }

    #[tokio::test]
    async fn deletes_the_kernel_device_of_a_removed_vlan() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        let with_vlan = settings_with(&[
            ("eth0", dhcp_iface()),
            ("eth0.100", vlan_iface("eth0", 100)),
        ]);
        reconciler.apply(&with_vlan).await.unwrap();

        reconciler
            .apply(&settings_with(&[("eth0", dhcp_iface())]))
            .await
            .unwrap();

        assert!(!dir.path().join("50-mica-eth0.100.netdev").exists());
        assert!(!dir.path().join("50-mica-eth0.100.network").exists());
        // The device is deleted BEFORE the reload, and the swept `.network`
        // asks for no deletion of its own: networkd would otherwise leave the
        // VLAN passing traffic until the next boot.
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "reload".to_string(),
                "del eth0.100".to_string(),
                "reload".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn recreates_a_vlan_whose_id_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        reconciler
            .apply(&settings_with(&[
                ("eth0", dhcp_iface()),
                ("eth0.100", vlan_iface("eth0", 100)),
            ]))
            .await
            .unwrap();

        reconciler
            .apply(&settings_with(&[
                ("eth0", dhcp_iface()),
                ("eth0.100", vlan_iface("eth0", 200)),
            ]))
            .await
            .unwrap();

        // Netdev properties are applied when the device is created, so a
        // rewritten file alone would leave the old id in the kernel.
        let netdev = std::fs::read_to_string(dir.path().join("50-mica-eth0.100.netdev")).unwrap();
        assert!(netdev.contains("Id=200"), "{netdev}");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "reload".to_string(),
                "del eth0.100".to_string(),
                "reload".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn reapplying_an_unchanged_vlan_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        let settings = settings_with(&[
            ("eth0", dhcp_iface()),
            ("eth0.100", vlan_iface("eth0", 100)),
        ]);
        reconciler.apply(&settings).await.unwrap();

        reconciler.apply(&settings).await.unwrap();

        // A convergent reconcile that tore its own VLAN down every pass would
        // drop the link on every settings write in the tree.
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["reload".to_string(), "reload".to_string()]
        );
    }

    #[tokio::test]
    async fn sweeps_a_stale_netdev_but_spares_a_foreign_one() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        std::fs::write(dir.path().join("50-mica-br9.netdev"), "stale").unwrap();
        std::fs::write(dir.path().join("70-vpn.netdev"), "foreign").unwrap();

        reconciler
            .apply(&settings_with(&[("eth0", dhcp_iface())]))
            .await
            .unwrap();

        assert!(!dir.path().join("50-mica-br9.netdev").exists());
        assert!(dir.path().join("70-vpn.netdev").exists());
    }

    #[tokio::test]
    async fn renders_a_wireguard_netdev_naming_its_key_file_and_its_peers() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("wg0", wireguard_iface(Some(51820), vec![peer(1)]))]);

        reconciler.apply(&settings).await.unwrap();

        let netdev = std::fs::read_to_string(dir.path().join("50-mica-wg0.netdev")).unwrap();
        let unit = std::fs::read_to_string(dir.path().join("50-mica-wg0.network")).unwrap();
        assert_eq!(
            netdev,
            format!(
                "[NetDev]\nName=wg0\nKind=wireguard\n\n\
                 [WireGuard]\nPrivateKeyFile={}\nListenPort=51820\n\n\
                 [WireGuardPeer]\nPublicKey={}\nAllowedIPs=10.8.0.0/24\n\
                 Endpoint=vpn.example.net:51820\nPersistentKeepalive=25\n",
                dir.path().join("networkd-secrets/wg-wg0.key").display(),
                peer_key(1),
            )
        );
        // The addressing side of a tunnel is the same `.network` every other
        // kind gets.
        assert_eq!(
            unit,
            "[Match]\nName=wg0\n\n[Network]\nAddress=10.8.0.2/24\n"
        );
        // A device that did not exist before is not deleted on the way in.
        assert_eq!(*calls.lock().unwrap(), vec!["reload".to_string()]);
    }

    #[tokio::test]
    async fn renders_a_tunnel_that_only_initiates_without_a_listen_port() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[(
            "wg0",
            wireguard_iface(
                None,
                vec![WireguardPeer {
                    public_key: peer_key(2),
                    allowed_ips: vec!["10.8.0.0/24".to_string(), "fd00::/64".to_string()],
                    endpoint: None,
                    persistent_keepalive: None,
                }],
            ),
        )]);

        reconciler.apply(&settings).await.unwrap();

        let netdev = std::fs::read_to_string(dir.path().join("50-mica-wg0.netdev")).unwrap();
        // Absent optionals render no line at all: an empty `ListenPort=` would
        // be a port, and networkd picking one is what a client wants.
        assert!(!netdev.contains("ListenPort"), "{netdev}");
        assert!(!netdev.contains("Endpoint"), "{netdev}");
        assert!(!netdev.contains("PersistentKeepalive"), "{netdev}");
        // Several allowed IPs are one comma-separated directive.
        assert!(
            netdev.contains("AllowedIPs=10.8.0.0/24,fd00::/64\n"),
            "{netdev}"
        );
    }

    #[tokio::test]
    async fn generates_the_key_once_and_publishes_only_its_public_half() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("wg0", wireguard_iface(Some(51820), vec![peer(1)]))]);

        let first = reconciler.apply(&settings).await.unwrap();
        let second = reconciler.apply(&settings).await.unwrap();

        let key_file = dir.path().join("networkd-secrets/wg-wg0.key");
        let private_key = std::fs::read_to_string(&key_file).unwrap();
        let public_key = first["wg0"]["publicKey"].as_str().unwrap().to_string();
        // Lazy, and exactly once: the second pass finds the key it drew.
        assert_eq!(first, second);
        assert_eq!(std::fs::read_to_string(&key_file).unwrap(), private_key);
        assert_eq!(
            first,
            json!({
                "wg0": {
                    "file": "50-mica-wg0.network",
                    "dhcp": false,
                    "kind": "wireguard",
                    "publicKey": public_key,
                }
            })
        );
        // The leak canary: the private key is in exactly one place, and the
        // tree served over the bus is not it. Nor is any rendered unit -- they
        // name the key file, they do not carry it.
        let state = serde_json::to_string(&first).unwrap();
        assert!(!state.contains(private_key.trim()), "{state}");
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let rendered = std::fs::read_to_string(&path).unwrap();
                assert!(
                    !rendered.contains(private_key.trim()),
                    "{} carries the private key",
                    path.display()
                );
            }
        }
    }

    #[tokio::test]
    async fn live_state_names_the_kind_of_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[
            ("br0", bridge_iface(&["eth1"])),
            ("eth0", dhcp_iface()),
            ("eth0.100", vlan_iface("eth0", 100)),
            ("eth1", port_iface()),
        ]);

        let state = reconciler.apply(&settings).await.unwrap();

        // The reader of this field is the pane, which is a later milestone;
        // the writer is here, beside the public key it sits next to.
        assert_eq!(state["br0"]["kind"], "bridge");
        assert_eq!(state["eth0"]["kind"], "physical");
        assert_eq!(state["eth0.100"]["kind"], "vlan");
        assert_eq!(state["eth1"]["kind"], "physical");
        // A physical entry has no public key to publish.
        assert!(state["eth0"].get("publicKey").is_none());
    }

    #[tokio::test]
    async fn recreates_a_tunnel_whose_peers_changed_and_tears_down_a_removed_one() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        reconciler
            .apply(&settings_with(&[(
                "wg0",
                wireguard_iface(Some(51820), vec![peer(1)]),
            )]))
            .await
            .unwrap();

        reconciler
            .apply(&settings_with(&[(
                "wg0",
                wireguard_iface(Some(51820), vec![peer(1), peer(2)]),
            )]))
            .await
            .unwrap();
        reconciler.apply(&Settings::default()).await.unwrap();

        assert!(!dir.path().join("50-mica-wg0.netdev").exists());
        // A changed netdev is a recreated device, and a swept one is a deleted
        // device: a tunnel joins the same mechanism the VLAN uses.
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "reload".to_string(),
                "del wg0".to_string(),
                "reload".to_string(),
                "del wg0".to_string(),
                "reload".to_string(),
            ]
        );
        // The key outlives the entry: re-declaring wg0 keeps the identity the
        // far end already trusts, and the file is unreachable to everything
        // but networkd meanwhile.
        assert!(dir.path().join("networkd-secrets/wg-wg0.key").exists());
    }

    #[tokio::test]
    async fn rejects_a_wireguard_entry_that_carries_no_wireguard_block() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        let settings = settings_with(&[(
            "wg0",
            IfaceSettings {
                kind: IfaceKind::Wireguard,
                dhcp: false,
                ..IfaceSettings::default()
            },
        )]);

        let err = reconciler.apply(&settings).await.unwrap_err();

        // The rule M4 left to the milestone that renders the tunnel: the block
        // carries the peers, so an entry without one is a link that could
        // never carry a packet.
        assert!(
            err.to_string().contains("carries no wireguard block"),
            "{err}"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_a_peer_whose_public_key_is_not_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let mut broken = peer(1);
        // A newline here would land verbatim on the PublicKey= line, where it
        // starts a new networkd directive.
        broken.public_key = "AAAA\nEndpoint=10.0.0.1:1".to_string();
        let settings = settings_with(&[("wg0", wireguard_iface(None, vec![broken]))]);

        let err = reconciler.apply(&settings).await.unwrap_err();

        assert!(
            err.to_string()
                .contains("peer 0 has a public key that is not a WireGuard key"),
            "{err}"
        );
        // The peer is named by its index, never by the value: a private key
        // pasted into this field must not come back out in an error.
        assert!(!err.to_string().contains("AAAA"), "{err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_a_peer_allowed_ip_and_endpoint_that_are_not_what_they_claim() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let mut bad_ip = peer(1);
        bad_ip.allowed_ips = vec!["10.8.0.0/33".to_string()];
        let mut bad_endpoint = peer(1);
        bad_endpoint.endpoint = Some("vpn.example.net:51820\nPublicKey=x".to_string());

        let ip_err = reconciler
            .apply(&settings_with(&[(
                "wg0",
                wireguard_iface(None, vec![bad_ip]),
            )]))
            .await
            .unwrap_err();
        let endpoint_err = reconciler
            .apply(&settings_with(&[(
                "wg0",
                wireguard_iface(None, vec![bad_endpoint]),
            )]))
            .await
            .unwrap_err();

        assert!(
            ip_err.to_string().contains("is not an IP address or CIDR"),
            "{ip_err}"
        );
        assert!(
            endpoint_err.to_string().contains("is not host:port"),
            "{endpoint_err}"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_endpoint_is_a_host_and_a_port_or_it_is_nothing() {
        assert!(is_host_port("vpn.example.net:51820"));
        assert!(is_host_port("10.0.0.1:51820"));
        assert!(is_host_port("[fd00::1]:51820"));
        assert!(!is_host_port("vpn.example.net"));
        assert!(!is_host_port("vpn.example.net:"));
        assert!(!is_host_port("vpn.example.net:70000"));
        assert!(!is_host_port(":51820"));
        assert!(!is_host_port("fd00::1:51820"));
        assert!(!is_host_port("vpn example.net:51820"));
    }

    #[tokio::test]
    async fn rotation_draws_a_new_key_deletes_the_device_and_never_logs_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("wg0", wireguard_iface(Some(51820), vec![peer(1)]))]);
        let before = reconciler.apply(&settings).await.unwrap();
        let old_private_key =
            std::fs::read_to_string(dir.path().join("networkd-secrets/wg-wg0.key")).unwrap();
        let rotation = KeyRotation::new(
            keystore_in(dir.path()),
            MockLink {
                calls: Arc::clone(&calls),
            },
        );

        let public_key = rotation.rotate_key("wg0").await.unwrap();
        let after = reconciler.apply(&settings).await.unwrap();

        // networkd reads the key when it creates the device, so the device
        // holding the old key is deleted and the reconcile that follows builds
        // it back around the new one.
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "reload".to_string(),
                "del wg0".to_string(),
                "reload".to_string(),
            ]
        );
        assert_ne!(public_key, before["wg0"]["publicKey"].as_str().unwrap());
        assert_eq!(after["wg0"]["publicKey"], public_key);
        // The old private key is gone from disk; there is no history beside it.
        let new_private_key =
            std::fs::read_to_string(dir.path().join("networkd-secrets/wg-wg0.key")).unwrap();
        assert_ne!(new_private_key, old_private_key);
        assert_eq!(
            std::fs::read_dir(dir.path().join("networkd-secrets"))
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_rotation_that_cannot_delete_the_device_is_an_error_that_names_no_key() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let rotation = KeyRotation::new(
            keystore_in(dir.path()),
            FailingLink {
                calls: Arc::clone(&calls),
            },
        );

        let err = rotation.rotate_key("wg0").await.unwrap_err();

        // Unlike the sweep's best-effort delete, this one is fatal: the key on
        // disk and the key in the kernel have diverged, and the caller would
        // otherwise be handed a public key the tunnel is not using.
        assert!(
            err.to_string().contains("networkctl delete wg0 failed"),
            "{err}"
        );
        let private_key =
            std::fs::read_to_string(dir.path().join("networkd-secrets/wg-wg0.key")).unwrap();
        assert!(!format!("{err:#}").contains(private_key.trim()), "{err:#}");
    }

    #[tokio::test]
    async fn a_rotation_refuses_a_name_that_would_escape_the_key_directory() {
        let dir = tempfile::tempdir().unwrap();
        let rotation = KeyRotation::new(keystore_in(dir.path()), NoDelete);

        let err = rotation.rotate_key("../evil").await.unwrap_err();

        // The name reaches a file path and a command argument here exactly as
        // it does on the render path, so it is checked here too.
        assert!(err.to_string().contains("interface"), "{err}");
        assert!(!dir.path().join("secrets").exists());
    }

    #[tokio::test]
    async fn the_only_log_line_a_tunnel_can_produce_carries_no_key() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        // A deleter that fails is the one path in this reconciler that logs at
        // all, and a tunnel being torn down is when it fires.
        let reconciler = NetworkReconciler::new(
            dir.path().to_path_buf(),
            MockReload {
                calls: Arc::clone(&calls),
            },
            FailingLink {
                calls: Arc::clone(&calls),
            },
            keystore_in(dir.path()),
        );
        let settings = settings_with(&[("wg0", wireguard_iface(Some(51820), vec![peer(1)]))]);
        reconciler.apply(&settings).await.unwrap();
        let private_key =
            std::fs::read_to_string(dir.path().join("networkd-secrets/wg-wg0.key")).unwrap();

        let logs = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .without_time()
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        reconciler.apply(&Settings::default()).await.unwrap();
        drop(guard);

        let captured = logs.text();
        assert!(
            captured.contains("could not delete network device"),
            "{captured}"
        );
        assert!(!captured.contains(private_key.trim()), "{captured}");
        // The tear-down is reported and the apply still converges, which is
        // the property that keeps one dead device from failing every other
        // interface.
        assert!(captured.contains("wg0"), "{captured}");
    }

    #[tokio::test]
    async fn rejects_a_kind_whose_own_block_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, calls) = reconciler_in(dir.path());
        let settings = settings_with(&[(
            "eth0.100",
            IfaceSettings {
                kind: IfaceKind::Vlan,
                dhcp: true,
                ..IfaceSettings::default()
            },
        )]);

        let err = reconciler.apply(&settings).await.unwrap_err();

        assert!(err.to_string().contains("carries no vlan block"), "{err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_a_block_that_does_not_match_the_kind() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        // A physical entry with bridge parameters is a statement about the
        // link that the render would silently drop.
        let settings = settings_with(&[
            (
                "eth0",
                IfaceSettings {
                    dhcp: true,
                    bridge: Some(BridgeConfig::default()),
                    ..IfaceSettings::default()
                },
            ),
            ("eth1", port_iface()),
        ]);

        let err = reconciler.apply(&settings).await.unwrap_err();

        assert!(
            err.to_string()
                .contains("is kind physical but carries a bridge block"),
            "{err}"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_a_vlan_parent_that_is_not_declared() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("eth0.100", vlan_iface("eth9", 100))]);

        let err = reconciler.apply(&settings).await.unwrap_err();

        // Fail closed: the VLAN line lives in the parent's unit, so an
        // undeclared parent is a VLAN that would never come up.
        assert!(
            err.to_string()
                .contains("VLAN parent \"eth9\", which is not a declared network entry"),
            "{err}"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_a_bridge_port_that_is_not_declared() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("br0", bridge_iface(&["eth9"]))]);

        let err = reconciler.apply(&settings).await.unwrap_err();

        assert!(
            err.to_string()
                .contains("bridge port \"eth9\", which is not a declared network entry"),
            "{err}"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_a_bridge_port_that_carries_its_own_addressing() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[("br0", bridge_iface(&["eth1"])), ("eth1", static_iface())]);

        let err = reconciler.apply(&settings).await.unwrap_err();

        // The port's `.network` is `Bridge=br0` and nothing else, so an
        // address on it is a value the render has no line for.
        assert!(
            err.to_string()
                .contains("must not carry addressing of its own"),
            "{err}"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rejects_a_port_two_bridges_both_claim() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _calls) = reconciler_in(dir.path());
        let settings = settings_with(&[
            ("br0", bridge_iface(&["eth1"])),
            ("br1", bridge_iface(&["eth1"])),
            ("eth1", port_iface()),
        ]);

        let err = reconciler.apply(&settings).await.unwrap_err();

        // One port, one `Bridge=` line: the second claim has to be an error
        // rather than whichever bridge the map iteration reached last.
        assert!(
            err.to_string()
                .contains("claimed as a port by both bridge br0 and bridge br1"),
            "{err}"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

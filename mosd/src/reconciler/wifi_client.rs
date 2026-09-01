//! WiFi station (uplink) reconciler: renders wpa_supplicant configuration from
//! `wifi.client`, drives `wpa_supplicant@<interface>.service`, and renders the
//! networkd unit that gives the associated link an address. Three system
//! effects, in this order:
//!
//! - `/etc/wpa_supplicant/wpa_supplicant-<interface>.conf` is rendered from
//!   `wifi.client`. That is the path Debian's `wpa_supplicant@.service`
//!   template reads, so the file name is a contract with the unit, not a
//!   preference. It holds PSKs, so it is written at 0600.
//! - a networkd `.network` unit for the interface is rendered, so an associated
//!   link actually gets an address; association without addressing is a link
//!   that looks connected and carries no traffic.
//! - `wpa_supplicant@<interface>.service` is brought to the state `wifi.client`
//!   asks for.
//!
//! Configuration before unit, deliberately: a supplicant started against a
//! stale or absent configuration associates with the wrong network, or with
//! none. Access-point mode is not handled here — this reconciler owns the
//! station role only, and `wifi.ap` belongs to its own reconciler.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use mosd_settings::{Settings, WifiClientSettings, WifiNetwork};
use serde_json::json;

use super::Reconciler;
use super::network::{NetworkReload, Networkd};
use super::systemd::{Systemd, UnitControl, is_active, is_enabled};

/// Directory Debian's `wpa_supplicant@.service` template reads its
/// per-interface configuration from.
const DEFAULT_CONFIG_DIR: &str = "/etc/wpa_supplicant";
/// Environment variable overriding the wpa_supplicant configuration directory.
const CONFIG_DIR_ENV: &str = "MOSD_WPA_SUPPLICANT_DIR";
/// Directory networkd reads runtime unit files from.
///
/// Deliberately the same directory and the same override the network
/// reconciler uses: both render into networkd's runtime drop-in directory, and
/// a test that redirects one must redirect the other with it.
const DEFAULT_NETWORK_DIR: &str = "/run/systemd/network";
/// Environment variable overriding the networkd unit directory.
const NETWORK_DIR_ENV: &str = "MOSD_NETWORK_DIR";
/// Mode of the rendered configuration: owner-only, because it carries PSKs.
const CONFIG_MODE: u32 = 0o600;
/// Prefix of the networkd units this reconciler owns.
///
/// `90-` sorts after the network reconciler's `50-mos-…` and the image's
/// `80-dhcp.network`, so an interface the operator configured explicitly keeps
/// winning: networkd applies the first matching unit in lexical order.
///
/// The prefix deliberately does **not** contain `-mos-`: that is the pattern
/// the network reconciler sweeps for, and a file matching it would be deleted
/// on the network reconciler's next pass.
const NETWORKD_PREFIX: &str = "90-wifi-client-";
/// Longest interface name the kernel accepts (`IFNAMSIZ` minus the terminator).
const MAX_INTERFACE_LEN: usize = 15;
/// Header of the rendered configuration.
const CONFIG_HEADER: &str = "# Managed by mosd from wifi.client. Do not edit.\n";

/// What the reconciler did to the station role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Station {
    /// Something changed: the configuration, the networkd unit, or the unit's
    /// runtime state.
    Applied,
    /// The station was already exactly as configured; nothing was written and
    /// no unit call was made.
    Unchanged,
    /// `wifi.client.enabled` is true but no network is configured, so there is
    /// nothing to associate with and the supplicant is kept down.
    ///
    /// Distinct from [`Self::Disabled`]: the operator asked for the station
    /// role and has simply not finished configuring it, which a UI can produce
    /// in one click. Running a supplicant with an empty network list would
    /// claim the radio without ever associating, and a claimed radio is one the
    /// access-point role cannot use — so "enabled, nothing to join" is kept
    /// down rather than started idle.
    Idle,
    /// `wifi.client.enabled` is false.
    Disabled,
}

impl Station {
    /// Live-state spelling of this outcome.
    fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Unchanged => "unchanged",
            Self::Idle => "idle",
            Self::Disabled => "disabled",
        }
    }
}

/// Reconciler for the `wifi.client` settings subtree.
pub struct WifiClientReconciler<C: UnitControl, R: NetworkReload> {
    config_dir: PathBuf,
    network_dir: PathBuf,
    control: C,
    reloader: R,
}

impl<C: UnitControl, R: NetworkReload> WifiClientReconciler<C, R> {
    /// Create a station reconciler writing wpa_supplicant configuration into
    /// `config_dir` and networkd units into `network_dir`, driving the
    /// supplicant unit through `control` and reloading networkd through
    /// `reloader`.
    ///
    /// Every path is a parameter so tests run entirely inside a temporary
    /// directory and never touch the host's wpa_supplicant.
    pub fn new(config_dir: PathBuf, network_dir: PathBuf, control: C, reloader: R) -> Self {
        Self {
            config_dir,
            network_dir,
            control,
            reloader,
        }
    }
}

impl WifiClientReconciler<Systemd, Networkd> {
    /// Production reconciler: paths from [`CONFIG_DIR_ENV`] and
    /// [`NETWORK_DIR_ENV`] if set, else the system locations.
    pub fn production() -> Self {
        let config_dir = std::env::var(CONFIG_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_DIR));
        let network_dir = std::env::var(NETWORK_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_NETWORK_DIR));
        Self::new(config_dir, network_dir, Systemd::new(), Networkd)
    }
}

/// Name of the configuration file `wpa_supplicant@<interface>.service` reads.
fn config_file_name(interface: &str) -> String {
    format!("wpa_supplicant-{interface}.conf")
}

/// Name of the supplicant unit instance for `interface`.
fn unit_name(interface: &str) -> String {
    format!("wpa_supplicant@{interface}.service")
}

/// Name of the networkd unit this reconciler renders for `interface`.
fn networkd_file_name(interface: &str) -> String {
    format!("{NETWORKD_PREFIX}{interface}.network")
}

/// Whether `file_name` is a networkd unit owned by this reconciler.
fn is_wifi_client_managed(file_name: &str) -> bool {
    file_name.starts_with(NETWORKD_PREFIX) && file_name.ends_with(".network")
}

/// Check that `interface` is a name the kernel could actually carry.
///
/// `wifi.client.interface` is free-form operator input that ends up inside a
/// file name and inside a systemd unit instance name. Validating it here is
/// what keeps `../../etc/passwd` from being a path the reconciler writes to,
/// and keeps a name with a `/` or a space from producing a unit instance that
/// means something other than what it reads like.
///
/// # Errors
///
/// Returns an error when the name is empty, longer than `IFNAMSIZ` allows, one
/// of the directory self-references, or contains anything but ASCII
/// alphanumerics and `.`, `-`, `_`, `:`.
fn validate_interface(interface: &str) -> Result<()> {
    if interface.is_empty() {
        return Err(anyhow!("wifi.client.interface is empty"));
    }
    if interface.len() > MAX_INTERFACE_LEN {
        return Err(anyhow!(
            "wifi.client.interface {interface:?} is longer than {MAX_INTERFACE_LEN} characters"
        ));
    }
    if interface == "." || interface == ".." {
        return Err(anyhow!("wifi.client.interface {interface:?} is not a name"));
    }
    if !interface
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
    {
        return Err(anyhow!(
            "wifi.client.interface {interface:?} contains a character an interface name cannot have"
        ));
    }
    Ok(())
}

/// True when `value` can be carried inside a wpa_supplicant double-quoted
/// string with no way of ending the string early.
///
/// **Lifted into `mosd-settings`**, for the reason the length
/// bound was lifted before it: a write surface has to be able to refuse what
/// this renderer cannot carry, and a second copy of the bytes could disagree
/// with the first. `mosd_settings::is_wpa_quotable` states the predicate --
/// printable ASCII, minus the quote that would close the string and the
/// backslash some wpa_supplicant string forms treat as an escape -- and this
/// is the station renderer's name for it; both encoders below still read it
/// here.
fn is_quotable(value: &str) -> bool {
    mosd_settings::is_wpa_quotable(value)
}

/// Encode `ssid` as a wpa_supplicant `ssid=` value.
///
/// A plain SSID is quoted, which is what an operator reading the file expects.
/// Anything else — a quote, a backslash, a newline, a control character, any
/// non-ASCII byte — is emitted as wpa_supplicant's unquoted hex form, where the
/// alphabet is `0-9a-f` and so injection is not expressible at all.
fn encode_ssid(ssid: &str) -> String {
    if is_quotable(ssid) {
        format!("\"{ssid}\"")
    } else {
        hex::encode(ssid.as_bytes())
    }
}

/// Encode `psk` as a wpa_supplicant `psk=` value.
///
/// A 64-character hex string is a raw 256-bit PMK and is emitted unquoted;
/// quoting it would make wpa_supplicant read it as a 64-character passphrase,
/// which exceeds the 63-character maximum and makes it reject the whole file.
/// Anything else is a passphrase and is quoted.
///
/// # Errors
///
/// Returns an error when the passphrase contains a character that cannot be
/// carried inside a quoted wpa_supplicant string. Unlike an SSID a passphrase
/// has no hex form — bare hex means a raw PMK, not a passphrase — so there is
/// nothing to fall back to. **The error deliberately does not name the value**;
/// see the module's secret-hygiene note.
fn encode_psk(psk: &str) -> Result<String> {
    if psk.len() == mosd_settings::RAW_PMK_LEN && psk.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(psk.to_string());
    }
    // IEEE 802.11i's passphrase bounds, called and no longer restated: they
    // were lifted into `mosd-settings` beside the typed model so
    // that the crate holding `WifiNetwork` states its own field's rule and
    // every write surface can run the same one. The reason they are checked at
    // all is unchanged — wpa_supplicant rejects an out-of-range passphrase by
    // refusing the WHOLE configuration file, which silently takes every other
    // configured network down with it while the reconcile still reports
    // `applied` — and so is the message, which never names the length
    // observed.
    mosd_settings::validate_wifi_psk(psk).map_err(|message| anyhow!(message))?;
    if !is_quotable(psk) {
        return Err(anyhow!(
            "the pre-shared key contains a character wpa_supplicant configuration \
             cannot carry; use printable ASCII without a quote or a backslash"
        ));
    }
    Ok(format!("\"{psk}\""))
}

/// Render the wpa_supplicant configuration for `client`.
///
/// Pure and deterministic: the same settings always produce the same bytes, so
/// a re-render can be compared against what is on disk to decide whether
/// anything changed. Networks are emitted highest `priority` first and each
/// block states its `priority=` explicitly, so wpa_supplicant's selection order
/// is the order the settings asked for rather than the order the blocks happen
/// to appear in; equal priorities keep their settings order, the sort being
/// stable. `update_config=0` is stated rather than left to the default: the
/// file is mosd's render of `wifi.client`, and a `wpa_cli save_config` that
/// rewrote it would be silently reverted on the next reconcile.
///
/// # Errors
///
/// Returns an error when a network's pre-shared key cannot be represented.
fn render_config(client: &WifiClientSettings) -> Result<String> {
    let mut ordered: Vec<&WifiNetwork> = client.networks.iter().collect();
    ordered.sort_by_key(|network| std::cmp::Reverse(network.priority));

    let mut out = String::from(CONFIG_HEADER);
    out.push_str("ctrl_interface=/run/wpa_supplicant\n");
    out.push_str("update_config=0\n");
    for network in ordered {
        out.push_str("\nnetwork={\n");
        out.push_str(&format!("\tssid={}\n", encode_ssid(&network.ssid)));
        if network.hidden {
            out.push_str("\tscan_ssid=1\n");
        }
        out.push_str(&format!("\tpriority={}\n", network.priority));
        match &network.psk {
            Some(psk) => {
                out.push_str("\tkey_mgmt=WPA-PSK\n");
                let encoded = encode_psk(psk)
                    .with_context(|| format!("render the network {:?}", network.ssid.as_str()))?;
                out.push_str(&format!("\tpsk={encoded}\n"));
            }
            None => out.push_str("\tkey_mgmt=NONE\n"),
        }
        out.push_str("}\n");
    }
    Ok(out)
}

/// Render the networkd unit that addresses an associated station link.
fn render_networkd(interface: &str) -> String {
    format!("[Match]\nName={interface}\n\n[Network]\nDHCP=yes\n")
}

/// Write `contents` to `path` atomically: a temporary file in the same
/// directory, flushed, then renamed over the target.
///
/// Same directory because `rename` is only atomic within one filesystem, and
/// the wpa_supplicant configuration directory is a separate mount from `/etc`
/// on the read-only root.
///
/// `mode` is applied to the **temporary** file, before the rename that makes it
/// visible under its real name — a configuration full of PSKs must never exist
/// at the target path with a umask-derived mode, not even for an instant.
fn write_atomically(path: &Path, contents: &str, mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    let temp = directory.join(format!(".{file_name}.mosd-tmp"));

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temp)
        .with_context(|| format!("create {}", temp.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("write {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("flush {}", temp.display()))?;
    drop(file);

    // The mode above only takes effect when the temporary file is created, and
    // is masked by the umask even then; a leftover from an interrupted run
    // would keep its old mode. Both are fixed here, still before the rename.
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("set mode on {}", temp.display()))?;

    std::fs::rename(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    Ok(())
}

impl<C: UnitControl, R: NetworkReload> WifiClientReconciler<C, R> {
    /// Render the wpa_supplicant configuration and report whether its bytes
    /// changed.
    ///
    /// An unchanged render is not rewritten: the configuration lives on STATE,
    /// and a rewrite that changes nothing still costs a flash write on every
    /// reconcile.
    fn apply_config(&self, client: &WifiClientSettings, path: &Path) -> Result<bool> {
        let rendered = render_config(client)?;
        if let Ok(current) = std::fs::read_to_string(path)
            && current == rendered
        {
            return Ok(false);
        }
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }
        write_atomically(path, &rendered, CONFIG_MODE)
            .with_context(|| format!("render {}", path.display()))?;
        Ok(true)
    }

    /// Render the networkd unit for `interface` when the station is meant to be
    /// up, remove every unit of this reconciler's that is not the wanted one,
    /// and report the wanted name plus whether anything changed.
    ///
    /// The sweep is what makes an interface rename converge: the unit for the
    /// old interface would otherwise keep telling networkd to run DHCP on a
    /// link mosd no longer manages.
    fn apply_networkd(&self, interface: &str, up: bool) -> Result<(Option<String>, bool)> {
        std::fs::create_dir_all(&self.network_dir)
            .with_context(|| format!("create {}", self.network_dir.display()))?;

        let wanted = up.then(|| networkd_file_name(interface));
        let mut changed = false;
        if let Some(name) = &wanted {
            let path = self.network_dir.join(name);
            let rendered = render_networkd(interface);
            if std::fs::read_to_string(&path).ok() != Some(rendered.clone()) {
                std::fs::write(&path, &rendered)
                    .with_context(|| format!("render {}", path.display()))?;
                changed = true;
            }
        }
        for entry in std::fs::read_dir(&self.network_dir)
            .with_context(|| format!("read {}", self.network_dir.display()))?
        {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if is_wifi_client_managed(file_name) && Some(file_name) != wanted.as_deref() {
                std::fs::remove_file(entry.path())
                    .with_context(|| format!("remove {}", entry.path().display()))?;
                changed = true;
            }
        }
        Ok((wanted, changed))
    }

    /// Bring the supplicant unit to the state the settings ask for, and report
    /// whether any call was needed.
    ///
    /// Reads before it writes, so a system already in the target state gets no
    /// calls at all. `config_changed` forces a restart of an already-running
    /// supplicant, because wpa_supplicant reads its configuration once at
    /// start: a rewritten file that nothing re-reads leaves the device
    /// associated with the previous network while the settings tree, the live
    /// state and the file on disk all agree it should not be.
    async fn apply_unit(&self, unit: &str, up: bool, config_changed: bool) -> Result<bool> {
        let mut changed = false;
        if up {
            if !is_enabled(&self.control.unit_file_state(unit).await?) {
                self.control.enable(unit).await?;
                changed = true;
            }
            if is_active(&self.control.active_state(unit).await?) {
                if config_changed {
                    self.control.restart(unit).await?;
                    changed = true;
                }
            } else {
                self.control.start(unit).await?;
                changed = true;
            }
        } else {
            if is_active(&self.control.active_state(unit).await?) {
                self.control.stop(unit).await?;
                changed = true;
            }
            if is_enabled(&self.control.unit_file_state(unit).await?) {
                self.control.disable(unit).await?;
                changed = true;
            }
        }
        Ok(changed)
    }
}

#[async_trait::async_trait]
impl<C: UnitControl, R: NetworkReload> Reconciler for WifiClientReconciler<C, R> {
    fn name(&self) -> &'static str {
        "wifiClient"
    }

    fn subtree(&self) -> &'static str {
        "wifi.client"
    }

    async fn apply(&self, settings: &Settings) -> Result<serde_json::Value> {
        let client = &settings.wifi.client;
        validate_interface(&client.interface)?;
        let unit = unit_name(&client.interface);
        let config_path = self.config_dir.join(config_file_name(&client.interface));

        // "Enabled with nothing to join" is not a reason to run a supplicant;
        // see `Station::Idle`.
        let up = client.enabled && !client.networks.is_empty();

        // The configuration is rendered even while the station is kept down, so
        // the file on disk always describes `wifi.client` and adding the first
        // network is an ordinary configuration change rather than a first
        // render.
        let config_changed = self.apply_config(client, &config_path)?;
        let (networkd_unit, networkd_changed) = self.apply_networkd(&client.interface, up)?;
        if networkd_changed {
            self.reloader.reload().await?;
        }
        let unit_changed = self.apply_unit(&unit, up, config_changed).await?;

        let station = if !client.enabled {
            Station::Disabled
        } else if client.networks.is_empty() {
            Station::Idle
        } else if config_changed || networkd_changed || unit_changed {
            Station::Applied
        } else {
            Station::Unchanged
        };

        // Every value below is public: an SSID is broadcast over the air, and
        // `secured` reports only whether a key exists. The key itself never
        // leaves the 0600 configuration file — this tree is served over D-Bus.
        let networks: Vec<serde_json::Value> = client
            .networks
            .iter()
            .map(|network| {
                json!({
                    "ssid": network.ssid,
                    "hidden": network.hidden,
                    "priority": network.priority,
                    "secured": network.psk.is_some(),
                })
            })
            .collect();

        Ok(json!({
            "enabled": client.enabled,
            "interface": client.interface,
            "station": station.as_str(),
            "networks": networks,
            "config": config_path.display().to_string(),
            "networkdUnit": networkd_unit,
            "unit": unit,
            "activeState": self.control.active_state(&unit).await?,
            "unitFileState": self.control.unit_file_state(&unit).await?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use mosd_settings::WifiSettings;

    use super::super::network::{NetworkReconciler, NoDelete};
    use super::super::systemd::mock::MockUnitControl;
    use super::*;
    use crate::wgkeys::Keystore;

    /// Golden render of [`multi_network_client`].
    const GOLDEN_MULTI: &str = "# Managed by mosd from wifi.client. Do not edit.\n\
        ctrl_interface=/run/wpa_supplicant\n\
        update_config=0\n\
        \n\
        network={\n\
        \tssid=\"hidden-lab\"\n\
        \tscan_ssid=1\n\
        \tpriority=20\n\
        \tkey_mgmt=WPA-PSK\n\
        \tpsk=\"labsecret1\"\n\
        }\n\
        \n\
        network={\n\
        \tssid=\"office\"\n\
        \tpriority=10\n\
        \tkey_mgmt=WPA-PSK\n\
        \tpsk=\"officepass\"\n\
        }\n\
        \n\
        network={\n\
        \tssid=\"guest-wifi\"\n\
        \tpriority=5\n\
        \tkey_mgmt=NONE\n\
        }\n";
    /// Golden render of a station with no networks configured.
    const GOLDEN_EMPTY: &str = "# Managed by mosd from wifi.client. Do not edit.\n\
        ctrl_interface=/run/wpa_supplicant\n\
        update_config=0\n";
    /// Golden networkd unit for `wlan0`.
    const GOLDEN_NETWORKD: &str = "[Match]\nName=wlan0\n\n[Network]\nDHCP=yes\n";
    /// Pre-shared key used by the leak test; distinctive enough that a
    /// substring search for it cannot match anything else.
    const SECRET_PSK: &str = "Zq7-LEAKCANARY-4x";

    /// Recording [`NetworkReload`] so a test can tell a needed reload from a
    /// reflexive one.
    struct MockReload {
        calls: Mutex<Vec<String>>,
    }

    impl MockReload {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            match self.calls.lock() {
                Ok(calls) => calls.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    #[async_trait::async_trait]
    impl NetworkReload for MockReload {
        async fn reload(&self) -> Result<()> {
            match self.calls.lock() {
                Ok(mut calls) => calls.push("reload".to_string()),
                Err(poisoned) => poisoned.into_inner().push("reload".to_string()),
            }
            Ok(())
        }
    }

    /// Paths of a fixture, all of them under the tempdir.
    struct Paths {
        config: PathBuf,
        config_dir: PathBuf,
        network_dir: PathBuf,
    }

    impl Paths {
        /// Path of the networkd unit this reconciler renders for `wlan0`.
        fn networkd(&self) -> PathBuf {
            self.network_dir.join("90-wifi-client-wlan0.network")
        }
    }

    /// Fixture rooted entirely inside `dir`. Neither directory exists yet, so
    /// a test also proves the reconciler creates what it needs.
    fn fixture(
        dir: &Path,
        active: &str,
        file_state: &str,
    ) -> (WifiClientReconciler<MockUnitControl, MockReload>, Paths) {
        let paths = Paths {
            config: dir.join("wpa_supplicant").join("wpa_supplicant-wlan0.conf"),
            config_dir: dir.join("wpa_supplicant"),
            network_dir: dir.join("network"),
        };
        let reconciler = WifiClientReconciler::new(
            paths.config_dir.clone(),
            paths.network_dir.clone(),
            MockUnitControl::new(active, file_state),
            MockReload::new(),
        );
        (reconciler, paths)
    }

    fn network(ssid: &str, psk: Option<&str>, hidden: bool, priority: i32) -> WifiNetwork {
        WifiNetwork {
            ssid: ssid.to_string(),
            psk: psk.map(str::to_string),
            hidden,
            priority,
        }
    }

    /// Mixed open and PSK, mixed hidden, differing priorities, deliberately
    /// **not** in priority order so the render has to sort.
    fn multi_network_client() -> WifiClientSettings {
        WifiClientSettings {
            enabled: true,
            interface: "wlan0".to_string(),
            networks: vec![
                network("office", Some("officepass"), false, 10),
                network("guest-wifi", None, false, 5),
                network("hidden-lab", Some("labsecret1"), true, 20),
            ],
        }
    }

    fn client(enabled: bool, networks: Vec<WifiNetwork>) -> WifiClientSettings {
        WifiClientSettings {
            enabled,
            interface: "wlan0".to_string(),
            networks,
        }
    }

    fn settings_with(client: WifiClientSettings) -> Settings {
        Settings {
            wifi: WifiSettings {
                client,
                ..WifiSettings::default()
            },
            ..Settings::default()
        }
    }

    /// The common single-network enabled case.
    fn one_psk_network() -> WifiClientSettings {
        client(true, vec![network("office", Some("officepass"), false, 10)])
    }

    fn mode_of(path: &Path) -> u32 {
        match std::fs::metadata(path) {
            Ok(metadata) => metadata.permissions().mode() & 0o7777,
            Err(err) => panic!("stat {}: {err}", path.display()),
        }
    }

    // ---- rendering --------------------------------------------------------

    #[test]
    fn multi_network_render_matches_the_golden_file() {
        assert_eq!(
            render_config(&multi_network_client()).unwrap(),
            GOLDEN_MULTI
        );
    }

    #[test]
    fn render_is_deterministic() {
        let client = multi_network_client();

        assert_eq!(
            render_config(&client).unwrap(),
            render_config(&client.clone()).unwrap()
        );
    }

    #[test]
    fn networks_are_ordered_by_priority_with_an_explicit_priority_line() {
        let rendered = render_config(&client(
            true,
            vec![
                network("low", None, false, -5),
                network("high", None, false, 100),
                network("mid", None, false, 0),
            ],
        ))
        .unwrap();

        let order: Vec<&str> = rendered
            .lines()
            .filter_map(|line| line.strip_prefix("\tssid="))
            .collect();
        assert_eq!(order, vec!["\"high\"", "\"mid\"", "\"low\""]);
        let priorities: Vec<&str> = rendered
            .lines()
            .filter_map(|line| line.strip_prefix("\tpriority="))
            .collect();
        assert_eq!(
            priorities,
            vec!["100", "0", "-5"],
            "every block must state its own priority so wpa_supplicant's \
             selection matches the settings order"
        );
    }

    #[test]
    fn equal_priorities_keep_their_settings_order() {
        let rendered = render_config(&client(
            true,
            vec![
                network("first", None, false, 7),
                network("second", None, false, 7),
                network("third", None, false, 7),
            ],
        ))
        .unwrap();

        let order: Vec<&str> = rendered
            .lines()
            .filter_map(|line| line.strip_prefix("\tssid="))
            .collect();
        assert_eq!(order, vec!["\"first\"", "\"second\"", "\"third\""]);
    }

    #[test]
    fn a_passphrase_outside_wpa2_bounds_is_rejected() {
        // Seven characters: one short of IEEE 802.11i's minimum. Rendering it
        // would make wpa_supplicant refuse the whole configuration file.
        let err = encode_psk("short07").unwrap_err();
        assert!(err.to_string().contains("8 to 63"), "{err}");
        // Sixty-four non-hex characters: too long for a passphrase and not a
        // PMK either.
        assert!(encode_psk(&"x".repeat(64)).is_err());
        // Sixty-four hex digits ARE a raw PMK; the passphrase bounds must not
        // apply to it.
        assert!(encode_psk(&"a".repeat(64)).is_ok());
        assert!(encode_psk("exactly8").is_ok());
    }

    /// The bound the renderer enforces is the one `mosd-settings` states, and
    /// there is no second copy of it left here (the lift for the
    /// length band, the for the quotable predicate).
    ///
    /// Asserted as an agreement over a table rather than by reading the
    /// constants: what matters is that no input exists for which the renderer
    /// and the lifted rule disagree, which is the property a second copy would
    /// have lost. The golden-file tests beside this one hold the other half —
    /// the bytes the renderer writes for an admissible key are unchanged.
    #[test]
    fn the_lifted_psk_bound_is_the_one_the_renderer_enforces() {
        for psk in [
            "",
            "short07",
            "exactly8",
            &"x".repeat(63),
            &"x".repeat(64),
            &"a".repeat(64),
            &"A".repeat(64),
            &"x".repeat(200),
            // the quotable half of the agreement. Each of these
            // is inside the length band and was accepted by the lifted rule
            // while the renderer refused it -- accepted at a write surface,
            // dead at render time.
            "has\"quote1",
            "has\\backslash",
            "two\nlines1",
            "tab\there1",
            "caf\u{e9}-latte",
        ] {
            assert_eq!(
                mosd_settings::validate_wifi_psk(psk).is_ok(),
                encode_psk(psk).is_ok(),
                "the renderer and the lifted rule disagree about a {}-character key",
                psk.len()
            );
        }
        // And the sentence is the lifted one, verbatim: a caller that reads it
        // from either side reads the same words.
        let lifted = mosd_settings::validate_wifi_psk("short07").unwrap_err();
        assert_eq!(encode_psk("short07").unwrap_err().to_string(), lifted);
        // Which still never names the length observed.
        assert!(!lifted.contains('7'), "{lifted}");
    }

    #[test]
    fn an_open_network_emits_key_mgmt_none_and_no_psk() {
        let rendered = render_config(&client(true, vec![network("cafe", None, false, 0)])).unwrap();

        assert!(
            rendered.contains("\tkey_mgmt=NONE\n"),
            "an open network must say so: {rendered}"
        );
        assert!(
            !rendered.contains("psk="),
            "an open network must not carry a key line: {rendered}"
        );
    }

    #[test]
    fn a_psk_network_emits_a_key_and_never_key_mgmt_none() {
        let rendered = render_config(&client(
            true,
            vec![network("office", Some("s3cretpass"), false, 0)],
        ))
        .unwrap();

        assert!(rendered.contains("\tpsk=\"s3cretpass\"\n"), "{rendered}");
        assert!(rendered.contains("\tkey_mgmt=WPA-PSK\n"), "{rendered}");
        assert!(
            !rendered.contains("key_mgmt=NONE"),
            "a protected network must never be downgraded to open: {rendered}"
        );
    }

    #[test]
    fn hidden_is_the_only_thing_that_emits_scan_ssid() {
        let hidden = render_config(&client(true, vec![network("lab", None, true, 0)])).unwrap();
        let visible = render_config(&client(true, vec![network("lab", None, false, 0)])).unwrap();

        assert!(hidden.contains("\tscan_ssid=1\n"), "{hidden}");
        assert!(!visible.contains("scan_ssid"), "{visible}");
    }

    #[test]
    fn a_hostile_ssid_cannot_inject_configuration() {
        // A quote to close the string, a brace to close the block, and a
        // newline to start a directive of its own — the whole escape.
        let hostile = "evil\"\n}\nnetwork={\n\tssid=\"pwned\"\n\tkey_mgmt=NONE\n}\n#";

        let rendered =
            render_config(&client(true, vec![network(hostile, None, false, 3)])).unwrap();

        assert_eq!(
            rendered.matches("network={").count(),
            1,
            "the hostile SSID opened a second block: {rendered}"
        );
        assert!(
            !rendered.contains("pwned"),
            "hostile content reached the file verbatim: {rendered}"
        );
        assert!(
            rendered.contains(&format!("\tssid={}\n", hex::encode(hostile.as_bytes()))),
            "an SSID that cannot be quoted must be emitted as hex: {rendered}"
        );
        assert_eq!(
            rendered,
            format!(
                "{GOLDEN_EMPTY}\nnetwork={{\n\tssid={}\n\tpriority=3\n\tkey_mgmt=NONE\n}}\n",
                hex::encode(hostile.as_bytes())
            ),
            "the render must be exactly one well-formed block"
        );
    }

    #[test]
    fn every_unquotable_ssid_shape_goes_to_hex_and_plain_ones_stay_quoted() {
        for hostile in [
            "has\"quote",
            "has\\backslash",
            "has\nnewline",
            "has\ttab",
            "has\0nul",
            "caf\u{e9}",
        ] {
            let rendered =
                render_config(&client(true, vec![network(hostile, None, false, 0)])).unwrap();
            assert!(
                rendered.contains(&format!("\tssid={}\n", hex::encode(hostile.as_bytes()))),
                "{hostile:?} must be hex-encoded: {rendered}"
            );
        }
        for plain in [
            "plain",
            "with space",
            "with#hash",
            "with'quote",
            "a-b_c.d:e",
        ] {
            let rendered =
                render_config(&client(true, vec![network(plain, None, false, 0)])).unwrap();
            assert!(
                rendered.contains(&format!("\tssid=\"{plain}\"\n")),
                "{plain:?} must stay quoted and readable: {rendered}"
            );
        }
    }

    #[test]
    fn a_sixty_four_character_hex_key_is_emitted_as_a_raw_pmk() {
        let pmk = "0123456789abcdef".repeat(4);
        assert_eq!(pmk.len(), 64);

        let rendered =
            render_config(&client(true, vec![network("office", Some(&pmk), false, 0)])).unwrap();

        assert!(
            rendered.contains(&format!("\tpsk={pmk}\n")),
            "a raw PMK must not be quoted, or wpa_supplicant reads it as an \
             over-long passphrase and rejects the file: {rendered}"
        );
        assert!(!rendered.contains(&format!("psk=\"{pmk}\"")), "{rendered}");
    }

    #[test]
    fn a_passphrase_that_cannot_be_quoted_is_an_error_that_does_not_name_it() {
        let err = render_config(&client(
            true,
            vec![network("office", Some("bad\"key\nMORE"), false, 0)],
        ))
        .unwrap_err();

        let chain = format!("{err:#}");
        assert!(chain.contains("pre-shared key"), "{chain}");
        assert!(chain.contains("\"office\""), "{chain}");
        assert!(
            !chain.contains("bad") && !chain.contains("MORE"),
            "the key leaked into the error: {chain}"
        );
    }

    // ---- what apply writes ------------------------------------------------

    #[tokio::test]
    async fn apply_writes_the_golden_config_at_0600_creating_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(multi_network_client()))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            GOLDEN_MULTI
        );
        assert_eq!(
            mode_of(&paths.config),
            0o600,
            "the file carries PSKs and must not be readable by anyone else"
        );
        assert_eq!(state["station"], json!("applied"));
        assert_eq!(state["config"], json!(paths.config.display().to_string()));
        assert_eq!(reconciler.name(), "wifiClient");
        assert_eq!(reconciler.subtree(), "wifi.client");
    }

    #[tokio::test]
    async fn a_leftover_temporary_file_does_not_widen_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        // The shape an interrupted run leaves behind: the temporary name
        // already exists, so the mode given at open time is never applied.
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let leftover = paths.config_dir.join(".wpa_supplicant-wlan0.conf.mosd-tmp");
        std::fs::write(&leftover, "stale").unwrap();
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o666)).unwrap();

        reconciler
            .apply(&settings_with(one_psk_network()))
            .await
            .unwrap();

        assert_eq!(mode_of(&paths.config), 0o600);
        assert!(
            !leftover.exists(),
            "the temporary file must be renamed away"
        );
    }

    #[tokio::test]
    async fn apply_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings_with(one_psk_network()))
            .await
            .unwrap();

        for directory in [&paths.config_dir, &paths.network_dir] {
            let leftovers: Vec<_> = std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .filter(|name| name.to_string_lossy().contains("mosd-tmp"))
                .collect();
            assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        }
    }

    #[tokio::test]
    async fn an_enabled_station_gets_a_networkd_unit_that_addresses_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(one_psk_network()))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(paths.networkd()).unwrap(),
            GOLDEN_NETWORKD,
            "association without addressing is a link that looks up and carries nothing"
        );
        assert_eq!(state["networkdUnit"], json!("90-wifi-client-wlan0.network"));
        assert_eq!(reconciler.reloader.calls(), vec!["reload".to_string()]);
    }

    #[tokio::test]
    async fn the_rendered_networkd_unit_survives_the_network_reconcilers_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        reconciler
            .apply(&settings_with(one_psk_network()))
            .await
            .unwrap();
        assert!(paths.networkd().exists());

        // The network reconciler deletes every `*-mos-*.network` it did not
        // itself render. Sharing a directory with it means the station's unit
        // has to be outside that pattern, and this is the check that says so.
        NetworkReconciler::new(
            paths.network_dir.clone(),
            MockReload::new(),
            NoDelete,
            // No tunnel in this test's tree, so no key is ever drawn; the
            // directory is inside the same temporary tree either way.
            Keystore::under(dir.path(), None),
        )
        .apply(&Settings::default())
        .await
        .unwrap();

        assert!(
            paths.networkd().exists(),
            "the network reconciler swept the station's networkd unit away"
        );
        assert_eq!(
            std::fs::read_to_string(paths.networkd()).unwrap(),
            GOLDEN_NETWORKD
        );
    }

    #[tokio::test]
    async fn renaming_the_interface_removes_the_previous_networkd_unit() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        reconciler
            .apply(&settings_with(one_psk_network()))
            .await
            .unwrap();

        let renamed = WifiClientSettings {
            interface: "wlan1".to_string(),
            ..one_psk_network()
        };
        reconciler.apply(&settings_with(renamed)).await.unwrap();

        assert!(
            !paths.networkd().exists(),
            "the old interface's unit would keep running DHCP on a link mosd no longer manages"
        );
        assert!(
            paths
                .network_dir
                .join("90-wifi-client-wlan1.network")
                .exists()
        );
    }

    // ---- unit lifecycle and convergence -----------------------------------

    #[tokio::test]
    async fn disabled_to_enabled_enables_then_starts() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(one_psk_network()))
            .await
            .unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable wpa_supplicant@wlan0.service".to_string(),
                "start wpa_supplicant@wlan0.service".to_string(),
            ]
        );
        assert_eq!(state["unit"], json!("wpa_supplicant@wlan0.service"));
        assert_eq!(state["activeState"], json!("active"));
        assert_eq!(state["unitFileState"], json!("enabled-runtime"));
        assert_eq!(state["enabled"], json!(true));
        assert_eq!(state["interface"], json!("wlan0"));
    }

    #[tokio::test]
    async fn enabled_to_disabled_stops_then_disables_and_drops_the_networkd_unit() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        std::fs::create_dir_all(&paths.network_dir).unwrap();
        std::fs::write(paths.networkd(), GOLDEN_NETWORKD).unwrap();

        let state = reconciler
            .apply(&settings_with(client(
                false,
                vec![network("office", Some("officepass"), false, 10)],
            )))
            .await
            .unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "stop wpa_supplicant@wlan0.service".to_string(),
                "disable wpa_supplicant@wlan0.service".to_string(),
            ]
        );
        assert_eq!(state["station"], json!("disabled"));
        assert_eq!(state["networkdUnit"], json!(null));
        assert!(!paths.networkd().exists());
        assert_eq!(reconciler.reloader.calls(), vec!["reload".to_string()]);
    }

    #[tokio::test]
    async fn reapplying_identical_settings_is_a_no_op_with_zero_bus_calls() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let settings = settings_with(multi_network_client());

        let first = reconciler.apply(&settings).await.unwrap();
        let after_first = reconciler.control.calls();
        // A marker the reconciler would clobber if it rewrote the file: the
        // renderer always produces 0600.
        std::fs::set_permissions(&paths.config, std::fs::Permissions::from_mode(0o644)).unwrap();

        let second = reconciler.apply(&settings).await.unwrap();

        assert_eq!(
            reconciler.control.calls(),
            after_first,
            "a converged system got extra calls"
        );
        assert_eq!(
            reconciler.reloader.calls(),
            vec!["reload".to_string()],
            "networkd was reloaded for nothing"
        );
        assert_eq!(
            mode_of(&paths.config),
            0o644,
            "an unchanged configuration must not be rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            GOLDEN_MULTI
        );
        assert_eq!(first["station"], json!("applied"));
        assert_eq!(second["station"], json!("unchanged"));
    }

    #[tokio::test]
    async fn an_already_converged_enabled_station_needs_no_calls_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.config, GOLDEN_MULTI).unwrap();
        std::fs::create_dir_all(&paths.network_dir).unwrap();
        std::fs::write(paths.networkd(), GOLDEN_NETWORKD).unwrap();

        let state = reconciler
            .apply(&settings_with(multi_network_client()))
            .await
            .unwrap();

        assert!(
            reconciler.control.calls().is_empty(),
            "converged system got calls: {:?}",
            reconciler.control.calls()
        );
        assert!(
            reconciler.reloader.calls().is_empty(),
            "converged system got a reload"
        );
        assert_eq!(state["station"], json!("unchanged"));
    }

    #[tokio::test]
    async fn an_already_stopped_disabled_station_needs_no_calls_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.config, GOLDEN_EMPTY).unwrap();

        reconciler
            .apply(&settings_with(client(false, Vec::new())))
            .await
            .unwrap();

        assert!(
            reconciler.control.calls().is_empty(),
            "converged system got calls: {:?}",
            reconciler.control.calls()
        );
        assert!(reconciler.reloader.calls().is_empty());
    }

    #[tokio::test]
    async fn changing_the_config_of_a_running_supplicant_restarts_it() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.config, GOLDEN_MULTI).unwrap();
        std::fs::create_dir_all(&paths.network_dir).unwrap();
        std::fs::write(paths.networkd(), GOLDEN_NETWORKD).unwrap();

        let mut changed = multi_network_client();
        changed
            .networks
            .push(network("annex", Some("annexpass"), false, 1));
        let state = reconciler.apply(&settings_with(changed)).await.unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec!["restart wpa_supplicant@wlan0.service".to_string()],
            "wpa_supplicant reads its configuration once at start; without a \
             restart the new file never takes effect"
        );
        assert!(
            std::fs::read_to_string(&paths.config)
                .unwrap()
                .contains("\tssid=\"annex\"\n")
        );
        assert_eq!(state["station"], json!("applied"));
    }

    #[tokio::test]
    async fn changing_the_config_of_a_stopped_supplicant_does_not_start_it() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.config, GOLDEN_EMPTY).unwrap();

        reconciler
            .apply(&settings_with(client(
                false,
                vec![network("office", Some("officepass"), false, 10)],
            )))
            .await
            .unwrap();

        assert!(
            reconciler.control.calls().is_empty(),
            "a disabled station must stay down however much its config moved: {:?}",
            reconciler.control.calls()
        );
    }

    // ---- enabled with nothing to join -------------------------------------

    #[tokio::test]
    async fn enabled_with_no_networks_stays_down_and_reports_idle() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        std::fs::create_dir_all(&paths.network_dir).unwrap();
        std::fs::write(paths.networkd(), GOLDEN_NETWORKD).unwrap();

        let state = reconciler
            .apply(&settings_with(client(true, Vec::new())))
            .await
            .unwrap();

        assert_eq!(
            state["station"],
            json!("idle"),
            "enabled-but-unconfigured must not read as disabled"
        );
        assert_eq!(state["enabled"], json!(true));
        assert_eq!(
            reconciler.control.calls(),
            vec![
                "stop wpa_supplicant@wlan0.service".to_string(),
                "disable wpa_supplicant@wlan0.service".to_string(),
            ],
            "a supplicant with no networks would claim the radio without ever associating"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            GOLDEN_EMPTY,
            "the configuration is still rendered, so adding the first network \
             is an ordinary change"
        );
        assert!(!paths.networkd().exists());
    }

    // ---- secret hygiene ---------------------------------------------------

    #[tokio::test]
    async fn the_psk_reaches_the_config_file_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(client(
                true,
                vec![network("office", Some(SECRET_PSK), true, 4)],
            )))
            .await
            .unwrap();

        // Anti-tautology: the key must genuinely have been applied, or every
        // absence below would be vacuous.
        assert!(
            std::fs::read_to_string(&paths.config)
                .unwrap()
                .contains(SECRET_PSK),
            "the key never reached the configuration, so this test proves nothing"
        );
        let rendered_state = serde_json::to_string(&state).unwrap();
        assert!(
            !rendered_state.contains(SECRET_PSK),
            "the key is in the live-state tree, which is served over D-Bus: {rendered_state}"
        );
        assert!(
            !std::fs::read_to_string(paths.networkd())
                .unwrap()
                .contains(SECRET_PSK)
        );
        assert!(
            !reconciler.control.calls().join(" ").contains(SECRET_PSK),
            "the key reached a unit name"
        );
        assert_eq!(
            state["networks"],
            json!([{ "ssid": "office", "hidden": true, "priority": 4, "secured": true }]),
            "the live state reports that a key exists, never what it is"
        );
    }

    #[tokio::test]
    async fn an_open_network_is_reported_unsecured() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(client(
                true,
                vec![network("cafe", None, false, 0)],
            )))
            .await
            .unwrap();

        assert_eq!(state["networks"][0]["secured"], json!(false));
    }

    #[tokio::test]
    async fn an_unrenderable_key_fails_before_anything_is_written_or_started() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let err = reconciler
            .apply(&settings_with(client(
                true,
                vec![network("office", Some("bad\"key\nMORE"), false, 0)],
            )))
            .await
            .unwrap_err();

        let chain = format!("{err:#}");
        assert!(chain.contains("pre-shared key"), "{chain}");
        assert!(!chain.contains("MORE"), "the key leaked: {chain}");
        assert!(!paths.config.exists());
        assert!(reconciler.control.calls().is_empty());
    }

    // ---- interface validation ---------------------------------------------

    #[test]
    fn interface_names_that_are_not_names_are_rejected() {
        for bad in [
            "",
            "..",
            ".",
            "../../etc/passwd",
            "wlan0/../evil",
            "wlan 0",
            "wlan0\n",
            "wlan0;reboot",
            "sixteencharsnam",
            "seventeen_charsx",
        ] {
            if bad == "sixteencharsnam" {
                assert!(validate_interface(bad).is_ok(), "{bad:?} is 15 characters");
                continue;
            }
            assert!(
                validate_interface(bad).is_err(),
                "{bad:?} was accepted as an interface name"
            );
        }
        for good in ["wlan0", "wlp2s0", "wlan0.1", "wl-an_0", "eth0:1"] {
            assert!(validate_interface(good).is_ok(), "{good:?} was rejected");
        }
    }

    #[tokio::test]
    async fn a_traversing_interface_name_writes_nothing_anywhere() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let err = reconciler
            .apply(&settings_with(WifiClientSettings {
                interface: "../../evil".to_string(),
                ..one_psk_network()
            }))
            .await
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("wifi.client.interface"),
            "{err:#}"
        );
        assert!(
            !paths.config_dir.exists() && !paths.network_dir.exists(),
            "a rejected interface name must not create anything"
        );
        assert!(reconciler.control.calls().is_empty());
    }

    // ---- containment ------------------------------------------------------

    #[tokio::test]
    async fn every_path_the_reconciler_writes_stays_inside_the_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(multi_network_client()))
            .await
            .unwrap();

        for path in [&paths.config, &paths.config_dir, &paths.network_dir] {
            assert!(
                path.starts_with(dir.path()),
                "{} escapes the tempdir",
                path.display()
            );
        }
        assert_ne!(
            state["config"],
            json!(format!("{DEFAULT_CONFIG_DIR}/wpa_supplicant-wlan0.conf")),
            "a test must never name the host's wpa_supplicant configuration"
        );
    }
}

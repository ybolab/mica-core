//! WiFi access-point reconciler: renders hostapd configuration from `wifi.ap`,
//! drives `hostapd@<interface>.service`, and renders the networkd unit that
//! gives the access point its address and its DHCP server. Three system
//! effects, in this order:
//!
//! - `/etc/hostapd/<interface>.conf` is rendered from `wifi.ap`. It carries the
//!   WPA2 pre-shared key, so it is written at 0600.
//! - a networkd `.network` unit for the interface is rendered, carrying the
//!   AP-side address and `DHCPServer=yes`. An access point clients can
//!   associate with but which hands out no address is an access point nothing
//!   can reach, the provisioning UI included.
//! - `hostapd@<interface>.service` is brought to the state `wifi.ap.mode` asks
//!   for.
//!
//! Configuration before unit, deliberately: hostapd reads its configuration
//! once at start, so a unit started against a stale file beacons the previous
//! SSID with the previous key. The station role is not handled here —
//! `wifi.client` belongs to [`super::wifi_client`], the two roles cannot share
//! one radio, and this reconciler's answer to that is to report the conflict
//! and touch nothing (see [`AccessPoint::Conflict`]).
//!
//! Secret hygiene: the pre-shared key reaches exactly one place, the 0600
//! configuration file. It is never published in the live-state tree (which is
//! served over D-Bus), never named in an error, and this module contains no
//! logging statement at all.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use micad_settings::{
    ApMode, MAX_PASSPHRASE_LEN, MIN_PASSPHRASE_LEN, RAW_PMK_LEN, Settings, WifiApSettings,
};
use serde_json::json;

use super::Reconciler;
use super::network::{NetworkReload, Networkd};
use super::systemd::{Systemd, UnitControl, is_active, is_enabled};
use crate::identity;

/// Directory the `hostapd@.service` template reads its per-interface
/// configuration from.
const DEFAULT_CONFIG_DIR: &str = "/etc/hostapd";
/// Environment variable overriding the hostapd configuration directory.
const CONFIG_DIR_ENV: &str = "MOSD_HOSTAPD_DIR";
/// Directory networkd reads runtime unit files from.
///
/// Deliberately the same directory and the same override the network and
/// station reconcilers use: all three render into networkd's runtime drop-in
/// directory, and a test that redirects one must redirect the others with it.
const DEFAULT_NETWORK_DIR: &str = "/run/systemd/network";
/// Environment variable overriding the networkd unit directory.
const NETWORK_DIR_ENV: &str = "MOSD_NETWORK_DIR";
/// Environment variable naming the settings file, whose directory is the
/// STATE-backed directory the per-device AP key lives under.
const SETTINGS_PATH_ENV: &str = "MOSD_SETTINGS_PATH";
/// Mode of the rendered configuration: owner-only, because it carries the
/// pre-shared key.
const CONFIG_MODE: u32 = 0o600;
/// Prefix of the networkd units this reconciler owns.
///
/// Two independent properties, both required:
///
/// - it does **not** contain `-mos-`, which is the pattern the network
///   reconciler sweeps for. A unit matching that pattern is deleted on the
///   network reconciler's next pass, leaving the access point without an
///   address and without a DHCP server while every unit test still passes.
/// - `90-` sorts after the network reconciler's `50-mos-…` and after the
///   image's `80-dhcp.network`, and networkd applies the first matching unit
///   in lexical order — so an interface the operator configured explicitly
///   under `network.<iface>` keeps winning.
const NETWORKD_PREFIX: &str = "90-wifi-ap-";
/// Longest interface name the kernel accepts (`IFNAMSIZ` minus the terminator).
const MAX_INTERFACE_LEN: usize = 15;
/// Header of the rendered configuration.
const CONFIG_HEADER: &str = "# Managed by micad from wifi.ap. Do not edit.\n";
/// Longest SSID IEEE 802.11 allows, in bytes.
const MAX_SSID_BYTES: usize = 32;
/// Prefix of a derived SSID, matching the derived hostname's family so a
/// device's access point is recognisably the same device.
const SSID_PREFIX: &str = "mos-";
/// Characters of the device identifier a derived SSID carries; the same count
/// the derived hostname uses.
const SSID_ID_CHARS: usize = 8;
/// Highest 2.4 GHz channel number; `wifi.ap.channel` is documented as 2.4 GHz.
const MAX_CHANNEL: u8 = 14;
/// Shortest prefix length that still leaves a usable subnet.
const MIN_PREFIX_LEN: u32 = 1;
/// Longest prefix length that can still hold a host and a one-address pool.
const MAX_PREFIX_LEN: u32 = 30;

/// What the reconciler did to the access-point role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessPoint {
    /// Something changed: the configuration, the networkd unit, or the unit's
    /// runtime state.
    Applied,
    /// The access point was already exactly as configured; nothing was written
    /// and no unit call was made.
    Unchanged,
    /// `wifi.ap.mode` is `off`, so hostapd is stopped and disabled.
    Stopped,
    /// The access point and the station role were both asked for on the same
    /// interface, which one radio cannot do.
    ///
    /// This is an invalid configuration, not a race to be won. Two reconcilers
    /// each starting and stopping units on one interface would flap forever,
    /// each undoing the other every cycle — so this reconciler reports the
    /// conflict and touches nothing: no unit call, no configuration write, no
    /// networkd unit. In particular it does **not** stop the supplicant: that
    /// unit belongs to the station reconciler, which would start it again on
    /// its next pass.
    Conflict,
}

impl AccessPoint {
    /// Live-state spelling of this outcome.
    fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Unchanged => "unchanged",
            Self::Stopped => "stopped",
            Self::Conflict => "conflict",
        }
    }
}

/// Live-state spelling of an [`ApMode`].
fn mode_str(mode: ApMode) -> &'static str {
    match mode {
        ApMode::Off => "off",
        ApMode::Provisioning => "provisioning",
        ApMode::Always => "always",
    }
}

/// Reconciler for the `wifi.ap` settings subtree.
pub struct WifiApReconciler<C: UnitControl, R: NetworkReload> {
    config_dir: PathBuf,
    network_dir: PathBuf,
    state_dir: PathBuf,
    control: C,
    reloader: R,
}

impl<C: UnitControl, R: NetworkReload> WifiApReconciler<C, R> {
    /// Create an access-point reconciler writing hostapd configuration into
    /// `config_dir`, networkd units into `network_dir`, reading the per-device
    /// AP key from under `state_dir`, driving the hostapd unit through
    /// `control` and reloading networkd through `reloader`.
    ///
    /// Every path is a parameter so tests run entirely inside a temporary
    /// directory and never touch the host's hostapd or the host's secrets.
    pub fn new(
        config_dir: PathBuf,
        network_dir: PathBuf,
        state_dir: PathBuf,
        control: C,
        reloader: R,
    ) -> Self {
        Self {
            config_dir,
            network_dir,
            state_dir,
            control,
            reloader,
        }
    }
}

impl WifiApReconciler<Systemd, Networkd> {
    /// Production reconciler: paths from [`CONFIG_DIR_ENV`],
    /// [`NETWORK_DIR_ENV`] and the directory of [`SETTINGS_PATH_ENV`] if set,
    /// else the system locations.
    ///
    /// The state directory is derived from the settings path rather than taken
    /// as a constructor argument, because `super::all()` takes none and the
    /// daemon's own derivation is exactly this: the secrets live beside the
    /// settings file, so a test that redirects `MOSD_SETTINGS_PATH` into a
    /// temporary directory redirects the secrets with it.
    pub fn production() -> Self {
        let config_dir = std::env::var(CONFIG_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_DIR));
        let network_dir = std::env::var(NETWORK_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_NETWORK_DIR));
        let state_dir = std::env::var(SETTINGS_PATH_ENV)
            .ok()
            .and_then(|path| {
                Path::new(&path)
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(Path::to_path_buf)
            })
            .unwrap_or_else(|| PathBuf::from(identity::DEFAULT_STATE_DIR));
        Self::new(config_dir, network_dir, state_dir, Systemd::new(), Networkd)
    }
}

/// Name of the configuration file `hostapd@<interface>.service` reads.
fn config_file_name(interface: &str) -> String {
    format!("{interface}.conf")
}

/// Name of the hostapd unit instance for `interface`.
fn unit_name(interface: &str) -> String {
    format!("hostapd@{interface}.service")
}

/// Name of the networkd unit this reconciler renders for `interface`.
fn networkd_file_name(interface: &str) -> String {
    format!("{NETWORKD_PREFIX}{interface}.network")
}

/// Whether `file_name` is a networkd unit owned by this reconciler.
fn is_wifi_ap_managed(file_name: &str) -> bool {
    file_name.starts_with(NETWORKD_PREFIX) && file_name.ends_with(".network")
}

/// Check that `interface` is a name the kernel could actually carry.
///
/// `wifi.ap.interface` is free-form operator input that ends up inside a file
/// name and inside a systemd unit instance name. Validating it here is what
/// keeps `../../etc/passwd` from being a path the reconciler writes to, and
/// keeps a name with a `/` or a space from producing a unit instance that
/// means something other than what it reads like.
///
/// # Errors
///
/// Returns an error when the name is empty, longer than `IFNAMSIZ` allows, one
/// of the directory self-references, or contains anything but ASCII
/// alphanumerics and `.`, `-`, `_`, `:`.
fn validate_interface(interface: &str) -> Result<()> {
    if interface.is_empty() {
        return Err(anyhow!("wifi.ap.interface is empty"));
    }
    if interface.len() > MAX_INTERFACE_LEN {
        return Err(anyhow!(
            "wifi.ap.interface {interface:?} is longer than {MAX_INTERFACE_LEN} characters"
        ));
    }
    if interface == "." || interface == ".." {
        return Err(anyhow!("wifi.ap.interface {interface:?} is not a name"));
    }
    if !interface
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
    {
        return Err(anyhow!(
            "wifi.ap.interface {interface:?} contains a character an interface name cannot have"
        ));
    }
    Ok(())
}

/// True when `value` can be carried verbatim on the right-hand side of a
/// hostapd configuration line with no way of meaning anything else.
///
/// hostapd's rules are not wpa_supplicant's: it takes the bytes after the `=`
/// literally to the end of the line, so there is no quoting to escape into, a
/// `"` is an ordinary character and a newline is a new directive. The byte set
/// is [`micad_settings::is_wpa_quotable`] — printable ASCII minus `"` and `\` —
/// imported rather than restated: neither excluded byte is dangerous in a raw
/// hostapd value, but keeping the accepted set identical to the settings
/// validator's and the station reconciler's means one escaping discipline
/// across both files, and a second copy of the bytes could disagree with the
/// first. The cost is that a handful of legal SSIDs take the hex form and stay
/// just as correct.
///
/// The emptiness and leading/trailing-space rules on top are AP-specific and
/// live here: they are about hostapd's line reader, which is not documented to
/// preserve edge spaces — a silently trimmed value is a value the operator did
/// not configure — not about wpa_supplicant quoting.
fn is_plain(value: &str) -> bool {
    !value.is_empty()
        && micad_settings::is_wpa_quotable(value)
        && !value.starts_with(' ')
        && !value.ends_with(' ')
}

/// Encode `ssid` as a hostapd SSID directive, including the key.
///
/// A plain SSID is emitted as `ssid=<value>`, which is what an operator reading
/// the file on device expects. Anything else is emitted as `ssid2=<hex>`:
/// hostapd's `ssid2` accepts a hexdump of the SSID bytes, whose alphabet is
/// `0-9a-f`, so injection is not merely escaped there — it is not expressible.
///
/// The two keys are not interchangeable spellings of one directive: `ssid=`
/// has no hex form and `ssid2=` has no raw form, so the key moves with the
/// encoding.
fn ssid_directive(ssid: &str) -> String {
    if is_plain(ssid) {
        format!("ssid={ssid}")
    } else {
        format!("ssid2={}", hex::encode(ssid.as_bytes()))
    }
}

/// Check that `ssid` is a name IEEE 802.11 can carry.
///
/// # Errors
///
/// Returns an error when the SSID is empty or longer than 32 bytes; hostapd
/// rejects the whole configuration file in either case, so the access point
/// would never come up at all.
fn validate_ssid(ssid: &str) -> Result<()> {
    if ssid.is_empty() {
        return Err(anyhow!("wifi.ap.ssid is empty"));
    }
    if ssid.len() > MAX_SSID_BYTES {
        return Err(anyhow!(
            "wifi.ap.ssid is {} bytes, more than the {MAX_SSID_BYTES} IEEE 802.11 allows",
            ssid.len()
        ));
    }
    Ok(())
}

/// Check that `country_code` is a regulatory domain hostapd can be given.
///
/// The value is rendered raw into the configuration file, so this is an
/// injection guard as much as a validity check: two ASCII letters cannot carry
/// a newline and therefore cannot start a directive of their own.
///
/// # Errors
///
/// Returns an error when the code is not exactly two ASCII letters.
fn validate_country_code(country_code: &str) -> Result<()> {
    if country_code.len() == 2 && country_code.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Ok(());
    }
    Err(anyhow!(
        "wifi.ap.countryCode {country_code:?} is not a two-letter regulatory domain"
    ))
}

/// Encode `psk` as a hostapd key directive, including the key name.
///
/// A 64-character hex string is a raw 256-bit PMK and is emitted as
/// `wpa_psk=`; anything else is a passphrase and is emitted as
/// `wpa_passphrase=`. Using the wrong key name makes hostapd reject the file.
///
/// # Errors
///
/// Returns an error when the passphrase is outside the 8..=63 character range
/// IEEE 802.11i defines, or contains a character a raw hostapd value cannot
/// carry unambiguously. Unlike an SSID a passphrase has no hex form — bare hex
/// means a raw PMK, not a passphrase — so there is nothing to fall back to.
/// **The error deliberately does not name the value, nor any property of it,
/// its length included**: a length is a fact about a secret, and this error
/// reaches an API client through the live-state tree and the apply-task
/// record. See the module's secret-hygiene note.
fn psk_directive(psk: &str) -> Result<String> {
    if psk.len() == RAW_PMK_LEN && psk.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(format!("wpa_psk={psk}"));
    }
    if !(MIN_PASSPHRASE_LEN..=MAX_PASSPHRASE_LEN).contains(&psk.len()) {
        return Err(anyhow!(
            "the pre-shared key must be a WPA2 passphrase of {MIN_PASSPHRASE_LEN} to \
             {MAX_PASSPHRASE_LEN} characters or a raw {RAW_PMK_LEN}-digit hex PMK"
        ));
    }
    if !is_plain(psk) {
        return Err(anyhow!(
            "the pre-shared key contains a character hostapd configuration cannot \
             carry unambiguously; use printable ASCII without a quote, a backslash, \
             or a leading or trailing space"
        ));
    }
    Ok(format!("wpa_passphrase={psk}"))
}

/// Split `address` into its host address and prefix length.
///
/// # Errors
///
/// Returns an error when the value is not `A.B.C.D/prefix`, or when the prefix
/// is outside the range that can hold a host address and a pool.
fn parse_cidr(address: &str) -> Result<(Ipv4Addr, u32)> {
    let (host, prefix) = address
        .split_once('/')
        .ok_or_else(|| anyhow!("wifi.ap.address {address:?} is not in CIDR notation"))?;
    let host: Ipv4Addr = host
        .parse()
        .map_err(|_| anyhow!("wifi.ap.address {address:?} does not start with an IPv4 address"))?;
    let prefix: u32 = prefix
        .parse()
        .map_err(|_| anyhow!("wifi.ap.address {address:?} has no numeric prefix length"))?;
    if !(MIN_PREFIX_LEN..=MAX_PREFIX_LEN).contains(&prefix) {
        return Err(anyhow!(
            "wifi.ap.address {address:?} has a /{prefix} prefix, outside \
             /{MIN_PREFIX_LEN}..=/{MAX_PREFIX_LEN}"
        ));
    }
    Ok((host, prefix))
}

/// The DHCP pool for an access point sitting at `host` in a `/prefix` subnet,
/// as networkd's `PoolOffset=` (from the subnet address) and `PoolSize=`.
///
/// Derived from the address and nothing else, deliberately: the pool is not a
/// second setting an operator has to keep in step, and a pool computed from
/// anything but the address can fall outside the subnet — which networkd
/// accepts and which then hands out addresses no client can use. The pool
/// starts one address above the access point and runs to the last address
/// before the broadcast address.
///
/// # Errors
///
/// Returns an error when `host` is the subnet or broadcast address, or sits so
/// high in the subnet that no address is left to hand out.
fn dhcp_pool(host: Ipv4Addr, prefix: u32) -> Result<(u32, u32)> {
    let host_bits = u32::from(host);
    let mask = u32::MAX << (32 - prefix);
    let index = host_bits - (host_bits & mask);
    let broadcast_index = (1u32 << (32 - prefix)) - 1;
    if index == 0 {
        return Err(anyhow!(
            "wifi.ap.address {host} is the subnet address of its own subnet"
        ));
    }
    if index >= broadcast_index {
        return Err(anyhow!(
            "wifi.ap.address {host} is the broadcast address of its own subnet"
        ));
    }
    let offset = index + 1;
    if offset >= broadcast_index {
        return Err(anyhow!(
            "wifi.ap.address {host}/{prefix} leaves no address for a DHCP pool"
        ));
    }
    Ok((offset, broadcast_index - offset))
}

/// Render the hostapd configuration for `ap` with the resolved `ssid` and
/// `psk`.
///
/// Pure and deterministic: the same inputs always produce the same bytes, so a
/// re-render can be compared against what is on disk to decide whether anything
/// changed. WPA2-PSK only (`wpa=2`, `rsn_pairwise=CCMP`): the settings model
/// has no field to ask for anything else, and an open access point is not
/// something `wifi.ap` can express. `ieee80211d=1` is what makes `country_code`
/// more than a comment — without it hostapd carries the code but does not
/// advertise the regulatory domain.
///
/// # Errors
///
/// Returns an error when the SSID, the country code, the channel or the
/// pre-shared key cannot be represented.
fn render_config(ap: &WifiApSettings, ssid: &str, psk: &str) -> Result<String> {
    validate_ssid(ssid)?;
    validate_country_code(&ap.country_code)?;
    if ap.channel == 0 || ap.channel > MAX_CHANNEL {
        return Err(anyhow!(
            "wifi.ap.channel {} is not a 2.4 GHz channel (1..={MAX_CHANNEL})",
            ap.channel
        ));
    }
    let key = psk_directive(psk)?;

    let mut out = String::from(CONFIG_HEADER);
    out.push_str(&format!("interface={}\n", ap.interface));
    out.push_str("driver=nl80211\n");
    out.push_str(&format!("{}\n", ssid_directive(ssid)));
    out.push_str(&format!("country_code={}\n", ap.country_code));
    out.push_str("ieee80211d=1\n");
    out.push_str("hw_mode=g\n");
    out.push_str(&format!("channel={}\n", ap.channel));
    out.push_str("auth_algs=1\n");
    out.push_str("ignore_broadcast_ssid=0\n");
    out.push_str("wmm_enabled=1\n");
    out.push_str("wpa=2\n");
    out.push_str("wpa_key_mgmt=WPA-PSK\n");
    out.push_str("rsn_pairwise=CCMP\n");
    out.push_str(&format!("{key}\n"));
    Ok(out)
}

/// Render the networkd unit that addresses the access-point link and runs the
/// DHCP server on it.
///
/// The address is re-rendered from the parsed value rather than echoed, so a
/// value that parsed cannot carry anything else into the file.
///
/// # Errors
///
/// Returns an error when `address` is not a CIDR address that leaves room for
/// a pool.
fn render_networkd(interface: &str, address: &str) -> Result<String> {
    let (host, prefix) = parse_cidr(address)?;
    let (offset, size) = dhcp_pool(host, prefix)?;
    Ok(format!(
        "[Match]\nName={interface}\n\n\
         [Network]\nAddress={host}/{prefix}\nDHCPServer=yes\n\n\
         [DHCPServer]\nPoolOffset={offset}\nPoolSize={size}\n"
    ))
}

/// SSID for a device with this identifier.
///
/// Derived from the device identity and nothing else — never from a secret —
/// so the same device always advertises the same name and two devices do not
/// collide. The prefix and the identifier length match the derived hostname's,
/// so an operator seeing `mos-1a2b3c4d` on the air knows which device it is.
///
/// `chars().take(..)` rather than a byte slice, so an identifier shorter than
/// [`SSID_ID_CHARS`] yields a short SSID instead of a panic.
fn derived_ssid(device_id: &str) -> String {
    let suffix: String = device_id.chars().take(SSID_ID_CHARS).collect();
    format!("{SSID_PREFIX}{suffix}")
}

/// Write `contents` to `path` atomically: a temporary file in the same
/// directory, flushed, then renamed over the target.
///
/// Same directory because `rename` is only atomic within one filesystem, and
/// the hostapd configuration directory is a separate mount from `/etc` on the
/// read-only root.
///
/// `mode` is applied to the **temporary** file, before the rename that makes it
/// visible under its real name — a configuration carrying the pre-shared key
/// must never exist at the target path with a umask-derived mode, not even for
/// an instant.
fn write_atomically(path: &Path, contents: &str, mode: u32) -> Result<()> {
    // One implementation, in crate::fswrite. This was a second copy of the
    // temp-and-rename dance; transient.rs had a third. They agreed, which is
    // the reason the duplication survived -- and the hostname reconciler then
    // needed a FOURTH variant, for a path where rename cannot work at all.
    crate::fswrite::write_config(path, contents, mode)
}

impl<C: UnitControl, R: NetworkReload> WifiApReconciler<C, R> {
    /// The SSID this device advertises, if one can be known.
    ///
    /// `wifi.ap.ssid` when the operator set one, otherwise derived from
    /// `provisioning.deviceId`. `None` only on a device that has neither — an
    /// unprovisioned tree — which is a hard error when the access point is
    /// actually meant to run and merely unknown when it is not.
    fn resolve_ssid(&self, settings: &Settings) -> Option<String> {
        settings
            .wifi
            .ap
            .ssid
            .clone()
            .or_else(|| settings.provisioning.device_id.as_deref().map(derived_ssid))
    }

    /// The pre-shared key the access point should use.
    ///
    /// `wifi.ap.psk` when the operator set one, otherwise the per-device key
    /// generated on STATE at first boot. There is deliberately no constant
    /// fallback: the signed rootfs is byte-identical on every device, so a
    /// baked default would be one WiFi key for the entire fleet.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings carry no key and the device has none
    /// on STATE either.
    fn resolve_psk(&self, ap: &WifiApSettings) -> Result<String> {
        if let Some(psk) = &ap.psk {
            return Ok(psk.clone());
        }
        identity::read_ap_psk(&self.state_dir)
            .context("read the per-device access-point key")?
            .ok_or_else(|| {
                anyhow!(
                    "wifi.ap.psk is unset and this device has no access-point key on STATE; \
                     a fleet-wide default is not an option"
                )
            })
    }

    /// Render the hostapd configuration and report whether its bytes changed.
    ///
    /// An unchanged render is not rewritten: the configuration lives on STATE,
    /// and a rewrite that changes nothing still costs a flash write on every
    /// reconcile.
    fn apply_config(&self, rendered: &str, path: &Path) -> Result<bool> {
        if let Ok(current) = std::fs::read_to_string(path)
            && current == rendered
        {
            return Ok(false);
        }
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }
        write_atomically(path, rendered, CONFIG_MODE)
            .with_context(|| format!("render {}", path.display()))?;
        Ok(true)
    }

    /// Render the networkd unit for `interface` when the access point is meant
    /// to be up, remove every unit of this reconciler's that is not the wanted
    /// one, and report the wanted name plus whether anything changed.
    ///
    /// The sweep is what makes an interface rename converge: the unit for the
    /// old interface would otherwise keep an AP address and a DHCP server on a
    /// link micad no longer manages.
    fn apply_networkd(
        &self,
        interface: &str,
        address: &str,
        up: bool,
    ) -> Result<(Option<String>, bool)> {
        std::fs::create_dir_all(&self.network_dir)
            .with_context(|| format!("create {}", self.network_dir.display()))?;

        let wanted = up.then(|| networkd_file_name(interface));
        let mut changed = false;
        if let Some(name) = &wanted {
            let path = self.network_dir.join(name);
            let rendered = render_networkd(interface, address)?;
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
            if is_wifi_ap_managed(file_name) && Some(file_name) != wanted.as_deref() {
                std::fs::remove_file(entry.path())
                    .with_context(|| format!("remove {}", entry.path().display()))?;
                changed = true;
            }
        }
        Ok((wanted, changed))
    }

    /// Bring the hostapd unit to the state the settings ask for, and report
    /// whether any call was needed.
    ///
    /// Reads before it writes, so a system already in the target state gets no
    /// calls at all. `config_changed` forces a restart of a running hostapd,
    /// because hostapd reads its configuration once at start: a rewritten file
    /// that nothing re-reads leaves the radio beaconing the previous SSID with
    /// the previous key while the settings tree, the live state and the file on
    /// disk all agree it should not be.
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
impl<C: UnitControl, R: NetworkReload> Reconciler for WifiApReconciler<C, R> {
    fn name(&self) -> &'static str {
        "wifiAp"
    }

    fn subtree(&self) -> &'static str {
        // The whole `wifi` tree, not just `wifi.ap`: the conflict check below
        // reads `wifi.client`, so a write there must re-run this reconciler
        // too. Declaring only the narrower subtree left the cross-subtree
        // dependency invisible to the bus's overlap test — enabling the
        // station reported no conflict until the next full `apply_all`, and
        // disabling it left an access point parked in `conflict` until
        // something happened to touch `wifi.ap`. The station reconciler keeps
        // its narrow subtree: it reads nothing outside `wifi.client`.
        "wifi"
    }

    async fn apply(&self, settings: &Settings) -> Result<serde_json::Value> {
        let ap = &settings.wifi.ap;
        validate_interface(&ap.interface)?;
        let unit = unit_name(&ap.interface);
        let config_path = self.config_dir.join(config_file_name(&ap.interface));
        let ssid = self.resolve_ssid(settings);
        let up = ap.mode != ApMode::Off;

        // One radio cannot be a station and an access point at once. Reported,
        // never fought over: see `AccessPoint::Conflict`. This returns before
        // any I/O and before any bus call, so a conflicting tree leaves the
        // radio exactly as the station reconciler set it.
        if up && settings.wifi.client.enabled && settings.wifi.client.interface == ap.interface {
            return Ok(json!({
                "mode": mode_str(ap.mode),
                "interface": ap.interface,
                "accessPoint": AccessPoint::Conflict.as_str(),
                "ssid": ssid,
                "unit": unit,
                "conflict": format!(
                    "wifi.client is enabled on {}, so the access point cannot use it: \
                     one radio cannot be a station and an access point at once",
                    ap.interface
                ),
            }));
        }

        // The configuration is rendered only while the access point is meant to
        // run. Unlike the station's, it cannot be rendered "for later": its key
        // comes from STATE, and a device that has not been provisioned yet has
        // none — rendering unconditionally would make the default tree
        // (`mode: off`) fail on every reconcile.
        let mut config_changed = false;
        if up {
            let ssid = ssid.as_deref().ok_or_else(|| {
                anyhow!("wifi.ap.ssid is unset and this device has no identity to derive one from")
            })?;
            let psk = self.resolve_psk(ap)?;
            let rendered = render_config(ap, ssid, &psk)?;
            config_changed = self.apply_config(&rendered, &config_path)?;
        }

        let (networkd_unit, networkd_changed) =
            self.apply_networkd(&ap.interface, &ap.address, up)?;
        if networkd_changed {
            self.reloader.reload().await?;
        }
        let unit_changed = self.apply_unit(&unit, up, config_changed).await?;

        let access_point = if !up {
            AccessPoint::Stopped
        } else if config_changed || networkd_changed || unit_changed {
            AccessPoint::Applied
        } else {
            AccessPoint::Unchanged
        };

        let ssid_source = if ap.ssid.is_some() {
            "settings"
        } else {
            "derived"
        };

        // Every value below is public: an SSID is broadcast over the air, and
        // `secured` reports only whether a key exists. The key itself never
        // leaves the 0600 configuration file — this tree is served over D-Bus.
        Ok(json!({
            "mode": mode_str(ap.mode),
            "interface": ap.interface,
            "accessPoint": access_point.as_str(),
            "ssid": ssid,
            "ssidSource": ssid_source,
            "channel": ap.channel,
            "countryCode": ap.country_code,
            "address": ap.address,
            "secured": true,
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

    use micad_settings::{ProvisioningSettings, WifiClientSettings, WifiSettings};

    use super::super::network::{NetworkReconciler, NoDelete};
    use super::super::systemd::mock::MockUnitControl;
    use super::*;
    use crate::wgkeys::Keystore;

    /// Golden render of [`lab_ap`] with an explicit SSID and key.
    const GOLDEN_CONFIG: &str = "# Managed by micad from wifi.ap. Do not edit.\n\
        interface=wlan0\n\
        driver=nl80211\n\
        ssid=mos-lab\n\
        country_code=DE\n\
        ieee80211d=1\n\
        hw_mode=g\n\
        channel=11\n\
        auth_algs=1\n\
        ignore_broadcast_ssid=0\n\
        wmm_enabled=1\n\
        wpa=2\n\
        wpa_key_mgmt=WPA-PSK\n\
        rsn_pairwise=CCMP\n\
        wpa_passphrase=labsecret1\n";
    /// Golden networkd unit for `wlan0` at the default AP address.
    const GOLDEN_NETWORKD: &str = "[Match]\nName=wlan0\n\n\
        [Network]\nAddress=192.168.4.1/24\nDHCPServer=yes\n\n\
        [DHCPServer]\nPoolOffset=2\nPoolSize=253\n";
    /// Device identifier used by the fixtures; 32 hex characters, as
    /// `identity` generates.
    const DEVICE_ID: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f809";
    /// Pre-shared key used by the leak test; distinctive enough that a
    /// substring search for it cannot match anything else.
    const SECRET_PSK: &str = "Zq7LEAKCANARY4x";

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
        state_dir: PathBuf,
    }

    impl Paths {
        /// Path of the networkd unit this reconciler renders for `wlan0`.
        fn networkd(&self) -> PathBuf {
            self.network_dir.join("90-wifi-ap-wlan0.network")
        }
    }

    /// Fixture rooted entirely inside `dir`. No directory exists yet, so a
    /// test also proves the reconciler creates what it needs.
    fn fixture(
        dir: &Path,
        active: &str,
        file_state: &str,
    ) -> (WifiApReconciler<MockUnitControl, MockReload>, Paths) {
        let paths = Paths {
            config: dir.join("hostapd").join("wlan0.conf"),
            config_dir: dir.join("hostapd"),
            network_dir: dir.join("network"),
            state_dir: dir.join("state"),
        };
        let reconciler = WifiApReconciler::new(
            paths.config_dir.clone(),
            paths.network_dir.clone(),
            paths.state_dir.clone(),
            MockUnitControl::new(active, file_state),
            MockReload::new(),
        );
        (reconciler, paths)
    }

    /// Put `psk` where [`identity::read_ap_psk`] looks for it.
    fn seed_ap_psk(state_dir: &Path, psk: &str) {
        let secrets = state_dir.join("secrets");
        std::fs::create_dir_all(&secrets).expect("create secrets dir");
        std::fs::write(secrets.join("ap-psk"), format!("{psk}\n")).expect("write ap-psk");
    }

    /// The access point the golden render describes: explicit SSID, explicit
    /// key, a non-default channel and a non-default regulatory domain so the
    /// golden file cannot pass by matching the struct defaults.
    fn lab_ap() -> WifiApSettings {
        WifiApSettings {
            mode: ApMode::Always,
            ssid: Some("mos-lab".to_string()),
            psk: Some("labsecret1".to_string()),
            channel: 11,
            country_code: "DE".to_string(),
            ..WifiApSettings::default()
        }
    }

    /// Settings carrying `ap`, a provisioned device identity, and a station
    /// role that is switched off.
    fn settings_with(ap: WifiApSettings) -> Settings {
        Settings {
            provisioning: ProvisioningSettings {
                device_id: Some(DEVICE_ID.to_string()),
                ..ProvisioningSettings::default()
            },
            wifi: WifiSettings {
                ap,
                ..WifiSettings::default()
            },
            ..Settings::default()
        }
    }

    /// `settings_with`, plus a station role on `interface`.
    fn settings_with_station(ap: WifiApSettings, interface: &str) -> Settings {
        let mut settings = settings_with(ap);
        settings.wifi.client = WifiClientSettings {
            enabled: true,
            interface: interface.to_string(),
            networks: Vec::new(),
        };
        settings
    }

    fn mode_of(path: &Path) -> u32 {
        match std::fs::metadata(path) {
            Ok(metadata) => metadata.permissions().mode() & 0o7777,
            Err(err) => panic!("stat {}: {err}", path.display()),
        }
    }

    // ---- rendering --------------------------------------------------------

    #[test]
    fn the_render_matches_the_golden_file() {
        assert_eq!(
            render_config(&lab_ap(), "mos-lab", "labsecret1").unwrap(),
            GOLDEN_CONFIG
        );
    }

    #[test]
    fn the_render_is_deterministic() {
        let ap = lab_ap();
        assert_eq!(
            render_config(&ap, "mos-lab", "labsecret1").unwrap(),
            render_config(&ap.clone(), "mos-lab", "labsecret1").unwrap()
        );
    }

    #[test]
    fn the_render_is_wpa2_psk_and_never_open() {
        let rendered = render_config(&lab_ap(), "mos-lab", "labsecret1").unwrap();

        for expected in [
            "wpa=2\n",
            "wpa_key_mgmt=WPA-PSK\n",
            "rsn_pairwise=CCMP\n",
            "auth_algs=1\n",
        ] {
            assert!(
                rendered.contains(expected),
                "{expected:?} missing, so the access point would not be WPA2-PSK: {rendered}"
            );
        }
        assert!(
            !rendered.contains("wpa=0") && !rendered.contains("wpa=1"),
            "an access point carrying a device credential must not be open or WPA1: {rendered}"
        );
    }

    #[test]
    fn the_country_code_is_emitted_and_advertised() {
        let rendered = render_config(&lab_ap(), "mos-lab", "labsecret1").unwrap();

        assert!(rendered.contains("country_code=DE\n"), "{rendered}");
        assert!(
            rendered.contains("ieee80211d=1\n"),
            "without ieee80211d the regulatory domain is carried and not advertised: {rendered}"
        );
    }

    #[test]
    fn a_country_code_that_is_not_a_regulatory_domain_is_rejected() {
        for bad in ["", "U", "USA", "U1", "US\nssid=pwned", "u s"] {
            assert!(
                validate_country_code(bad).is_err(),
                "{bad:?} was accepted as a regulatory domain"
            );
        }
        for good in ["US", "DE", "JP", "gb"] {
            assert!(
                validate_country_code(good).is_ok(),
                "{good:?} was rejected as a regulatory domain"
            );
        }
    }

    #[test]
    fn a_channel_outside_the_two_point_four_gigahertz_band_is_rejected() {
        for bad in [0u8, 15, 36, 255] {
            let ap = WifiApSettings {
                channel: bad,
                ..lab_ap()
            };
            assert!(
                render_config(&ap, "mos-lab", "labsecret1").is_err(),
                "channel {bad} was accepted"
            );
        }
        for good in [1u8, 6, 11, 14] {
            let ap = WifiApSettings {
                channel: good,
                ..lab_ap()
            };
            let rendered = render_config(&ap, "mos-lab", "labsecret1").unwrap();
            assert!(
                rendered.contains(&format!("channel={good}\n")),
                "{rendered}"
            );
        }
    }

    #[test]
    fn a_hostile_ssid_cannot_inject_configuration() {
        // A newline to end the directive and a whole second directive after it
        // — the entire escape, because hostapd has no quoting to break out of.
        let hostile = "evil\nssid=pwned\nwpa=0\n#";

        let rendered = render_config(&lab_ap(), hostile, "labsecret1").unwrap();

        assert!(
            !rendered.contains("pwned"),
            "hostile content reached the file verbatim: {rendered}"
        );
        assert!(
            !rendered.contains("wpa=0"),
            "a hostile SSID turned the access point open: {rendered}"
        );
        assert_eq!(
            rendered
                .lines()
                .filter(|line| line.contains("ssid"))
                .count(),
            2,
            "exactly one SSID directive and one ignore_broadcast_ssid: {rendered}"
        );
        assert!(
            rendered.contains(&format!("ssid2={}\n", hex::encode(hostile.as_bytes()))),
            "an SSID that cannot be carried raw must be emitted as hex: {rendered}"
        );
        assert_eq!(
            rendered,
            GOLDEN_CONFIG.replace(
                "ssid=mos-lab\n",
                &format!("ssid2={}\n", hex::encode(hostile.as_bytes()))
            ),
            "the render must be the golden file with only the SSID directive changed"
        );
    }

    #[test]
    fn every_unrepresentable_ssid_goes_to_hex_and_plain_ones_stay_readable() {
        for hostile in [
            "has\nnewline",
            "has\ttab",
            "has\0nul",
            "has\"quote",
            "has\\backslash",
            "caf\u{e9}",
            " leading",
            "trailing ",
        ] {
            let rendered = render_config(&lab_ap(), hostile, "labsecret1").unwrap();
            assert!(
                rendered.contains(&format!("ssid2={}\n", hex::encode(hostile.as_bytes()))),
                "{hostile:?} must be hex-encoded: {rendered}"
            );
            assert!(
                !rendered.contains(&format!("ssid={hostile}")),
                "{hostile:?} reached the file raw: {rendered}"
            );
        }
        for plain in [
            "mos-lab",
            "with space",
            "with#hash",
            "with'quote",
            "a.b:c_d",
        ] {
            let rendered = render_config(&lab_ap(), plain, "labsecret1").unwrap();
            assert!(
                rendered.contains(&format!("ssid={plain}\n")),
                "{plain:?} must stay readable: {rendered}"
            );
            assert!(!rendered.contains("ssid2="), "{rendered}");
        }
    }

    #[test]
    fn an_ssid_ieee_802_11_cannot_carry_is_rejected() {
        assert!(validate_ssid("").is_err(), "an empty SSID was accepted");
        assert!(validate_ssid(&"a".repeat(33)).is_err(), "33 bytes accepted");
        assert!(validate_ssid(&"a".repeat(32)).is_ok(), "32 bytes rejected");
        assert!(validate_ssid("mos-lab").is_ok());
        assert!(
            render_config(&lab_ap(), &"a".repeat(33), "labsecret1").is_err(),
            "hostapd rejects the whole file on an over-long SSID"
        );
    }

    #[test]
    fn a_sixty_four_character_hex_key_is_emitted_as_a_raw_pmk() {
        let pmk = "0123456789abcdef".repeat(4);
        assert_eq!(pmk.len(), 64);

        let rendered = render_config(&lab_ap(), "mos-lab", &pmk).unwrap();

        assert!(
            rendered.contains(&format!("wpa_psk={pmk}\n")),
            "a raw PMK must use wpa_psk; wpa_passphrase would be an over-long \
             passphrase and hostapd would reject the file: {rendered}"
        );
        assert!(!rendered.contains("wpa_passphrase="), "{rendered}");
    }

    #[test]
    fn a_key_hostapd_cannot_carry_is_an_error_that_does_not_name_it() {
        for bad in ["short7X", &"x".repeat(64), "has\nnewlineXX", "trailingX "] {
            let err = render_config(&lab_ap(), "mos-lab", bad).unwrap_err();
            let chain = format!("{err:#}");
            assert!(chain.contains("pre-shared key"), "{bad:?} produced {chain}");
            assert!(
                !chain.contains(bad),
                "the key leaked into the error: {chain}"
            );
        }
        // The boundaries themselves are accepted.
        for good in ["8charsXX", &"y".repeat(63)] {
            assert!(
                render_config(&lab_ap(), "mos-lab", good).is_ok(),
                "{good:?} is a legal WPA2 passphrase and was rejected"
            );
        }
    }

    #[test]
    fn the_length_refusal_is_the_same_sentence_for_every_length() {
        // Interpolating any property of the secret — its length included —
        // would make the sentence vary with the input; an identical refusal
        // for a short key and a long one proves the message names only the
        // rule. The refusal reaches an API client through the live-state tree
        // and the apply-task record, so a length in it is a disclosed fact
        // about a secret.
        let short = psk_directive("short7X").unwrap_err().to_string();
        let long = psk_directive(&"y".repeat(70)).unwrap_err().to_string();
        assert_eq!(
            short, long,
            "the refusal varies with the key, so it names a property of the secret"
        );
        assert!(
            !short.contains('7'),
            "the rejected key's length leaked into the refusal: {short}"
        );
        assert!(
            !long.contains("70"),
            "the rejected key's length leaked into the refusal: {long}"
        );
    }

    // ---- derived SSID -----------------------------------------------------

    #[test]
    fn the_derived_ssid_is_stable_and_distinct_per_device() {
        assert_eq!(derived_ssid(DEVICE_ID), derived_ssid(DEVICE_ID));
        assert_eq!(derived_ssid(DEVICE_ID), "mos-1a2b3c4d");
        assert_ne!(
            derived_ssid(DEVICE_ID),
            derived_ssid("ffffffffffffffffffffffffffffffff"),
            "two devices would advertise the same SSID"
        );
        assert!(
            validate_ssid(&derived_ssid(DEVICE_ID)).is_ok(),
            "a derived SSID must itself be a legal SSID"
        );
    }

    #[tokio::test]
    async fn an_absent_ssid_is_derived_from_the_device_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let ap = WifiApSettings {
            ssid: None,
            ..lab_ap()
        };

        let state = reconciler.apply(&settings_with(ap)).await.unwrap();

        assert_eq!(state["ssid"], json!("mos-1a2b3c4d"));
        assert_eq!(state["ssidSource"], json!("derived"));
        assert!(
            std::fs::read_to_string(&paths.config)
                .unwrap()
                .contains("ssid=mos-1a2b3c4d\n")
        );
    }

    #[tokio::test]
    async fn a_configured_ssid_is_used_verbatim_and_never_derived() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler.apply(&settings_with(lab_ap())).await.unwrap();

        assert_eq!(state["ssid"], json!("mos-lab"));
        assert_eq!(state["ssidSource"], json!("settings"));
        let rendered = std::fs::read_to_string(&paths.config).unwrap();
        assert!(rendered.contains("ssid=mos-lab\n"), "{rendered}");
        assert!(
            !rendered.contains("mos-1a2b3c4d"),
            "the configured SSID was overridden by the derived one: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_device_with_no_identity_and_no_ssid_refuses_to_start_the_radio() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let mut settings = settings_with(WifiApSettings {
            ssid: None,
            ..lab_ap()
        });
        settings.provisioning.device_id = None;

        let err = reconciler.apply(&settings).await.unwrap_err();

        assert!(format!("{err:#}").contains("wifi.ap.ssid"), "{err:#}");
        assert!(!paths.config.exists());
        assert!(reconciler.control.calls().is_empty());
    }

    // ---- where the key comes from -----------------------------------------

    #[tokio::test]
    async fn an_absent_key_comes_from_the_per_device_state_secret() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        seed_ap_psk(&paths.state_dir, SECRET_PSK);
        let ap = WifiApSettings {
            psk: None,
            ..lab_ap()
        };

        reconciler.apply(&settings_with(ap)).await.unwrap();

        assert!(
            std::fs::read_to_string(&paths.config)
                .unwrap()
                .contains(&format!("wpa_passphrase={SECRET_PSK}\n")),
            "the per-device key on STATE must be what the access point uses"
        );
    }

    #[tokio::test]
    async fn a_configured_key_wins_over_the_state_secret() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        seed_ap_psk(&paths.state_dir, SECRET_PSK);

        reconciler.apply(&settings_with(lab_ap())).await.unwrap();

        let rendered = std::fs::read_to_string(&paths.config).unwrap();
        assert!(
            rendered.contains("wpa_passphrase=labsecret1\n"),
            "{rendered}"
        );
        assert!(
            !rendered.contains(SECRET_PSK),
            "the settings key was ignored in favour of the state secret: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_device_with_no_key_anywhere_refuses_rather_than_using_a_constant() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let ap = WifiApSettings {
            psk: None,
            ..lab_ap()
        };

        let err = reconciler.apply(&settings_with(ap)).await.unwrap_err();

        let chain = format!("{err:#}");
        assert!(chain.contains("fleet-wide"), "{chain}");
        assert!(
            !paths.config.exists(),
            "a configuration was written without a key"
        );
        assert!(reconciler.control.calls().is_empty());
    }

    // ---- address and DHCP pool --------------------------------------------

    #[test]
    fn the_dhcp_pool_is_derived_from_the_access_point_address() {
        for (address, expected) in [
            (
                "192.168.4.1/24",
                "[Match]\nName=wlan0\n\n[Network]\nAddress=192.168.4.1/24\nDHCPServer=yes\n\n\
                 [DHCPServer]\nPoolOffset=2\nPoolSize=253\n",
            ),
            (
                "10.7.0.1/16",
                "[Match]\nName=wlan0\n\n[Network]\nAddress=10.7.0.1/16\nDHCPServer=yes\n\n\
                 [DHCPServer]\nPoolOffset=2\nPoolSize=65533\n",
            ),
            (
                "192.168.9.129/25",
                "[Match]\nName=wlan0\n\n[Network]\nAddress=192.168.9.129/25\nDHCPServer=yes\n\n\
                 [DHCPServer]\nPoolOffset=2\nPoolSize=125\n",
            ),
            (
                "192.168.4.200/24",
                "[Match]\nName=wlan0\n\n[Network]\nAddress=192.168.4.200/24\nDHCPServer=yes\n\n\
                 [DHCPServer]\nPoolOffset=201\nPoolSize=54\n",
            ),
            (
                "192.168.4.253/30",
                "[Match]\nName=wlan0\n\n[Network]\nAddress=192.168.4.253/30\nDHCPServer=yes\n\n\
                 [DHCPServer]\nPoolOffset=2\nPoolSize=1\n",
            ),
        ] {
            assert_eq!(
                render_networkd("wlan0", address).unwrap(),
                expected,
                "pool for {address}"
            );
        }
    }

    #[test]
    fn the_pool_always_lies_inside_the_subnet_and_excludes_the_host() {
        for (address, host, last) in [
            ("192.168.4.1/24", 1u32, 255u32),
            ("192.168.9.129/25", 1, 127),
            ("192.168.4.200/24", 200, 255),
        ] {
            let (ip, prefix) = parse_cidr(address).unwrap();
            let (offset, size) = dhcp_pool(ip, prefix).unwrap();
            assert!(
                offset > host,
                "the pool would hand out the AP's own address"
            );
            assert!(
                offset + size <= last,
                "the pool runs past the broadcast address of {address}"
            );
            assert!(size > 0, "no client could get an address on {address}");
        }
    }

    #[test]
    fn an_address_that_cannot_carry_a_pool_is_rejected() {
        for bad in [
            "192.168.4.1",            // no prefix at all
            "192.168.4.1/",           // no prefix length
            "192.168.4.1/33",         // not a v4 prefix
            "192.168.4.1/31",         // no room for a pool
            "192.168.4.1/32",         // no room for a pool
            "192.168.4.0/24",         // the subnet address
            "192.168.4.255/24",       // the broadcast address
            "192.168.4.254/24",       // leaves nothing to hand out
            "not-an-address/24",      // not an address
            "192.168.4.1/24\nEvil=1", // an injection attempt
        ] {
            assert!(
                render_networkd("wlan0", bad).is_err(),
                "{bad:?} was accepted as an access-point address"
            );
        }
    }

    #[tokio::test]
    async fn an_unusable_address_stops_the_apply_before_the_unit_is_started() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "inactive", "disabled");
        let ap = WifiApSettings {
            address: "192.168.4.255/24".to_string(),
            ..lab_ap()
        };

        let err = reconciler.apply(&settings_with(ap)).await.unwrap_err();

        assert!(format!("{err:#}").contains("broadcast"), "{err:#}");
        assert!(
            reconciler.control.calls().is_empty(),
            "hostapd was started against an address that cannot serve clients"
        );
    }

    // ---- what apply writes ------------------------------------------------

    #[tokio::test]
    async fn apply_writes_the_golden_config_at_0600_creating_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler.apply(&settings_with(lab_ap())).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            GOLDEN_CONFIG
        );
        assert_eq!(
            mode_of(&paths.config),
            0o600,
            "the file carries the pre-shared key and must not be readable by anyone else"
        );
        assert_eq!(state["accessPoint"], json!("applied"));
        assert_eq!(state["config"], json!(paths.config.display().to_string()));
        assert_eq!(state["unit"], json!("hostapd@wlan0.service"));
        assert_eq!(reconciler.name(), "wifiAp");
        assert_eq!(reconciler.subtree(), "wifi");
    }

    #[tokio::test]
    async fn a_leftover_temporary_file_does_not_widen_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        // The shape an interrupted run leaves behind: the temporary name
        // already exists, so the mode given at open time is never applied.
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let leftover = paths.config_dir.join(".wlan0.conf.micad-tmp");
        std::fs::write(&leftover, "stale").unwrap();
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o666)).unwrap();

        reconciler.apply(&settings_with(lab_ap())).await.unwrap();

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

        reconciler.apply(&settings_with(lab_ap())).await.unwrap();

        for directory in [&paths.config_dir, &paths.network_dir] {
            let leftovers: Vec<_> = std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .filter(|name| name.to_string_lossy().contains("micad-tmp"))
                .collect();
            assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        }
    }

    #[tokio::test]
    async fn a_running_access_point_gets_an_address_and_a_dhcp_server() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler.apply(&settings_with(lab_ap())).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(paths.networkd()).unwrap(),
            GOLDEN_NETWORKD,
            "an access point that hands out no address is one nothing can reach"
        );
        assert_eq!(state["networkdUnit"], json!("90-wifi-ap-wlan0.network"));
        assert_eq!(reconciler.reloader.calls(), vec!["reload".to_string()]);
    }

    // ---- surviving the network reconciler's sweep -------------------------

    #[tokio::test]
    async fn the_rendered_networkd_unit_survives_the_network_reconcilers_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        reconciler.apply(&settings_with(lab_ap())).await.unwrap();
        assert!(paths.networkd().exists());

        // The network reconciler deletes every `*-mos-*.network` it did not
        // itself render. Sharing a directory with it means this reconciler's
        // unit has to be outside that pattern, and running the real thing over
        // the same directory is the only check that says so.
        NetworkReconciler::new(
            paths.network_dir.clone(),
            MockReload::new(),
            NoDelete,
            // No tunnel in this test's tree, so no key is ever drawn; the
            // directory is inside the same temporary tree either way.
            Keystore::under(&paths.state_dir, None),
        )
        .apply(&Settings::default())
        .await
        .unwrap();

        assert!(
            paths.networkd().exists(),
            "the network reconciler swept the access point's networkd unit away"
        );
        assert_eq!(
            std::fs::read_to_string(paths.networkd()).unwrap(),
            GOLDEN_NETWORKD
        );
    }

    #[tokio::test]
    async fn the_networkd_unit_sorts_after_the_units_it_must_not_override() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler.apply(&settings_with(lab_ap())).await.unwrap();

        let name = networkd_file_name("wlan0");
        for earlier in ["50-mos-wlan0.network", "80-dhcp.network"] {
            assert!(
                name.as_str() > earlier,
                "networkd applies the first matching unit in lexical order, so \
                 {name} must sort after {earlier}"
            );
        }
        assert!(paths.networkd().exists());
    }

    #[tokio::test]
    async fn renaming_the_interface_removes_the_previous_networkd_unit() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        reconciler.apply(&settings_with(lab_ap())).await.unwrap();

        let renamed = WifiApSettings {
            interface: "wlan1".to_string(),
            ..lab_ap()
        };
        reconciler.apply(&settings_with(renamed)).await.unwrap();

        assert!(
            !paths.networkd().exists(),
            "the old interface would keep an AP address and a DHCP server on a \
             link micad no longer manages"
        );
        assert!(paths.network_dir.join("90-wifi-ap-wlan1.network").exists());
    }

    // ---- unit lifecycle and convergence -----------------------------------

    #[tokio::test]
    async fn off_to_always_enables_then_starts_and_back_to_off_stops_then_disables() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let off = reconciler
            .apply(&settings_with(WifiApSettings {
                mode: ApMode::Off,
                ..lab_ap()
            }))
            .await
            .unwrap();
        assert_eq!(off["accessPoint"], json!("stopped"));
        assert!(
            reconciler.control.calls().is_empty(),
            "an already-stopped access point got calls: {:?}",
            reconciler.control.calls()
        );
        assert!(reconciler.reloader.calls().is_empty());
        assert!(
            !paths.config.exists(),
            "a stopped access point rendered a key"
        );

        let on = reconciler.apply(&settings_with(lab_ap())).await.unwrap();
        assert_eq!(on["accessPoint"], json!("applied"));
        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable hostapd@wlan0.service".to_string(),
                "start hostapd@wlan0.service".to_string(),
            ]
        );
        assert_eq!(on["activeState"], json!("active"));
        assert_eq!(on["unitFileState"], json!("enabled-runtime"));

        let back_off = reconciler
            .apply(&settings_with(WifiApSettings {
                mode: ApMode::Off,
                ..lab_ap()
            }))
            .await
            .unwrap();
        assert_eq!(back_off["accessPoint"], json!("stopped"));
        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable hostapd@wlan0.service".to_string(),
                "start hostapd@wlan0.service".to_string(),
                "stop hostapd@wlan0.service".to_string(),
                "disable hostapd@wlan0.service".to_string(),
            ]
        );
        assert_eq!(back_off["networkdUnit"], json!(null));
        assert!(
            !paths.networkd().exists(),
            "a stopped access point must not leave a DHCP server behind"
        );
    }

    #[tokio::test]
    async fn provisioning_mode_starts_the_access_point_exactly_as_always_does() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(WifiApSettings {
                mode: ApMode::Provisioning,
                ..lab_ap()
            }))
            .await
            .unwrap();

        assert_eq!(state["mode"], json!("provisioning"));
        assert_eq!(state["accessPoint"], json!("applied"));
        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            GOLDEN_CONFIG
        );
        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable hostapd@wlan0.service".to_string(),
                "start hostapd@wlan0.service".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn reapplying_identical_settings_is_a_no_op_with_zero_bus_calls() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let settings = settings_with(lab_ap());

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
            GOLDEN_CONFIG
        );
        assert_eq!(first["accessPoint"], json!("applied"));
        assert_eq!(second["accessPoint"], json!("unchanged"));
    }

    #[tokio::test]
    async fn an_already_converged_access_point_needs_no_calls_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.config, GOLDEN_CONFIG).unwrap();
        std::fs::create_dir_all(&paths.network_dir).unwrap();
        std::fs::write(paths.networkd(), GOLDEN_NETWORKD).unwrap();

        let state = reconciler.apply(&settings_with(lab_ap())).await.unwrap();

        assert!(
            reconciler.control.calls().is_empty(),
            "converged system got calls: {:?}",
            reconciler.control.calls()
        );
        assert!(
            reconciler.reloader.calls().is_empty(),
            "converged system got a reload"
        );
        assert_eq!(state["accessPoint"], json!("unchanged"));
    }

    #[tokio::test]
    async fn changing_the_config_of_a_running_access_point_restarts_it() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.config, GOLDEN_CONFIG).unwrap();
        std::fs::create_dir_all(&paths.network_dir).unwrap();
        std::fs::write(paths.networkd(), GOLDEN_NETWORKD).unwrap();

        let state = reconciler
            .apply(&settings_with(WifiApSettings {
                ssid: Some("mos-annex".to_string()),
                ..lab_ap()
            }))
            .await
            .unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec!["restart hostapd@wlan0.service".to_string()],
            "hostapd reads its configuration once at start; without a restart \
             the radio keeps beaconing the previous SSID and key"
        );
        assert!(
            std::fs::read_to_string(&paths.config)
                .unwrap()
                .contains("ssid=mos-annex\n")
        );
        assert_eq!(state["accessPoint"], json!("applied"));
    }

    #[tokio::test]
    async fn changing_the_address_of_a_running_access_point_reloads_networkd_once() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.config, GOLDEN_CONFIG).unwrap();
        std::fs::create_dir_all(&paths.network_dir).unwrap();
        std::fs::write(paths.networkd(), GOLDEN_NETWORKD).unwrap();

        reconciler
            .apply(&settings_with(WifiApSettings {
                address: "10.7.0.1/16".to_string(),
                ..lab_ap()
            }))
            .await
            .unwrap();

        assert_eq!(reconciler.reloader.calls(), vec!["reload".to_string()]);
        let rendered = std::fs::read_to_string(paths.networkd()).unwrap();
        assert!(rendered.contains("Address=10.7.0.1/16\n"), "{rendered}");
        assert!(
            rendered.contains("PoolSize=65533\n"),
            "the pool must move with the address: {rendered}"
        );
    }

    // ---- the single-radio conflict ----------------------------------------

    #[tokio::test]
    async fn an_access_point_on_the_station_s_radio_reports_conflict_and_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with_station(lab_ap(), "wlan0"))
            .await
            .unwrap();

        assert_eq!(state["accessPoint"], json!("conflict"));
        assert!(
            reconciler.control.calls().is_empty(),
            "the reconciler fought the station for the radio: {:?}",
            reconciler.control.calls()
        );
        assert!(
            reconciler.reloader.calls().is_empty(),
            "a conflicting configuration still touched the network stack"
        );
        assert!(
            !paths.config.exists() && !paths.config_dir.exists(),
            "a conflicting configuration still wrote a hostapd config"
        );
        assert!(
            !paths.network_dir.exists(),
            "a conflicting configuration still wrote a networkd unit"
        );
        let rendered = serde_json::to_string(&state).unwrap();
        assert!(
            rendered.contains("wifi.client is enabled on wlan0"),
            "the conflict must say what is wrong: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_conflict_does_not_stop_the_station_s_unit() {
        let dir = tempfile::tempdir().unwrap();
        // The station is up and running on this radio, which is exactly the
        // state the AP reconciler must not "fix".
        let (reconciler, _paths) = fixture(dir.path(), "active", "enabled");

        reconciler
            .apply(&settings_with_station(lab_ap(), "wlan0"))
            .await
            .unwrap();

        assert!(
            reconciler.control.calls().is_empty(),
            "stopping a unit the station reconciler owns would flap forever: {:?}",
            reconciler.control.calls()
        );
    }

    #[tokio::test]
    async fn a_station_on_a_different_radio_is_not_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with_station(lab_ap(), "wlan1"))
            .await
            .unwrap();

        assert_eq!(
            state["accessPoint"],
            json!("applied"),
            "two radios can carry both roles; refusing that is as wrong as flapping"
        );
        assert!(paths.config.exists());
        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable hostapd@wlan0.service".to_string(),
                "start hostapd@wlan0.service".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn a_disabled_station_on_the_same_radio_is_not_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "inactive", "disabled");
        let mut settings = settings_with_station(lab_ap(), "wlan0");
        settings.wifi.client.enabled = false;

        let state = reconciler.apply(&settings).await.unwrap();

        assert_eq!(
            state["accessPoint"],
            json!("applied"),
            "a station that is switched off is not using the radio"
        );
    }

    #[tokio::test]
    async fn an_access_point_that_is_off_never_conflicts_with_the_station() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with_station(
                WifiApSettings {
                    mode: ApMode::Off,
                    ..lab_ap()
                },
                "wlan0",
            ))
            .await
            .unwrap();

        assert_eq!(
            state["accessPoint"],
            json!("stopped"),
            "an access point that is off is not contending for anything"
        );
    }

    // ---- secret hygiene ---------------------------------------------------

    #[tokio::test]
    async fn the_key_reaches_the_config_file_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        seed_ap_psk(&paths.state_dir, SECRET_PSK);
        let ap = WifiApSettings {
            psk: None,
            ssid: None,
            ..lab_ap()
        };

        let state = reconciler.apply(&settings_with(ap)).await.unwrap();

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
            state["secured"],
            json!(true),
            "the live state reports that the access point is protected, never how"
        );
    }

    #[tokio::test]
    async fn a_key_that_cannot_be_rendered_leaks_nothing_and_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let ap = WifiApSettings {
            psk: Some(format!("bad\n{SECRET_PSK}")),
            ..lab_ap()
        };

        let err = reconciler.apply(&settings_with(ap)).await.unwrap_err();

        let chain = format!("{err:#}");
        assert!(chain.contains("pre-shared key"), "{chain}");
        assert!(!chain.contains(SECRET_PSK), "the key leaked: {chain}");
        assert!(!paths.config.exists());
        assert!(reconciler.control.calls().is_empty());
    }

    // ---- interface validation and containment -----------------------------

    #[test]
    fn interface_names_that_are_not_names_are_rejected() {
        for bad in [
            "",
            ".",
            "..",
            "../../etc/passwd",
            "wlan0/../evil",
            "wlan 0",
            "wlan0\n",
            "wlan0;reboot",
            "seventeen_charsx",
        ] {
            assert!(
                validate_interface(bad).is_err(),
                "{bad:?} was accepted as an interface name"
            );
        }
        for good in [
            "wlan0",
            "wlp2s0",
            "wlan0.1",
            "wl-an_0",
            "eth0:1",
            "sixteencharsnam",
        ] {
            assert!(validate_interface(good).is_ok(), "{good:?} was rejected");
        }
    }

    #[tokio::test]
    async fn a_traversing_interface_name_writes_nothing_anywhere() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let err = reconciler
            .apply(&settings_with(WifiApSettings {
                interface: "../../evil".to_string(),
                ..lab_ap()
            }))
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("wifi.ap.interface"), "{err:#}");
        assert!(
            !paths.config_dir.exists() && !paths.network_dir.exists(),
            "a rejected interface name must not create anything"
        );
        assert!(reconciler.control.calls().is_empty());
    }

    #[tokio::test]
    async fn every_path_the_reconciler_writes_stays_inside_the_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler.apply(&settings_with(lab_ap())).await.unwrap();

        for path in [
            &paths.config,
            &paths.config_dir,
            &paths.network_dir,
            &paths.state_dir,
        ] {
            assert!(
                path.starts_with(dir.path()),
                "{} escapes the tempdir",
                path.display()
            );
        }
        assert_ne!(
            state["config"],
            json!(format!("{DEFAULT_CONFIG_DIR}/wlan0.conf")),
            "a test must never name the host's hostapd configuration"
        );
    }
}

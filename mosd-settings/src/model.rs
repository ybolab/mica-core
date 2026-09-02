//! Typed settings tree (schema v9) and its dot-path accessors.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::SettingsError;
use crate::path::{json_path_get, json_path_set, split_path};

/// Current settings schema version written by this crate.
pub const SCHEMA_VERSION: u32 = 9;

/// Persistent mosd settings tree (schema v9).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Schema version of this tree; read-only through [`Settings::set`].
    pub schema_version: u32,
    /// System hostname.
    pub hostname: String,
    /// Per-interface network configuration, keyed by interface name.
    pub network: BTreeMap<String, IfaceSettings>,
    /// Access control settings.
    #[serde(default)]
    pub access: AccessSettings,
    /// First-boot self-provisioning status.
    #[serde(default)]
    pub provisioning: ProvisioningSettings,
    /// WiFi station and access-point settings.
    #[serde(default)]
    pub wifi: WifiSettings,
    /// Container engine policy.
    #[serde(default)]
    pub container: ContainerSettings,
    /// MQTT broker and bridge policy.
    #[serde(default)]
    pub mqtt: MqttSettings,
    /// NTP server and presentation-timezone settings.
    #[serde(default)]
    pub time: TimeSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            hostname: "mos".to_string(),
            network: BTreeMap::new(),
            access: AccessSettings::default(),
            provisioning: ProvisioningSettings::default(),
            wifi: WifiSettings::default(),
            container: ContainerSettings::default(),
            mqtt: MqttSettings::default(),
            time: TimeSettings::default(),
        }
    }
}

/// Time synchronization and presentation-timezone settings (PLAN-044).
///
/// Exactly two knobs, deliberately. There is no enable or pause switch here,
/// in the API or in the UI — `systemd-timesyncd` is an always-running base
/// service — and the polling, retry and saved-clock intervals are pinned base
/// policy shipped in `/etc/systemd/timesyncd.conf.d/50-mos.conf`, never
/// settings. Machine, RTC, API and log time stay UTC; the timezone below is
/// presentation only.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeSettings {
    /// Managed NTP servers.
    pub ntp: NtpSettings,
    /// IANA timezone name used for presentation and explicitly local
    /// schedules; it never moves the machine clock off UTC.
    pub timezone: String,
}

impl Default for TimeSettings {
    fn default() -> Self {
        Self {
            ntp: NtpSettings::default(),
            timezone: "UTC".to_string(),
        }
    }
}

/// The managed NTP server list.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NtpSettings {
    /// Server names or addresses, rendered in order into timesyncd's runtime
    /// `NTP=` list. Empty means the image's fallback pool is used — an empty
    /// list is "no operator override", not "no time synchronization".
    pub servers: Vec<String>,
}

/// The most servers one `time.ntp.servers` list may carry.
///
/// timesyncd polls one selected server at a time and steps through the list
/// only on failure, so a longer list buys redundancy, not accuracy; eight is
/// well past any real deployment and keeps the rendered `NTP=` line bounded.
pub const MAX_NTP_SERVERS: usize = 8;

/// RFC 1035's bound on a full domain name, which also covers any IP literal.
const MAX_NTP_SERVER_LEN: usize = 253;

/// Longest IANA zone name accepted; the longest real one is around 32 bytes.
const MAX_TIMEZONE_LEN: usize = 64;

/// Refuse a `time.ntp.servers` list timesyncd's `NTP=` line cannot carry.
///
/// **This is the one statement of the predicate**: the time reconciler renders
/// the list space-separated into a `[Time]` drop-in, so a value carrying
/// whitespace, a control character or `=`/`#` could end its own assignment or
/// smuggle a second one. The charset is a hostname's or IP literal's — ASCII
/// alphanumerics, `.`, `-` and `:` (IPv6) — which is also everything timesyncd
/// itself will resolve. Bounded in count by [`MAX_NTP_SERVERS`] and per entry
/// by RFC 1035's 253 bytes; duplicates are refused because timesyncd steps
/// through the list on failure and a duplicate is a retry disguised as
/// redundancy.
///
/// Not enforced in `Deserialize`, deliberately, for [`validate_wifi_psk`]'s
/// reason: a bound enforced at load would turn one bad value already on disk
/// into a device whose every unrelated write fails. [`Settings::set`] calls
/// this for a write that changes the `time` subtree.
///
/// # Errors
///
/// Returns the sentence the refusal carries.
pub fn validate_ntp_servers(servers: &[String]) -> Result<(), String> {
    if servers.len() > MAX_NTP_SERVERS {
        return Err(format!(
            "at most {MAX_NTP_SERVERS} NTP servers are supported; timesyncd only ever polls one \
             and steps through the rest on failure"
        ));
    }
    for (index, server) in servers.iter().enumerate() {
        if server.is_empty() {
            return Err(format!("NTP server {} is empty", index + 1));
        }
        if server.len() > MAX_NTP_SERVER_LEN {
            return Err(format!(
                "NTP server {server:?} is longer than {MAX_NTP_SERVER_LEN} characters"
            ));
        }
        if !server
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
        {
            return Err(format!(
                "NTP server {server:?} contains a character a host name or IP address cannot \
                 have; use ASCII letters, digits, '.', '-' or ':'"
            ));
        }
        if servers[..index].contains(server) {
            return Err(format!("NTP server {server:?} is listed twice"));
        }
    }
    Ok(())
}

/// Refuse a `time.timezone` value that is not an IANA zone name.
///
/// Syntactic only, and deterministically so: the rule is the tzdata NAME
/// grammar — `/`-separated components of ASCII letters, digits, `.`, `_`,
/// `+` and `-`, none empty, none `.` or `..`, none starting with a digit's
/// worth of path games — and never a lookup against the host's tzdata, so the
/// same input passes or fails on every machine a test runs on. Whether the
/// zone actually exists on the device is checked where it can only be checked,
/// at reconcile time against `/usr/share/zoneinfo`.
///
/// # Errors
///
/// Returns the sentence the refusal carries.
pub fn validate_timezone_name(zone: &str) -> Result<(), String> {
    const RULES: &str = "a timezone is an IANA zone name such as \"UTC\" or \"Europe/Berlin\": \
                         '/'-separated components of ASCII letters, digits, '.', '_', '+' and '-'";
    if zone.is_empty() || zone.len() > MAX_TIMEZONE_LEN {
        return Err(format!(
            "{RULES}, between 1 and {MAX_TIMEZONE_LEN} characters"
        ));
    }
    for component in zone.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(RULES.to_string());
        }
        if !component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
        {
            return Err(RULES.to_string());
        }
    }
    Ok(())
}

/// The `time` subtree's whole write rule, called by [`Settings::set`].
fn validate_time_settings(time: &TimeSettings) -> Result<(), String> {
    validate_ntp_servers(&time.ntp.servers)?;
    validate_timezone_name(&time.timezone)
}

/// Container engine policy, reconciled by `ContainerReconciler`.
///
/// Named for the capability, not the implementation: if the engine is replaced,
/// this key and its APID pane are unchanged and only the binaries move.
/// Disabled by default, and false means nothing runs — the engine is
/// daemonless, with no socket and no service to leave stopped, so what this
/// switch gates is whether `/etc/containers/systemd` is bound from STATE.
/// Unbound, that path is the empty directory inside the read-only verity root,
/// Quadlet finds nothing to parse, and no container unit exists to be started.
/// The default is false because mos does not build rootless, so containers run
/// root-capable and turning this on grants root-equivalent capability to
/// whatever can write a `.container` file into STATE. That is the switch's
/// purpose, not a side effect.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContainerSettings {
    /// Whether the Quadlet directory is bound from STATE and container units
    /// may run.
    pub enabled: bool,
}

/// MQTT policy: the master switch for the broker and the bridge, and the
/// listener and credential policy the broker is rendered from.
///
/// `enabled` is a master switch and nothing else. False means neither the
/// broker nor the bridge runs: no `mos-mqtt-broker.service`, no
/// `mos-mqttd.service`. It validates nothing and depends on nothing below it —
/// no combination of `listen` and `auth` makes it mean anything other than "run
/// both" or "run neither". `listen` and `auth` are a separate configuration,
/// deliberately not coupled to the switch: no code may refuse to start on a
/// listen/auth combination. A broker bound off-host with `auth.enabled = false`
/// is worth a loud WARN in the journal and not a gate, because an operator who
/// widened the bind made a decision and a daemon that answers by quietly not
/// starting is one whose reason for being down cannot be read anywhere.
///
/// The default is false because enabling the pair opens an application-data
/// plane and may expose a listener outside loopback. That is an explicit
/// operator decision, not a service every device starts merely because its
/// binaries ship in the image.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MqttSettings {
    /// Whether the broker and the bridge run at all.
    pub enabled: bool,
    /// Where the broker listens.
    pub listen: MqttListenSettings,
    /// Whether the broker demands credentials.
    pub auth: MqttAuthSettings,
}

/// Where the broker listens.
///
/// Loopback and the MQTT default port: the bridge is an on-device client, so
/// the reachable-by-default listener a wider bind would create is one nobody
/// asked for. Widening it is a deliberate operator edit, and -- see
/// [`MqttSettings`] -- nothing refuses to start because of what is here.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MqttListenSettings {
    /// Address the broker binds.
    pub address: String,
    /// TCP port the broker listens on.
    pub port: u16,
}

impl Default for MqttListenSettings {
    fn default() -> Self {
        Self {
            address: "127.0.0.1".to_string(),
            port: 1883,
        }
    }
}

/// Whether the broker demands credentials from a connecting client.
///
/// **There is no username and no password here, and that absence is the
/// point.** System settings are management data and never enter MQTT, but
/// credential material still does not belong in the ordinary settings value
/// returned to authorized management clients. The broker reads its accounts from
/// `/var/lib/mos/mqtt-broker-users.toml` on STATE instead, which is how the
/// device password is already handled: the tree carries the policy, the STATE
/// file carries the secret.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MqttAuthSettings {
    /// Whether a client must authenticate to connect.
    pub enabled: bool,
}

/// Access control settings.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessSettings {
    /// Web admin credentials; absent until apid sets them.
    #[serde(rename = "webAdmin", default, skip_serializing_if = "Option::is_none")]
    pub web_admin: Option<WebAdminSettings>,
    /// SSH channel policy.
    #[serde(default)]
    pub ssh: SshSettings,
    /// Local console policy.
    #[serde(default)]
    pub console: ConsoleSettings,
    /// Device credential metadata; never holds a plaintext secret.
    #[serde(default)]
    pub device: DeviceCredentialSettings,
    /// Bearer API tokens, hashes only.
    ///
    /// Empty by default, and empty is not written out: a device that never
    /// minted a token has a v8 document identical to its v7 form but for the
    /// version integer, which is what makes the v7 -> v8 bump additive and the
    /// A/B rollback survivable (see [`crate::MigrateV7ToV8`]).
    ///
    /// Under `access` rather than beside it because apid's gate already reads
    /// the `access` subtree on every request, so the token set the check needs
    /// is in hand at the moment the check runs; a sibling root would have cost
    /// a second bus round trip per request.
    ///
    /// Written as a whole JSON array through the dot-path API -- the path
    /// syntax has no array indexing -- so mint and revoke are read-modify-write
    /// of the list. Every entry must satisfy [`crate::validate_api_tokens`]
    /// before it is stored.
    ///
    /// Declared last so the TOML serializer emits this array of tables after
    /// every other key of `access`.
    #[serde(rename = "apiTokens", default, skip_serializing_if = "Vec::is_empty")]
    pub api_tokens: Vec<ApiToken>,
}

/// Web admin credentials, written by apid.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebAdminSettings {
    /// Argon2id password hash in PHC string format.
    pub password_hash: String,
}

/// SSH channel policy, reconciled into sshd configuration.
///
/// Disabled by default: `prod` images ship sshd but never open it without an
/// authenticated admin action (see `docs/design/access.md` section 5).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SshSettings {
    /// Whether sshd is started.
    pub enabled: bool,
    /// TCP port sshd listens on.
    pub port: u16,
    /// Whether the root account may log in; phase 1 has only that account.
    #[serde(rename = "permitRootLogin")]
    pub permit_root_login: bool,
    /// Whether password authentication is offered; phase 1 auth is the device
    /// password.
    #[serde(rename = "passwordAuthentication")]
    pub password_authentication: bool,
    /// Addresses sshd binds to; empty means every address.
    #[serde(rename = "listenAddresses")]
    pub listen_addresses: Vec<String>,
    /// Public keys rendered into the root account's `authorized_keys` file.
    ///
    /// Empty by default: a key baked into the signed rootfs would let whoever
    /// holds its private half into every device built from that image. Keys
    /// arrive one at a time through an authenticated admin action, and every
    /// entry must satisfy [`crate::validate_authorized_keys`] before it is
    /// rendered.
    ///
    /// Declared last so the TOML serializer emits this array of tables after
    /// every scalar key of `access.ssh`.
    #[serde(rename = "authorizedKeys")]
    pub authorized_keys: Vec<AuthorizedKey>,
}

impl Default for SshSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 22,
            permit_root_login: true,
            password_authentication: true,
            listen_addresses: Vec::new(),
            authorized_keys: Vec::new(),
        }
    }
}

/// One SSH public key authorized to log in.
///
/// The comment lives in its own field rather than inside `key` so that the
/// canonical key text is what duplicate detection runs on: two operators
/// pasting the same key under different labels must not end up with two
/// entries granting the same access.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedKey {
    /// Canonical single-line key text, `<type> <base64blob>`, with no comment.
    pub key: String,
    /// Operator-supplied label; absent when the key was pasted without one.
    #[serde(rename = "comment", default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Local console policy.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConsoleSettings {
    /// Whether the tty3 root shell is started; only the `debug` image profile
    /// ships that shell at all.
    #[serde(rename = "shellEnabled")]
    pub shell_enabled: bool,
}

/// Device credential metadata.
///
/// Holds the hash of the per-device password and its revision, never the
/// password itself. Both stay `None`/`0` in a freshly built tree: a non-empty
/// default here would be a fleet-wide shared secret baked into the signed
/// rootfs.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceCredentialSettings {
    /// Argon2id password hash in PHC string format; absent until first boot
    /// generates the credential.
    #[serde(
        rename = "passwordHash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub password_hash: Option<String>,
    /// Revision of the stored credential, bumped on every regeneration.
    pub generation: u32,
}

/// One bearer API token, as the settings tree holds it.
///
/// The plaintext secret is not here and is nowhere else on the device: only
/// `hash` is stored, so a lost token is replaced rather than recovered. That
/// is the posture [`DeviceCredentialSettings`] already takes, and it is why
/// `hash` sits on apid's redaction denylist -- mosd answers a settings read
/// with the subtree verbatim, so a digest served in the clear would be an
/// offline guessing target handed to every authenticated reader.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiToken {
    /// Stable identity of this token, lowercase hex.
    ///
    /// Identity is this field and never a list position: an index is
    /// meaningful only against the list the caller last read, and a concurrent
    /// mint slides it onto a different entry.
    pub id: String,
    /// Operator-supplied label, the only thing that tells one token from
    /// another in a listing.
    pub name: String,
    /// SHA-256 hex digest of the token secret, lowercase, 64 characters.
    ///
    /// SHA-256 and not argon2id deliberately: the secret is machine-generated
    /// and has nothing to guess, so a work factor would buy no security and
    /// would be paid on every API request rather than once per login.
    pub hash: String,
    /// Seconds since the UNIX epoch as the device clock read them when the
    /// token was minted, saturating at 0.
    ///
    /// **A label, never a deadline.** The image this daemon runs on enables no
    /// RTC sync unit and no time daemon, so this reading is whatever the
    /// device's clock happened to say and may be wrong by any amount; 0 means
    /// the clock was unset or before the epoch. It is displayed and ordered by,
    /// and it is compared against nothing. There is deliberately no `expiresAt`
    /// beside it: an expiry enforced against an untrusted clock is worse than
    /// no expiry at all, and revocation is the whole lifecycle.
    ///
    /// [`crate::validate_api_tokens`] therefore places no bound on this value.
    /// Rejecting an implausible reading would turn a wrong clock into a mint
    /// failure, which is the untrusted clock deciding whether the operator may
    /// have a credential.
    pub created: u64,
}

/// First-boot self-provisioning status.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProvisioningSettings {
    /// Whether first-boot provisioning has run to completion.
    pub state: ProvisioningState,
    /// Device identity assigned at first boot, lowercase hex.
    #[serde(rename = "deviceId", default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Seeding revision that produced this tree.
    #[serde(rename = "seededGeneration")]
    pub seeded_generation: u32,
}

/// Stage of first-boot self-provisioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProvisioningState {
    /// The device has not provisioned itself yet.
    #[default]
    Pending,
    /// First-boot provisioning finished; the tree is the device's own.
    Complete,
}

/// WiFi settings, reconciled by connd into wpa_supplicant and hostapd.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WifiSettings {
    /// Station (uplink) configuration.
    pub client: WifiClientSettings,
    /// Access-point (provisioning) configuration.
    pub ap: WifiApSettings,
}

/// WiFi station configuration.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WifiClientSettings {
    /// Whether the station role is started.
    pub enabled: bool,
    /// Interface the station role runs on.
    pub interface: String,
    /// Known networks, most preferred by `priority`.
    ///
    /// Written as a whole JSON array through the dot-path API; the path syntax
    /// has no array indexing.
    pub networks: Vec<WifiNetwork>,
}

impl Default for WifiClientSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interface: "wlan0".to_string(),
            networks: Vec::new(),
        }
    }
}

/// One known WiFi network.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WifiNetwork {
    /// Network name.
    pub ssid: String,
    /// Pre-shared key; absent means an open network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psk: Option<String>,
    /// Whether the network hides its SSID.
    #[serde(default)]
    pub hidden: bool,
    /// Selection preference; higher wins.
    #[serde(default)]
    pub priority: i32,
}

/// Characters a raw 256-bit pre-shared key occupies, spelled in hex.
pub const RAW_PMK_LEN: usize = 64;

/// IEEE 802.11i's shortest WPA2 passphrase.
pub const MIN_PASSPHRASE_LEN: usize = 8;

/// IEEE 802.11i's longest.
pub const MAX_PASSPHRASE_LEN: usize = 63;

/// True when `value` can be carried inside a wpa_supplicant double-quoted
/// string with no way of ending the string early.
///
/// Printable ASCII only, minus the quote that would close the string and the
/// backslash that some wpa_supplicant string forms treat as an escape. A
/// newline is excluded by the printable range, which is the character that
/// would otherwise let a value close its `network={…}` block and append
/// directives of its own.
///
/// **This is the one statement of the predicate.** `mosd`'s station renderer
/// calls it for both the SSID and the pre-shared key, and
/// [`validate_wifi_psk`] calls it so a write surface refuses what the renderer
/// cannot carry rather than storing a value that dies at render time. A second
/// copy could disagree with the first, which is the defect one level up.
#[must_use]
pub fn is_wpa_quotable(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| (0x20..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\')
}

/// Refuse a [`WifiNetwork::psk`] no WPA2 supplicant could use.
///
/// **Lifted out of the station reconciler's renderer**: the
/// bound used to live inside `mosd`'s private `encode_psk`, so the one crate
/// that holds the typed model could not state its own field's rule and every
/// write surface accepted a key the renderer would later refuse. The renderer
/// now calls this, so there is one rule and not two — the reason the
/// alternative, a second copy in apid, was refused outright: a second copy can
/// disagree with the first.
///
/// The bound is IEEE 802.11i's and it is checked for the reason the access
/// point checks it: wpa_supplicant rejects an out-of-range passphrase by
/// refusing the WHOLE configuration file, which takes every other configured
/// network down with it while the reconcile still reports `applied`.
///
/// A [`RAW_PMK_LEN`]-digit hex string is the raw 256-bit key rather than a
/// passphrase, and the passphrase bounds do not apply to it.
///
/// **The message deliberately does not name the length observed.** A length is
/// a fact about a secret, and this string reaches an HTTP client.
///
/// It is not enforced in [`WifiNetwork`]'s `Deserialize`, deliberately.
/// [`Settings::set`] validates by deserializing the whole candidate tree and
/// [`crate::Store`] loads by deserializing it, so a bound enforced there would
/// turn one out-of-range key already on disk into a device whose settings file
/// does not load and whose every unrelated write fails. This is a rule about a
/// value being written, and it is checked where a write is decided.
///
/// A passphrase must also be one the station renderer can carry, which is
/// [`is_wpa_quotable`]. That half was measured missing here by
/// A key carrying a quote or a
/// backslash passed this function, was stored, and was then refused at render
/// time with the error visible only in live state.
///
/// # Errors
///
/// Returns the sentence a refusal carries when `psk` is neither a raw PMK nor
/// a passphrase of an admissible length that the renderer can quote.
pub fn validate_wifi_psk(psk: &str) -> Result<(), String> {
    if psk.len() == RAW_PMK_LEN && psk.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(());
    }
    if psk.len() < MIN_PASSPHRASE_LEN || psk.len() > MAX_PASSPHRASE_LEN {
        return Err(format!(
            "a WPA2 passphrase is {MIN_PASSPHRASE_LEN} to {MAX_PASSPHRASE_LEN} characters \
             (or a {RAW_PMK_LEN}-digit hex PMK)"
        ));
    }
    // A passphrase has no hex form -- bare hex means a raw PMK, not a
    // passphrase -- so the renderer has nothing to fall back to and refuses.
    // Refusing here instead means a key the renderer cannot carry is never
    // accepted, rather than stored and dead. The sentence names neither the
    // value nor its length, for the reason the length bound's does not.
    if !is_wpa_quotable(psk) {
        return Err(
            "the pre-shared key contains a character wpa_supplicant configuration \
             cannot carry; use printable ASCII without a quote or a backslash"
                .to_string(),
        );
    }
    Ok(())
}

/// WiFi access-point configuration used by the provisioning flow.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WifiApSettings {
    /// When the access point runs.
    pub mode: ApMode,
    /// Interface the access point runs on.
    pub interface: String,
    /// Advertised SSID; absent means derive it from the device identity at
    /// render time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    /// Pre-shared key; absent means derive it from the device credential at
    /// render time. A fleet-wide constant default is forbidden
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub psk: Option<String>,
    /// 2.4 GHz channel the access point uses.
    pub channel: u8,
    /// Regulatory domain the radio is configured for.
    #[serde(rename = "countryCode")]
    pub country_code: String,
    /// AP-side address in CIDR notation.
    pub address: String,
    /// Seconds without a usable uplink before the access point starts.
    #[serde(rename = "holdDownSeconds")]
    pub hold_down_seconds: u32,
    /// Seconds the access point stays up after an uplink is restored.
    #[serde(rename = "graceSeconds")]
    pub grace_seconds: u32,
}

impl Default for WifiApSettings {
    fn default() -> Self {
        Self {
            mode: ApMode::Off,
            interface: "wlan0".to_string(),
            ssid: None,
            psk: None,
            channel: 6,
            country_code: "US".to_string(),
            address: "192.168.4.1/24".to_string(),
            hold_down_seconds: 120,
            grace_seconds: 60,
        }
    }
}

/// When the WiFi access point runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApMode {
    /// Never.
    #[default]
    Off,
    /// Only while no usable uplink exists.
    Provisioning,
    /// Always, regardless of the uplink.
    Always,
}

/// What kind of link a `network` entry describes.
///
/// Absent means [`IfaceKind::Physical`], and a physical entry never serializes
/// the field: a v6 tree of physical interfaces and its v7 form differ by the
/// schema version integer alone, which is what makes the v6 -> v7 bump
/// additive and the A/B rollback survivable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IfaceKind {
    /// A NIC the kernel already has.
    #[default]
    Physical,
    /// An 802.1Q VLAN on top of another declared entry.
    Vlan,
    /// A software bridge over other declared entries.
    Bridge,
    /// A WireGuard tunnel.
    Wireguard,
}

impl IfaceKind {
    /// Whether this is the default kind, the one that is never written out.
    fn is_physical(&self) -> bool {
        matches!(self, Self::Physical)
    }
}

/// Network configuration for a single interface.
///
/// `kind` selects which of the three optional blocks is meaningful; the block
/// is authoritative, not the interface name (`eth0.100` is a convention, not a
/// declaration). Cross-field consistency — that `kind = "vlan"` carries a
/// `vlan` block and no other, that a bridge port declares no addressing of its
/// own, that a `parent` or a `port` names a declared entry — is enforced in the
/// network reconciler, where the security boundary for a file anything with
/// STATE write access can edit already sits.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IfaceSettings {
    /// What kind of link this is; absent means physical.
    #[serde(default, skip_serializing_if = "IfaceKind::is_physical")]
    pub kind: IfaceKind,
    /// Whether the interface acquires its address via DHCP.
    pub dhcp: bool,
    /// Static addressing, used when `dhcp` is false.
    #[serde(rename = "static", default, skip_serializing_if = "Option::is_none")]
    pub static_: Option<StaticConfig>,
    /// VLAN parameters, for `kind = "vlan"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlan: Option<VlanConfig>,
    /// Bridge parameters, for `kind = "bridge"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<BridgeConfig>,
    /// WireGuard parameters, for `kind = "wireguard"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wireguard: Option<WireguardConfig>,
}

/// The 802.1Q parameters of a VLAN interface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VlanConfig {
    /// Name of the `network` entry this VLAN sits on.
    pub parent: String,
    /// 802.1Q VLAN id.
    pub id: u16,
}

/// The parameters of a software bridge.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    /// Names of the `network` entries enslaved to this bridge.
    #[serde(default)]
    pub ports: Vec<String>,
}

/// The parameters of a WireGuard tunnel.
///
/// There is no private-key field here and there never will be: this subtree is
/// served over the bus and over `GET /api/v1/settings/...`, so a key in it is a
/// key published to every client. The private key lives in a mode-0640 file on
/// STATE and only its public half is ever surfaced. Peer pre-shared keys are
/// out of this schema revision for the same reason — shipping no secret field
/// beats shipping one more redaction obligation.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireguardConfig {
    /// UDP port to listen on. Absent lets the kernel pick one, which is what a
    /// client that only ever initiates wants.
    #[serde(
        rename = "listenPort",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub listen_port: Option<u16>,
    /// The far ends of the tunnel.
    #[serde(default)]
    pub peers: Vec<WireguardPeer>,
}

/// One far end of a WireGuard tunnel.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireguardPeer {
    /// The peer's base64 X25519 public key.
    #[serde(rename = "publicKey")]
    pub public_key: String,
    /// CIDRs routed to this peer.
    #[serde(rename = "allowedIps", default)]
    pub allowed_ips: Vec<String>,
    /// `host:port` to send to, for a peer this end initiates to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Keepalive interval in seconds, for a peer behind NAT.
    #[serde(
        rename = "persistentKeepalive",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub persistent_keepalive: Option<u16>,
}

/// Linux `IFNAMSIZ` minus the terminator: the longest name an interface can
/// actually have.
const MAX_IFACE_NAME_LEN: usize = 15;

/// Refuse a `network` map key the kernel could not name an interface.
///
/// The charset is the network reconciler's own
/// (`mosd/src/reconciler/network.rs`, the security boundary for a settings
/// file anything with STATE write access can edit); repeating it here makes a
/// key the renderer would refuse unwritable through the tree in the first
/// place, and makes a key carrying `"` — the one key the path syntax cannot
/// spell — structurally impossible rather than merely unaddressable.
fn validate_network_key(iface: &str) -> Result<(), String> {
    if iface.is_empty() {
        return Err("network interface name is empty".to_string());
    }
    if iface.len() > MAX_IFACE_NAME_LEN {
        return Err(format!(
            "network interface {iface:?} is longer than {MAX_IFACE_NAME_LEN} characters"
        ));
    }
    if iface == "." || iface == ".." {
        return Err(format!("network interface {iface:?} is not a name"));
    }
    if !iface
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
    {
        return Err(format!(
            "network interface {iface:?} contains a character an interface name cannot have"
        ));
    }
    Ok(())
}

/// Static addressing for a single interface.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticConfig {
    /// Interface address in CIDR notation, e.g. `"192.168.1.10/24"`.
    pub address: String,
    /// Default gateway address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// DNS server addresses.
    #[serde(default)]
    pub dns: Vec<String>,
}

impl Settings {
    /// Read the node at `path` as JSON. `""` or `"."` return the whole tree.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::NotFound`] when the path does not resolve.
    pub fn get(&self, path: &str) -> Result<Value, SettingsError> {
        let root = self.to_json()?;
        json_path_get(&root, path)
            .cloned()
            .ok_or_else(|| SettingsError::NotFound(path.to_string()))
    }

    /// Write `value` at `path`. `""` or `"."` replace the whole tree.
    ///
    /// Missing intermediate map entries are created (e.g. setting
    /// `network.eth1.dhcp` creates `eth1`), provided the resulting tree still
    /// deserializes into a valid [`Settings`]. On any error the settings are
    /// left unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::ReadOnly`] for writes that would change
    /// `schema_version`, [`SettingsError::NotFound`] for malformed paths, and
    /// [`SettingsError::Validation`] when the value does not fit the tree or
    /// the write introduces a `network` key that is not an interface name.
    pub fn set(&mut self, path: &str, value: Value) -> Result<(), SettingsError> {
        let mut root = self.to_json()?;
        if path.is_empty() || path == "." {
            root = value;
        } else {
            let segments = split_path(path)?;
            if segments[0] == "schema_version" {
                return Err(SettingsError::ReadOnly(path.to_string()));
            }
            json_path_set(&mut root, &segments, value)?;
        }
        let candidate: Self =
            serde_json::from_value(root).map_err(|err| SettingsError::Validation {
                path: path.to_string(),
                message: err.to_string(),
            })?;
        if candidate.schema_version != self.schema_version {
            return Err(SettingsError::ReadOnly("schema_version".to_string()));
        }
        // Key-charset validation is a property of the write, not of the tree:
        // a document that already loads keeps loading, so an entry this write
        // does not touch is left alone even if a hand edit spelled it badly.
        for (iface, settings) in &candidate.network {
            if self.network.get(iface) == Some(settings) {
                continue;
            }
            validate_network_key(iface).map_err(|message| SettingsError::Validation {
                path: path.to_string(),
                message,
            })?;
        }
        // Same rule as the network keys: a property of the write, not of the
        // tree. A document that already loads keeps loading; only a write that
        // CHANGES the `time` subtree has to satisfy its predicates.
        if candidate.time != self.time {
            validate_time_settings(&candidate.time).map_err(|message| {
                SettingsError::Validation {
                    path: path.to_string(),
                    message,
                }
            })?;
        }
        *self = candidate;
        Ok(())
    }

    fn to_json(&self) -> Result<Value, SettingsError> {
        serde_json::to_value(self).map_err(|err| SettingsError::Parse(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrips_via_toml() {
        let settings = Settings::default();
        let text = toml::to_string(&settings).unwrap();
        let parsed: Settings = toml::from_str(&text).unwrap();
        assert_eq!(parsed, settings);
        assert_eq!(parsed.schema_version, SCHEMA_VERSION);
        assert_eq!(parsed.hostname, "mos");
        assert!(parsed.network.is_empty());
        assert!(parsed.access.web_admin.is_none());
        assert_eq!(parsed.access.ssh, SshSettings::default());
        assert_eq!(parsed.access.console, ConsoleSettings::default());
        assert_eq!(parsed.access.device, DeviceCredentialSettings::default());
        assert!(parsed.access.api_tokens.is_empty());
        assert_eq!(parsed.provisioning, ProvisioningSettings::default());
        assert_eq!(parsed.wifi, WifiSettings::default());
        assert_eq!(parsed.container, ContainerSettings::default());
        assert_eq!(parsed.mqtt, MqttSettings::default());
        assert_eq!(parsed.time, TimeSettings::default());
    }

    /// The time defaults, spelled out: no managed servers (the image fallback
    /// pool applies) and the UTC presentation zone the contract starts from.
    #[test]
    fn time_defaults_are_no_servers_and_utc() {
        let settings = Settings::default();
        assert!(settings.time.ntp.servers.is_empty());
        assert_eq!(settings.time.timezone, "UTC");

        // There is deliberately no switch to find here: timesyncd is an
        // always-running base service, and a field named like one appearing
        // in this subtree is the regression this pins against.
        let time = toml::to_string(&settings.time).unwrap();
        assert!(!time.contains("enabled"), "{time}");
        assert!(!time.contains("Poll"), "{time}");
    }

    /// The renderer writes `NTP=` space-separated into an ini drop-in, so
    /// everything that could end the assignment or smuggle another is refused
    /// at the write surface rather than stored and dead at render time.
    #[test]
    fn an_ntp_server_the_renderer_cannot_carry_is_refused() {
        for server in [
            "",
            "pool one.example",
            "pool\tone",
            "two\nlines",
            "a=b",
            "#comment",
            "host_name.example",
            "höst.example",
        ] {
            assert!(
                validate_ntp_servers(&[server.to_string()]).is_err(),
                "{server:?} must be refused"
            );
        }

        assert!(
            validate_ntp_servers(&[
                "0.debian.pool.ntp.org".to_string(),
                "time.example-corp.com".to_string(),
                "192.0.2.7".to_string(),
                "2001:db8::123".to_string(),
            ])
            .is_ok()
        );
    }

    #[test]
    fn the_ntp_server_list_is_bounded_and_duplicate_free() {
        let too_many: Vec<String> = (0..=MAX_NTP_SERVERS)
            .map(|index| format!("ntp{index}.example"))
            .collect();
        let err = validate_ntp_servers(&too_many).unwrap_err();
        assert!(err.contains(&MAX_NTP_SERVERS.to_string()), "{err}");

        let twice = vec!["ntp.example".to_string(), "ntp.example".to_string()];
        let err = validate_ntp_servers(&twice).unwrap_err();
        assert!(err.contains("twice"), "{err}");

        let long = "a".repeat(254);
        assert!(validate_ntp_servers(&[long]).is_err());
    }

    /// Deterministic on every host: the rule is the tzdata name grammar and
    /// never a lookup against the machine's own zoneinfo tree.
    #[test]
    fn a_timezone_is_validated_by_grammar_not_by_the_host_tzdata() {
        for zone in [
            "UTC",
            "Etc/GMT+8",
            "Europe/Berlin",
            "America/Argentina/Buenos_Aires",
            "America/Port-au-Prince",
            // Grammatically fine and almost certainly not a real zone: the
            // existence check belongs to reconcile time, not to this rule.
            "Atlantis/Made_Up",
        ] {
            assert!(validate_timezone_name(zone).is_ok(), "{zone:?}");
        }
        for zone in [
            "",
            "/Etc/UTC",
            "Etc/",
            "Etc//UTC",
            "../etc/shadow",
            "Europe/..",
            "Europe/Ber lin",
            "Europe/Berlin\n",
            "Europe/Bërlin",
            &"Z/".repeat(40),
        ] {
            assert!(validate_timezone_name(zone).is_err(), "{zone:?}");
        }
    }

    /// The write surface enforces the two `time` predicates through the tree
    /// itself, so no caller of `Settings::set` can store what the reconciler
    /// cannot render — and an unrelated write leaves a hand-edited `time`
    /// subtree alone, the same property the network keys have.
    #[test]
    fn a_time_write_is_validated_and_an_unrelated_write_is_not() {
        let mut settings = Settings::default();
        settings
            .set("time.timezone", Value::from("Europe/Berlin"))
            .unwrap();
        assert_eq!(settings.time.timezone, "Europe/Berlin");

        let err = settings
            .set("time.timezone", Value::from("Europe/Ber lin"))
            .unwrap_err();
        assert!(matches!(err, SettingsError::Validation { .. }), "{err:?}");
        assert_eq!(settings.time.timezone, "Europe/Berlin");

        settings
            .set(
                "time.ntp.servers",
                serde_json::json!(["0.pool.ntp.org", "192.0.2.7"]),
            )
            .unwrap();
        assert_eq!(settings.time.ntp.servers.len(), 2);
        let err = settings
            .set("time.ntp.servers", serde_json::json!(["bad server"]))
            .unwrap_err();
        assert!(matches!(err, SettingsError::Validation { .. }), "{err:?}");
        assert_eq!(settings.time.ntp.servers.len(), 2);

        // An unrelated write over a tree whose `time` subtree would no longer
        // validate must still land: the rule is about the write, not the tree.
        let mut hand_edited: Settings = settings.clone();
        hand_edited.time.timezone = "not a zone!".to_string();
        hand_edited.set("hostname", Value::from("edge-42")).unwrap();
        assert_eq!(hand_edited.hostname, "edge-42");
        assert_eq!(hand_edited.time.timezone, "not a zone!");
    }

    /// The MQTT defaults, spelled out: off, loopback, and no auth. The switch
    /// is what an operator turns on; the listener is what the broker is
    /// rendered from, and neither constrains the other.
    #[test]
    fn mqtt_defaults_are_off_and_loopback() {
        let settings = Settings::default();
        assert!(!settings.mqtt.enabled);
        assert_eq!(settings.mqtt.listen.address, "127.0.0.1");
        assert_eq!(settings.mqtt.listen.port, 1883);
        assert!(!settings.mqtt.auth.enabled);

        // No credential field exists in this management subtree; the broker's
        // accounts live in a mode-restricted STATE file instead.
        let mqtt = toml::to_string(&settings.mqtt).unwrap();
        assert!(!mqtt.contains("password"), "{mqtt}");
        assert!(!mqtt.contains("username"), "{mqtt}");
    }

    #[test]
    fn no_secret_is_present_in_a_freshly_built_tree() {
        // A non-None default here would be a fleet-wide shared secret baked
        // into a byte-identical signed rootfs.
        let settings = Settings::default();
        assert_eq!(settings.access.device.password_hash, None);
        assert_eq!(settings.access.device.generation, 0);
        assert_eq!(settings.access.web_admin, None);
        assert!(settings.access.api_tokens.is_empty());
        assert_eq!(settings.wifi.ap.psk, None);
        assert_eq!(settings.wifi.ap.ssid, None);
        assert!(settings.wifi.client.networks.is_empty());
        assert_eq!(settings.provisioning.device_id, None);

        let text = toml::to_string(&settings).unwrap();
        assert!(
            !text.contains("psk"),
            "serialized tree must hold no key: {text}"
        );
        assert!(
            !text.contains("passwordHash"),
            "serialized tree must hold no credential: {text}"
        );
        assert!(
            !text.contains("apiTokens"),
            "serialized tree must hold no credential: {text}"
        );
    }

    /// The empty list is not written out, and that is what makes the v7 -> v8
    /// bump additive: a device that never minted a token has a v8 document
    /// whose only difference from its v7 form is the version integer.
    #[test]
    fn an_empty_token_list_is_not_serialized() {
        let text = toml::to_string(&Settings::default()).unwrap();
        assert!(!text.contains("apiTokens"), "{text}");

        let mut with_token = Settings::default();
        with_token.access.api_tokens.push(sample_token());
        let text = toml::to_string(&with_token).unwrap();
        assert!(text.contains("[[access.apiTokens]]"), "{text}");
    }

    /// The wire names are the tree's camelCase convention, and the entry
    /// carries the four fields section 3.2 names and no fifth.
    #[test]
    fn a_token_round_trips_through_toml_under_its_camel_case_name() {
        let mut settings = Settings::default();
        settings.access.api_tokens.push(sample_token());

        let text = toml::to_string(&settings).unwrap();
        let parsed: Settings = toml::from_str(&text).unwrap();
        assert_eq!(parsed, settings);

        // Through the dot-path API the JSON shape is the same one apid reads
        // out of `GetSettings("access")`.
        let value = settings.get("access.apiTokens").unwrap();
        let entry = &value.as_array().unwrap()[0];
        let fields: Vec<&str> = entry
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(fields, ["created", "hash", "id", "name"]);
        assert_eq!(entry["id"], Value::String("3f2a9c41".to_string()));
        assert_eq!(entry["created"], Value::from(1_700_000_000_u64));
    }

    /// The path syntax has no array indexing, so mint and revoke are
    /// read-modify-write of the whole list. This pins that the whole-array
    /// write works and that the indexed one does not silently appear to.
    #[test]
    fn the_token_list_is_written_whole_and_not_by_index() {
        let mut settings = Settings::default();
        let one = serde_json::to_value([sample_token()]).unwrap();
        settings.set("access.apiTokens", one).unwrap();
        assert_eq!(settings.access.api_tokens.len(), 1);

        // An index is not a path segment; a write through one must not land.
        assert!(
            settings
                .set("access.apiTokens.0.name", Value::from("x"))
                .is_err()
        );
        assert_eq!(settings.access.api_tokens[0].name, "ci-deploy");

        settings
            .set("access.apiTokens", Value::Array(Vec::new()))
            .unwrap();
        assert!(settings.access.api_tokens.is_empty());
    }

    /// A passphrase the station renderer cannot carry is refused here, so it
    /// cannot be accepted at a write surface and then die at render time.
    ///
    /// The predicate is the renderer's own — printable ASCII, minus the quote
    /// that would close the wpa_supplicant string and the backslash some of
    /// its string forms treat as an escape — and it is stated once, in
    /// [`is_wpa_quotable`], which the renderer calls.
    #[test]
    fn a_passphrase_the_renderer_cannot_quote_is_refused() {
        for psk in [
            "has\"quote1",
            "has\\backslash",
            "two\nlines1",
            "tab\there1",
            "cafe\u{301}-latte",
            "caf\u{e9}-latte",
        ] {
            let Err(err) = validate_wifi_psk(psk) else {
                panic!("{psk:?} must be refused");
            };
            assert!(err.contains("pre-shared key"), "{psk:?}: {err}");
            // A refusal never echoes the value; a key is a secret and this
            // sentence reaches an HTTP client.
            assert!(!err.contains(psk), "the refusal echoed the key: {err}");
        }

        // Everything IEEE 802.11i's own passphrase alphabet allows and the
        // renderer can quote still passes, and so does a raw PMK.
        assert!(validate_wifi_psk("hunter2hunter2").is_ok());
        assert!(validate_wifi_psk("p@ssw0rd!#$%^&*()_+-=[]{};:',.<>/? ~`").is_ok());
        assert!(validate_wifi_psk(&"a".repeat(RAW_PMK_LEN)).is_ok());
    }

    /// One well-formed entry, spelled the way section 3.2 spells it.
    fn sample_token() -> ApiToken {
        ApiToken {
            id: "3f2a9c41".to_string(),
            name: "ci-deploy".to_string(),
            hash: "9".repeat(64),
            created: 1_700_000_000,
        }
    }
}

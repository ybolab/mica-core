//! MQTT reconciler: one master switch driving the broker and the bridge that
//! needs it, and the broker's runtime config rendered from `mqtt`.
//!
//! `/run/mica/mqtt-broker.toml` is rendered from `mqtt.listen` and `mqtt.auth`
//! (the three keys the `mica-mqtt-broker` binary parses), and
//! `/run/mica/mqttd-device.env` carries only the validated device identity.
//! Both are rendered before
//! `mica-mqtt-broker.service` and `mica-mqttd.service` are brought to the state
//! `mqtt.enabled` asks for. Config before service start, as in `sshd.rs`: a
//! broker on a stale config listens on the wrong address, and the unit's state
//! does not show it. The render is unconditional and on `/run`, so the file
//! always describes what the switch would start, at no flash cost.
//! `mqtt.enabled` is a master switch and nothing else (see
//! [`micad_settings::MqttSettings`]): false means neither unit runs, and no
//! `listen`/`auth` combination changes that. Order reverses between up and down
//! -- broker then bridge starting, the bridge being the broker's client; bridge
//! then broker stopping, or the client logs failures about a deliberate stop.

use std::path::PathBuf;

use anyhow::{Context, Result};
use micad_settings::{MqttSettings, Settings};
use serde_json::json;

use super::Reconciler;
use super::systemd::{Systemd, UnitControl, is_active, is_enabled};
use crate::fswrite::write_config;

/// Unit implementing the MQTT broker (rumqttd, used as a library).
const BROKER_UNIT: &str = "mica-mqtt-broker.service";
/// Unit implementing the bridge from the broker to the cloud. Predates this
/// reconciler; unchanged by it apart from who starts and stops it.
const BRIDGE_UNIT: &str = "mica-mqttd.service";
/// Config the broker binary reads, rendered by micad at runtime.
///
/// On `/run`, not on STATE: it is derived entirely from the settings tree and
/// is re-rendered on every boot before the broker starts, so persisting it
/// would only create a second copy of the truth that could disagree with the
/// first.
const DEFAULT_CONFIG_PATH: &str = "/run/mica/mqtt-broker.toml";
/// Root-rendered runtime identity read by `mica-mqttd.service`.
const IDENTITY_FILE_NAME: &str = "mqttd-device.env";
/// Fixed production path required by `mica-mqttd.service`.
const DEFAULT_IDENTITY_PATH: &str = "/run/mica/mqttd-device.env";
/// Environment variable overriding the rendered config path.
///
/// Nothing in the image sets it; the override exists so tests run entirely
/// inside a temporary directory and never touch the host's `/run`.
const CONFIG_PATH_ENV: &str = "MICAD_MQTT_BROKER_CONFIG";
/// Mode of the rendered config: world-readable, owner-writable.
///
/// The broker runs as the unprivileged `mica-mqtt-broker` account and micad
/// writes this file as root, so it has to be readable by somebody other than
/// its owner. It carries no secret — credentials live in the STATE-backed
/// `/var/lib/mica/mqtt-broker-users.toml`, precisely so that this file does
/// not need to be protected.
const CONFIG_MODE: u32 = 0o644;
/// `ActiveState` of a unit systemd has given up on.
///
/// Worth naming because it is not merely "down": it is the state in which a
/// unit that has exhausted its start limit REFUSES further start jobs, which
/// is what [`MqttReconciler::turn_broker_on`] has to clear before it can start
/// anything. `systemd::is_active` deliberately does not distinguish it from
/// `inactive` -- for "should I start this?" they are the same answer, and only
/// this reconciler needs the difference.
const FAILED_STATE: &str = "failed";

/// Reconciler for the `mqtt` settings subtree.
pub struct MqttReconciler<C: UnitControl> {
    /// Path the broker config is rendered to.
    config_path: PathBuf,
    /// One-purpose runtime identity file beside the broker configuration.
    identity_path: PathBuf,
    control: C,
}

impl<C: UnitControl> MqttReconciler<C> {
    /// Create an MQTT reconciler rendering the broker config to `config_path`
    /// and driving both units through `control`.
    ///
    /// The path is a parameter so tests run entirely inside a temporary
    /// directory and never touch the host's `/run`.
    pub fn new(config_path: PathBuf, control: C) -> Self {
        let identity_path = config_path
            .parent()
            .map(|parent| parent.join(IDENTITY_FILE_NAME))
            .unwrap_or_else(|| PathBuf::from(IDENTITY_FILE_NAME));
        Self {
            config_path,
            identity_path,
            control,
        }
    }
}

impl MqttReconciler<Systemd> {
    /// Production reconciler: config path from [`CONFIG_PATH_ENV`] if set,
    /// else [`DEFAULT_CONFIG_PATH`].
    pub fn production() -> Self {
        let config_path = std::env::var(CONFIG_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH));
        let mut reconciler = Self::new(config_path, Systemd::new());
        // The broker-config override exists for tests and diagnostics, but the
        // systemd unit intentionally names one fixed identity path. Never let
        // an unrelated broker path move this security boundary.
        reconciler.identity_path = PathBuf::from(DEFAULT_IDENTITY_PATH);
        reconciler
    }
}

/// How `mqtt.listen.address` reads to the broker.
///
/// Three states rather than a single "is it loopback" predicate, because the
/// two things worth warning about are different things and their messages
/// contradict each other. An address that does not parse is not a wide bind —
/// it is not a bind at all, and telling the operator it "accepts connections
/// from the network" would be false. Matching on this makes the two warnings
/// in [`MqttReconciler::apply`] mutually exclusive by construction rather than
/// by the order two `if`s happen to be written in.
#[derive(Debug, PartialEq, Eq)]
enum ListenAddress {
    /// Parses as an `IpAddr` and is loopback: reachable only from the device.
    Loopback,
    /// Parses as an `IpAddr` and is not loopback: reachable from the network.
    OffHost,
    /// Does not parse as an `IpAddr`.
    ///
    /// `mica-mqtt-broker` parses `listen_address` as an `IpAddr` and does not
    /// resolve names, so a value like `"localhost"` is a startup error and the
    /// process exits. Nothing here refuses anything for it — see
    /// [`MqttReconciler::apply`].
    Unparseable,
}

/// Classify `address` for the warnings in [`MqttReconciler::apply`].
///
/// Pure: it decides what to say, never whether to act.
fn classify_listen_address(address: &str) -> ListenAddress {
    match address.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_loopback() => ListenAddress::Loopback,
        Ok(_) => ListenAddress::OffHost,
        Err(_) => ListenAddress::Unparseable,
    }
}

/// Render the broker config for `mqtt`.
///
/// Pure and deterministic: the same settings always produce the same bytes, so
/// a re-render compared against what is on disk tells [`MqttReconciler::apply`]
/// whether a running broker has to be restarted.
///
/// Three keys and no more, all always present: the broker's parser requires
/// exactly `listen_address`, `listen_port` and `auth_enabled`. `mqtt.enabled`
/// is deliberately absent, deciding whether the broker runs, which the process
/// cannot act on once started. The address is written verbatim -- no default
/// substituted, nothing corrected, not even a value that cannot parse as an
/// `IpAddr` -- because a config file disagreeing with the settings tree is
/// worse than one the broker rejects loudly. An unparseable address gets a
/// WARN from `apply` and a failed unit, both of which name it.
fn render_config(mqtt: &MqttSettings) -> String {
    let mut out = String::new();
    out.push_str(&format!("listen_address = \"{}\"\n", mqtt.listen.address));
    out.push_str(&format!("listen_port = {}\n", mqtt.listen.port));
    out.push_str(&format!("auth_enabled = {}\n", mqtt.auth.enabled));
    out
}

/// Render the one environment assignment consumed by `mica-mqttd.service`.
///
/// Device identities are generated as lowercase hex, but accepting the wider
/// identifier alphabet keeps existing provisioned devices compatible. Shell,
/// systemd-environment and MQTT metacharacters are rejected so the value can
/// be written without quoting or escaping and still remain exactly one topic
/// segment.
fn render_device_identity(device_id: Option<&str>) -> Result<String> {
    let device_id = device_id.context("mqtt: device identity is not provisioned")?;
    if device_id.is_empty()
        || !device_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        anyhow::bail!(
            "mqtt: device identity must contain only ASCII letters, digits, '.', '_', '-' or ':'"
        );
    }
    Ok(format!("MICA_MQTT_DEVICE_ID={device_id}\n"))
}

impl<C: UnitControl> MqttReconciler<C> {
    /// Render the broker config and report whether its bytes changed.
    ///
    /// An unchanged render is not rewritten. The file is on `/run` so the
    /// write itself is cheap, but "did the bytes change" is the signal that
    /// decides whether a running broker is restarted, and re-deriving it from
    /// the file's mtime rather than its contents would restart the broker on
    /// every reconcile.
    fn apply_config(&self, mqtt: &MqttSettings) -> Result<bool> {
        let rendered = render_config(mqtt);
        if let Ok(current) = std::fs::read_to_string(&self.config_path)
            && current == rendered
        {
            return Ok(false);
        }
        if let Some(directory) = self.config_path.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }
        write_config(&self.config_path, &rendered, CONFIG_MODE)
            .with_context(|| format!("render {}", self.config_path.display()))?;
        Ok(true)
    }

    /// Render the bridge identity and report whether its bytes changed.
    fn apply_identity(&self, settings: &Settings) -> Result<bool> {
        let rendered = render_device_identity(settings.provisioning.device_id.as_deref())?;
        if let Ok(current) = std::fs::read_to_string(&self.identity_path)
            && current == rendered
        {
            return Ok(false);
        }
        if let Some(directory) = self.identity_path.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }
        write_config(&self.identity_path, &rendered, CONFIG_MODE)
            .with_context(|| format!("render {}", self.identity_path.display()))?;
        Ok(true)
    }

    /// Start `unit`, and on failure WARN instead of failing the reconcile.
    ///
    /// `apply` covers the whole `mqtt` subtree, so propagating a start failure
    /// would couple `mqtt.enabled` to `mqtt.listen` being valid -- the coupling
    /// this reconciler does not have (see [`micad_settings::MqttSettings`]).
    ///
    /// The refusal is reachable: `mica-mqtt-broker.service` carries
    /// `StartLimitIntervalSec=60` / `StartLimitBurst=5`, so a persistent failure
    /// such as an unparseable `mqtt.listen.address` exhausts five attempts in
    /// about 25 seconds and systemd rejects every start job for the rest of that
    /// minute; reconcilers run together, so an unrelated write would fail on a
    /// broker in cool-off. The WARN names the unit and the error, and the live
    /// state publishes each unit's `activeState`, which the apid pane renders
    /// pointing at `journalctl -u mica-mqtt-broker`. Start only: the stop path
    /// keeps propagating errors, `mqtt.enabled = false` that left a broker
    /// listening being worth failing on.
    async fn start_or_warn(&self, unit: &str) {
        if let Err(error) = self.control.start(unit).await {
            tracing::warn!(
                unit,
                %error,
                "mqtt: the unit refused to start; mqtt.enabled stays applied and the unit's real \
                 state is published as it is"
            );
        }
    }

    /// Clear `unit`'s failed state so the start after it is not refused.
    ///
    /// Warns rather than propagating, for the same reason
    /// [`MqttReconciler::start_or_warn`] does: this call exists only on the
    /// way up, and a bus failure here would fail the reconcile of
    /// `mqtt.enabled` over a broker that is already broken. Continuing costs
    /// nothing -- the start below is attempted either way, and if it is
    /// refused it warns in its own right.
    async fn reset_failed_or_warn(&self, unit: &str) {
        if let Err(error) = self.control.reset_failed(unit).await {
            tracing::warn!(
                unit,
                %error,
                "mqtt: could not clear the unit's failed state; a start may be refused until its \
                 start-limit window elapses"
            );
        }
    }

    /// Bring the broker up, restarting it when the config it is running
    /// against has been rewritten.
    ///
    /// `restart`, not `reload`. The broker is rumqttd used as a library and its
    /// unit carries no `ExecReload`, so a reload would fail on every config
    /// change and leave the broker on the old address. A broker restart drops
    /// MQTT sessions and the bridge reconnects: a cost this unit can pay.
    ///
    /// Reads before it writes, so a broker already in the target state and
    /// running against the current config gets no calls at all.
    async fn turn_broker_on(&self, config_changed: bool) -> Result<()> {
        if !is_enabled(&self.control.unit_file_state(BROKER_UNIT).await?) {
            self.control.enable(BROKER_UNIT).await?;
        }
        let active_state = self.control.active_state(BROKER_UNIT).await?;
        if is_active(&active_state) {
            if config_changed {
                self.control.restart(BROKER_UNIT).await?;
            }
        } else {
            // A failed broker may be inside its start-limit window, where
            // systemd refuses start jobs outright. Clear the failure first, so
            // an operator who has just corrected the address gets a broker that
            // comes up on this apply rather than one that stays down until the
            // minute expires.
            //
            // Guarded on the state rather than issued unconditionally: it would
            // be harmless -- `reset-failed` on a healthy unit does nothing --
            // but a call log showing it on every apply stops distinguishing the
            // broker that needed rescuing from the one that did not.
            //
            // Broker only. The bridge carries no StartLimit override, so it
            // inherits systemd's 10-second default, which at its RestartSec=5
            // fits about two attempts and cannot reach the limit at all.
            if active_state == FAILED_STATE {
                self.reset_failed_or_warn(BROKER_UNIT).await;
            }
            // Not running: start it. A restart here would work too, but
            // starting says what is meant, and a broker that is down is not
            // running a stale config — it is running none.
            self.start_or_warn(BROKER_UNIT).await;
        }
        Ok(())
    }

    /// Bring the bridge up, restarting it when its root-rendered identity
    /// changed. Broker configuration changes still need no bridge restart: its
    /// reconnect loop handles the broker transition.
    async fn turn_bridge_on(&self, identity_changed: bool) -> Result<()> {
        if !is_enabled(&self.control.unit_file_state(BRIDGE_UNIT).await?) {
            self.control.enable(BRIDGE_UNIT).await?;
        }
        if is_active(&self.control.active_state(BRIDGE_UNIT).await?) {
            if identity_changed {
                self.control.restart(BRIDGE_UNIT).await?;
            }
        } else {
            self.start_or_warn(BRIDGE_UNIT).await;
        }
        Ok(())
    }

    /// Stop and disable `unit` if it is not already down.
    async fn turn_unit_off(&self, unit: &str) -> Result<()> {
        if is_active(&self.control.active_state(unit).await?) {
            self.control.stop(unit).await?;
        }
        if is_enabled(&self.control.unit_file_state(unit).await?) {
            self.control.disable(unit).await?;
        }
        Ok(())
    }

    /// Live state of `unit`, read after the transition so what is published is
    /// what the system now is rather than what it was asked to become.
    async fn unit_state(&self, unit: &str) -> Result<serde_json::Value> {
        Ok(json!({
            "unit": unit,
            "activeState": self.control.active_state(unit).await?,
            "unitFileState": self.control.unit_file_state(unit).await?,
        }))
    }
}

#[async_trait::async_trait]
impl<C: UnitControl> Reconciler for MqttReconciler<C> {
    fn name(&self) -> &'static str {
        "mqtt"
    }

    fn subtree(&self) -> &'static str {
        "mqtt"
    }

    async fn apply(&self, settings: &Settings) -> Result<serde_json::Value> {
        let mqtt = &settings.mqtt;

        // Unconditionally, and before any unit is touched: the file then
        // describes what the switch would start even while it is off, and a
        // broker is never started against a config older than the settings
        // that were just applied.
        //
        // The ordering is load-bearing in a way that fails SILENTLY if it is
        // ever reversed. The broker unit carries
        // `ConditionPathExists=/run/mica/mqtt-broker.toml`, so with the file
        // absent systemd does not fail the start -- it skips it, and the unit
        // reads as perfectly healthy having never run. Do not move this below
        // the unit calls.
        let config_changed = self.apply_config(mqtt)?;
        // Not `?`: the identity is the bridge's input and nobody else's. An
        // identity that cannot be rendered withholds the bridge below and
        // must not stop the broker, and must never stop the off path -- a
        // switch that cannot turn the units off is worse than a bridge that
        // does not start.
        let identity = self.apply_identity(settings);

        if mqtt.enabled {
            // WARNs, and deliberately not gates. Neither may become a refusal
            // and neither may skip a unit: an operator who widened the bind
            // made a decision, and a daemon that answers by quietly not
            // starting is one whose reason for being down cannot be read
            // anywhere. `listen`/`auth` are deliberately not coupled to the
            // master switch -- see `micad_settings::MqttSettings`.
            //
            // Returning `Err` would be that coupling again: `apply` covers the
            // whole `mqtt` subtree, so an error raised over `listen` fails the
            // reconcile of `mqtt.enabled` itself. Deliberately a different rule
            // from `SshdReconciler`, which does reject an unparseable
            // listen address -- SSH has one key, `access.ssh.enabled`, and no
            // separate switch to protect.
            match classify_listen_address(&mqtt.listen.address) {
                // Not a wide bind -- not a bind at all. The config is rendered
                // verbatim anyway (see `apply_config`), the unit is started
                // anyway, and the broker exits with a parse error naming the
                // file and the value. That lands the unit in `failed`, which
                // the `units` array below reports, so the operator reads the
                // real cause in one place instead of two half-causes.
                ListenAddress::Unparseable => tracing::warn!(
                    address = %mqtt.listen.address,
                    "mqtt: listen address is not an IP address; the broker does not resolve names \
                     and will refuse to start against it"
                ),
                ListenAddress::OffHost if !mqtt.auth.enabled => tracing::warn!(
                    address = %mqtt.listen.address,
                    port = mqtt.listen.port,
                    "mqtt: the broker is bound off-host with authentication disabled; it accepts \
                     unauthenticated connections from the network"
                ),
                ListenAddress::Loopback | ListenAddress::OffHost => {}
            }
            // Broker first: the bridge is its client.
            self.turn_broker_on(config_changed).await?;
            match identity {
                Ok(identity_changed) => self.turn_bridge_on(identity_changed).await?,
                Err(error) => tracing::warn!(
                    %error,
                    "mqtt: the bridge identity could not be rendered; the bridge is not started \
                     and a running one keeps the identity it has"
                ),
            }
        } else {
            // Bridge first, the reverse of start: the client goes before the
            // server it talks to, so a deliberate shutdown does not read as a
            // connection failure in the bridge's journal.
            self.turn_unit_off(BRIDGE_UNIT).await?;
            self.turn_unit_off(BROKER_UNIT).await?;
        }

        // The shape below is a contract with a consumer in another crate:
        // `micad/apid/src/routes.rs` renders the MQTT pane by reading these
        // keys by name out of the bus item this becomes. Nothing in the type
        // system connects the two -- apid talks to micad over the bus -- so
        // `the_published_shape_is_the_contract_with_the_apid_pane` below
        // asserts the exact key set, and changing a key here means changing
        // the pane. Skipping that does not break loudly: the pane renders
        // "unknown", keeps its 200, and the warning it exists to raise
        // silently never fires again.
        Ok(json!({
            "enabled": mqtt.enabled,
            "listen": {
                "address": mqtt.listen.address,
                "port": mqtt.listen.port,
            },
            "auth": {
                // Policy only. There is no credential in the settings tree to
                // publish -- the broker's accounts live on STATE.
                "enabled": mqtt.auth.enabled,
            },
            "configPath": self.config_path.display().to_string(),
            "units": [
                self.unit_state(BROKER_UNIT).await?,
                self.unit_state(BRIDGE_UNIT).await?,
            ],
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use micad_settings::{MqttAuthSettings, MqttListenSettings};

    use super::super::systemd::mock::MockUnitControl;
    use super::*;

    /// What `apply` renders for default `mqtt` settings.
    const GOLDEN_DEFAULTS: &str =
        "listen_address = \"127.0.0.1\"\nlisten_port = 1883\nauth_enabled = false\n";
    const GOLDEN_IDENTITY: &str = "MICA_MQTT_DEVICE_ID=00112233445566778899aabbccddeeff\n";

    fn mqtt_settings(enabled: bool, address: &str, port: u16, auth: bool) -> MqttSettings {
        MqttSettings {
            enabled,
            listen: MqttListenSettings {
                address: address.to_string(),
                port,
            },
            auth: MqttAuthSettings { enabled: auth },
        }
    }

    fn settings_with(mqtt: MqttSettings) -> Settings {
        let mut settings = Settings {
            mqtt,
            ..Settings::default()
        };
        settings.provisioning.device_id = Some("00112233445566778899aabbccddeeff".to_string());
        settings
    }

    /// The two above composed, because every unit test names all four values
    /// and nothing else in the tree.
    fn settings(enabled: bool, address: &str, port: u16, auth: bool) -> Settings {
        settings_with(mqtt_settings(enabled, address, port, auth))
    }

    /// Reconciler rendering into a directory that does not exist yet, so every
    /// test also proves the parent is created, and driving both units through
    /// a mock starting at `active`/`file_state`.
    fn fixture(
        dir: &Path,
        active: &str,
        file_state: &str,
    ) -> (MqttReconciler<MockUnitControl>, PathBuf) {
        let config = dir.join("mica").join("mqtt-broker.toml");
        (
            MqttReconciler::new(config.clone(), MockUnitControl::new(active, file_state)),
            config,
        )
    }

    #[tokio::test]
    async fn disabled_to_enabled_starts_the_broker_before_the_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings(true, "127.0.0.1", 1883, false))
            .await
            .unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable mica-mqtt-broker.service".to_string(),
                "start mica-mqtt-broker.service".to_string(),
                "enable mica-mqttd.service".to_string(),
                "start mica-mqttd.service".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn enabled_to_disabled_stops_the_bridge_before_the_broker() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "active", "enabled");

        reconciler
            .apply(&settings(false, "127.0.0.1", 1883, false))
            .await
            .unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "stop mica-mqttd.service".to_string(),
                "disable mica-mqttd.service".to_string(),
                "stop mica-mqtt-broker.service".to_string(),
                "disable mica-mqtt-broker.service".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn a_second_apply_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "disabled");
        let unchanged = settings(true, "127.0.0.1", 1883, false);

        reconciler.apply(&unchanged).await.unwrap();
        let after_first = reconciler.control.calls();
        reconciler.apply(&unchanged).await.unwrap();

        assert_eq!(
            reconciler.control.calls(),
            after_first,
            "a repeated apply against an unchanged system must issue no calls"
        );
    }

    #[tokio::test]
    async fn a_changed_listen_config_restarts_the_running_broker() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings(true, "127.0.0.1", 1883, false))
            .await
            .unwrap();
        let after_first = reconciler.control.calls().len();
        reconciler
            .apply(&settings(true, "127.0.0.1", 1884, false))
            .await
            .unwrap();

        // Restarted, not reloaded: the unit carries no ExecReload. And only the
        // broker -- the bridge does not read this config.
        assert_eq!(
            reconciler.control.calls()[after_first..],
            ["restart mica-mqtt-broker.service".to_string()]
        );
    }

    #[tokio::test]
    async fn a_changed_device_identity_restarts_only_the_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "disabled");
        let initial = settings(true, "127.0.0.1", 1883, false);

        reconciler.apply(&initial).await.unwrap();
        let after_first = reconciler.control.calls().len();

        let mut changed = initial;
        changed.provisioning.device_id = Some("ffeeddccbbaa99887766554433221100".to_string());
        reconciler.apply(&changed).await.unwrap();

        assert_eq!(
            reconciler.control.calls()[after_first..],
            ["restart mica-mqttd.service".to_string()]
        );
    }

    /// There is deliberately no gate here -- nothing refuses to start a
    /// broker bound off-host with authentication disabled. `mqtt.enabled` is a
    /// master switch and nothing else; `listen` and `auth` are a separate
    /// configuration that nothing may refuse to start on. The warning in
    /// `apply` is the whole of the response. Do not delete this test in order
    /// to add the gate.
    #[tokio::test]
    async fn off_host_without_auth_still_starts() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings(true, "0.0.0.0", 1883, false))
            .await
            .expect("an off-host bind without auth warns; it must never fail");

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable mica-mqtt-broker.service".to_string(),
                "start mica-mqtt-broker.service".to_string(),
                "enable mica-mqttd.service".to_string(),
                "start mica-mqttd.service".to_string(),
            ]
        );
        assert_eq!(state["enabled"], serde_json::json!(true));
    }

    /// The other half of the StartLimit amendment. `mica-mqtt-broker.service`
    /// carries `StartLimitIntervalSec=60` / `StartLimitBurst=5`, and inside
    /// that window systemd REFUSES start jobs on a unit that has exhausted its
    /// burst. Converging by "not active, so start it" therefore issues a start
    /// that cannot succeed, and the operator who has just fixed the address
    /// would have to save again after the minute expired -- with nothing
    /// telling them so.
    #[tokio::test]
    async fn a_failed_broker_is_reset_before_it_is_started() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "enabled");
        reconciler.control.set_active_state(BROKER_UNIT, "failed");

        reconciler
            .apply(&settings(true, "127.0.0.1", 1883, false))
            .await
            .unwrap();

        // ORDER is the assertion: a reset AFTER the start would clear the
        // failure and leave the broker still down.
        assert_eq!(
            reconciler.control.calls(),
            vec![
                "reset-failed mica-mqtt-broker.service".to_string(),
                "start mica-mqtt-broker.service".to_string(),
                "start mica-mqttd.service".to_string(),
            ]
        );
    }

    /// No wasted call on the healthy path, and -- more to the point -- a call
    /// log in which `reset-failed` means something. Issued on every apply it
    /// would stop distinguishing the broker that needed rescuing from the one
    /// that never failed.
    #[tokio::test]
    async fn a_broker_that_is_not_failed_is_not_reset() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings(true, "127.0.0.1", 1883, false))
            .await
            .unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable mica-mqtt-broker.service".to_string(),
                "start mica-mqtt-broker.service".to_string(),
                "enable mica-mqttd.service".to_string(),
                "start mica-mqttd.service".to_string(),
            ],
            "a broker that never failed must not be reset-failed"
        );
    }

    /// A start systemd REFUSES must not fail the reconcile. `apply` covers the
    /// whole `mqtt` subtree, so an `Err` here fails `mqtt.enabled` itself over
    /// a unit in start-limit cool-off -- the same coupling that
    /// `an_address_that_cannot_parse_still_starts_both_units` exists to
    /// forbid, arriving one step later. Worse, reconcilers run together: an
    /// unrelated hostname or WiFi write would fail on a broker in cool-off that
    /// has nothing to do with it. Do not "fix" this into an `Err`.
    #[tokio::test]
    async fn a_refused_broker_start_does_not_fail_the_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "enabled");
        reconciler.control.refuse_start(BROKER_UNIT);

        let state = reconciler
            .apply(&settings(true, "127.0.0.1", 1883, false))
            .await
            .expect("a refused start warns; it must never fail the reconcile");

        // The bridge is still driven: the broker's refusal must not abort the
        // rest of the subtree's convergence either.
        assert_eq!(
            reconciler.control.calls(),
            vec![
                "start mica-mqtt-broker.service".to_string(),
                "start mica-mqttd.service".to_string(),
            ]
        );
        // And the failure is REPORTED rather than hidden. Both units are still
        // in the published state, and the broker's `activeState` is what it
        // really is -- which is what the apid pane renders and points at
        // `journalctl -u mica-mqtt-broker`.
        assert_eq!(
            state["units"],
            json!([
                {
                    "unit": "mica-mqtt-broker.service",
                    "activeState": "inactive",
                    "unitFileState": "enabled",
                },
                {
                    "unit": "mica-mqttd.service",
                    "activeState": "active",
                    "unitFileState": "enabled",
                },
            ])
        );
    }

    /// The bridge gets the same treatment as the broker, for the same reason:
    /// `mqtt.enabled` is one switch over two units, and neither of them being
    /// startable is a reason to fail the switch.
    #[tokio::test]
    async fn a_refused_bridge_start_does_not_fail_the_reconcile_either() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "enabled");
        reconciler.control.refuse_start(BRIDGE_UNIT);

        let state = reconciler
            .apply(&settings(true, "127.0.0.1", 1883, false))
            .await
            .expect("a refused bridge start warns; it must never fail the reconcile");

        assert_eq!(state["enabled"], json!(true));
        assert_eq!(state["units"][0]["activeState"], json!("active"));
        assert_eq!(state["units"][1]["activeState"], json!("inactive"));
    }

    /// Stop and disable keep propagating. A start limit cannot cause them, and
    /// `mqtt.enabled = false` that quietly left a broker listening is a failure
    /// worth failing on -- the leniency above is scoped to the start path and
    /// must not spread.
    #[tokio::test]
    async fn turning_the_switch_off_still_propagates_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "active", "enabled");
        // Refusing `start` is the only injectable failure, and the off path
        // must never reach it: what this asserts is that turning the switch off
        // issues stops and disables and no start at all, so the `?` on those
        // calls is still the code that runs.
        reconciler.control.refuse_start(BROKER_UNIT);
        reconciler.control.refuse_start(BRIDGE_UNIT);

        reconciler
            .apply(&settings(false, "127.0.0.1", 1883, false))
            .await
            .unwrap();

        let calls = reconciler.control.calls();
        assert!(
            calls.iter().all(|call| !call.starts_with("start ")),
            "the off path must issue no start: {calls:?}"
        );
        assert_eq!(
            calls,
            vec![
                "stop mica-mqttd.service".to_string(),
                "disable mica-mqttd.service".to_string(),
                "stop mica-mqtt-broker.service".to_string(),
                "disable mica-mqtt-broker.service".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn apply_writes_the_golden_config_creating_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, config) = fixture(dir.path(), "inactive", "disabled");
        assert!(!config.parent().unwrap().exists());

        reconciler
            .apply(&settings_with(MqttSettings::default()))
            .await
            .unwrap();

        assert!(config.parent().unwrap().is_dir());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), GOLDEN_DEFAULTS);
        assert_eq!(
            std::fs::read_to_string(config.parent().unwrap().join(IDENTITY_FILE_NAME)).unwrap(),
            GOLDEN_IDENTITY
        );
    }

    /// An identity the environment grammar cannot carry withholds the bridge
    /// and nothing else. The broker does not read the identity, so it is
    /// driven as usual, and the reconcile of `mqtt.enabled` succeeds: the
    /// master switch is not coupled to the provisioning subtree any more than
    /// it is to `listen`.
    #[tokio::test]
    async fn an_unsafe_device_identity_withholds_only_the_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "disabled");
        let mut invalid = settings(true, "127.0.0.1", 1883, false);
        invalid.provisioning.device_id = Some("device id$injected".to_string());

        let state = reconciler
            .apply(&invalid)
            .await
            .expect("an unsafe identity warns; it must not fail the reconcile");

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable mica-mqtt-broker.service".to_string(),
                "start mica-mqtt-broker.service".to_string(),
            ],
            "the bridge must not start against an invalid identity; the broker still does"
        );
        assert_eq!(state["units"][1]["activeState"], json!("inactive"));
    }

    /// The off path never depends on the identity render. A device whose
    /// identity fails validation must still be able to stop both units.
    #[tokio::test]
    async fn an_unsafe_device_identity_does_not_block_the_off_path() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "active", "enabled");
        let mut invalid = settings(false, "127.0.0.1", 1883, false);
        invalid.provisioning.device_id = Some("device id$injected".to_string());

        reconciler
            .apply(&invalid)
            .await
            .expect("turning the switch off must not depend on the identity");

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "stop mica-mqttd.service".to_string(),
                "disable mica-mqttd.service".to_string(),
                "stop mica-mqtt-broker.service".to_string(),
                "disable mica-mqtt-broker.service".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn the_config_is_rendered_even_while_the_switch_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, config) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings(false, "10.0.0.5", 8883, true))
            .await
            .unwrap();

        // So turning the switch on does not have to wait for a second
        // reconcile to get a config that matches the settings.
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "listen_address = \"10.0.0.5\"\nlisten_port = 8883\nauth_enabled = true\n"
        );
    }

    #[test]
    fn the_render_is_deterministic() {
        let mqtt = mqtt_settings(true, "10.0.0.5", 8883, true);
        assert_eq!(render_config(&mqtt), render_config(&mqtt));
    }

    #[test]
    fn a_listen_address_is_loopback_off_host_or_not_an_address() {
        use ListenAddress::{Loopback, OffHost, Unparseable};

        assert_eq!(classify_listen_address("127.0.0.1"), Loopback);
        assert_eq!(classify_listen_address("127.0.0.2"), Loopback);
        assert_eq!(classify_listen_address("::1"), Loopback);
        assert_eq!(classify_listen_address("0.0.0.0"), OffHost);
        assert_eq!(classify_listen_address("10.0.0.5"), OffHost);
        assert_eq!(classify_listen_address("::"), OffHost);
        // A name, not an address. The broker does not resolve names, so this
        // is NOT an off-host bind -- it is not a bind at all, and it gets its
        // own warning rather than one claiming the network can reach it.
        assert_eq!(classify_listen_address("localhost"), Unparseable);
        assert_eq!(classify_listen_address(""), Unparseable);
        assert_eq!(classify_listen_address("127.0.0.1:1883"), Unparseable);
    }

    /// An unparseable address must NOT fail the reconcile. `apply` covers the
    /// whole `mqtt` subtree, so an `Err` raised over `listen` would fail the
    /// reconcile of `mqtt.enabled` itself -- making the master switch depend on
    /// `listen` being valid, which is exactly the coupling this reconciler
    /// refuses. The broker is started, exits with its own parse error naming
    /// the file and the value, and lands in `failed` where live state reports
    /// it. Do not "fix" this into an `Err`; that introduces the coupling.
    #[tokio::test]
    async fn an_address_that_cannot_parse_still_starts_both_units() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, config) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings(true, "localhost", 1883, true))
            .await
            .expect("an unparseable listen address warns; it must never fail the reconcile");

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable mica-mqtt-broker.service".to_string(),
                "start mica-mqtt-broker.service".to_string(),
                "enable mica-mqttd.service".to_string(),
                "start mica-mqttd.service".to_string(),
            ]
        );
        // Rendered verbatim: no default substituted, no value corrected.
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "listen_address = \"localhost\"\nlisten_port = 1883\nauth_enabled = true\n"
        );
        assert_eq!(state["listen"]["address"], json!("localhost"));
    }

    /// The keys of a JSON object, sorted, for an exact-set assertion.
    fn key_set(value: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("published live state is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    /// The exact published shape, asserted key by key, because the consumer
    /// is in another crate and cannot be seen from this file.
    ///
    /// That consumer is `micad/apid/src/routes.rs` -- the MQTT pane, which reads
    /// `listen.address`, `listen.port`, `auth.enabled` and the `units` entry
    /// named `mica-mqtt-broker.service` out of the bus item `apply` returns.
    /// Changing this shape requires changing the pane. The key set is asserted
    /// and not only the values: a pane written against a flat shape this
    /// reconciler does not publish passes its own tests while its
    /// off-host-without-authentication warning can never fire, because
    /// `auth.enabled` does not arrive where it looks for it.
    /// `micad/apid/src/tests.rs` carries a verbatim copy of this expectation as
    /// its fixture -- two copies in two crates, but with a named source.
    #[tokio::test]
    async fn the_published_shape_is_the_contract_with_the_apid_pane() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _config) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings(true, "10.0.0.5", 8883, true))
            .await
            .unwrap();

        assert_eq!(
            key_set(&state),
            ["auth", "configPath", "enabled", "listen", "units"],
            "the pane reads these top-level keys by name"
        );
        assert_eq!(
            key_set(&state["listen"]),
            ["address", "port"],
            "the pane reads `listen.address` and `listen.port`"
        );
        assert_eq!(
            key_set(&state["auth"]),
            ["enabled"],
            "the pane reads `auth.enabled`; it is what the open-listener warning is gated on"
        );

        let units = state["units"]
            .as_array()
            .expect("`units` is an array -- the pane iterates it");
        assert_eq!(units.len(), 2);
        for unit in units {
            assert_eq!(
                key_set(unit),
                ["activeState", "unit", "unitFileState"],
                "the pane selects a unit by its `unit` field and shows its `activeState`"
            );
        }
        // Both names must be here. The pane selects by name and not by index,
        // so the ORDER is deliberately not part of this assertion -- but the
        // names are, and dropping one would leave that half of the switch
        // reading "unknown" on the page forever.
        let names: Vec<&str> = units
            .iter()
            .map(|unit| unit["unit"].as_str().expect("`unit` is a string"))
            .collect();
        assert!(names.contains(&BROKER_UNIT), "{names:?}");
        assert!(names.contains(&BRIDGE_UNIT), "{names:?}");
    }

    #[tokio::test]
    async fn live_state_names_both_units_and_the_config_path() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, config) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings(true, "127.0.0.1", 1883, false))
            .await
            .unwrap();

        assert_eq!(state["configPath"], json!(config.display().to_string()));
        assert_eq!(state["listen"]["address"], json!("127.0.0.1"));
        assert_eq!(state["listen"]["port"], json!(1883));
        assert_eq!(state["auth"]["enabled"], json!(false));
        // Read after the transition, so both report what they now are.
        assert_eq!(
            state["units"],
            json!([
                {
                    "unit": "mica-mqtt-broker.service",
                    "activeState": "active",
                    "unitFileState": "enabled-runtime",
                },
                {
                    "unit": "mica-mqttd.service",
                    "activeState": "active",
                    "unitFileState": "enabled-runtime",
                },
            ])
        );
    }
}

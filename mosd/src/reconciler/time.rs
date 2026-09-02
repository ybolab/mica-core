//! Time reconciler: the `time` settings subtree rendered into timesyncd
//! runtime configuration and the runtime presentation timezone.
//!
//! Two renders, one unit. `time.ntp.servers` becomes a `[Time]` drop-in under
//! `/run/systemd/timesyncd.conf.d/`, beside — never inside — the pinned base
//! policy the image ships in `/etc/systemd/timesyncd.conf.d/50-mos.conf`:
//! polling, retry and saved-clock intervals are PLAN-044 base policy, and
//! nothing here reads or writes them. `/run` and not STATE because the file is
//! derived entirely from the settings tree and re-rendered before the unit is
//! touched on every boot's `apply_all`, so persisting it would only create a
//! second copy of the truth (the `mqtt.rs` reasoning, verbatim).
//!
//! `time.timezone` becomes `/run/mos/timezone`, one IANA name and a newline.
//! Deliberately NOT `/etc/localtime`, and not for lack of a bind seam: machine,
//! RTC, API and log time are UTC by contract, and the image check
//! `packed-localtime-utc` (verify/src/checks-time.ts) spells out that a
//! timezone is presentation, applied by the settings surface, never a zone the
//! device itself runs in. `/etc/localtime` in the image is a symlink into
//! zoneinfo, so an `etc-hostname.mount`-style bind over it would resolve onto
//! the UTC zone file and hand every reader of UTC the operator's local zone —
//! the exact corruption the contract forbids. Consumers that want the operator
//! zone (the UI, an explicitly local schedule) read it from the settings
//! surface or from this file; everything else stays on UTC.
//!
//! There is no enable or pause path in this reconciler. timesyncd is an
//! always-running base service enabled by the image, so convergence only ever
//! means "running against the current render", never "off".

use std::path::PathBuf;

use anyhow::{Context, Result};
use mosd_settings::{Settings, TimeSettings};
use serde_json::json;

use super::Reconciler;
use super::systemd::{Systemd, UnitControl, is_active};
use crate::fswrite::write_config;

/// The one unit this reconciler converges.
const TIMESYNCD_UNIT: &str = "systemd-timesyncd.service";
/// Where the managed server list is rendered. `60-` so it sorts after the
/// shipped `50-mos.conf` and under a name the read-only `/etc` copy cannot
/// shadow (drop-ins of the SAME name resolve to `/etc` first).
const DEFAULT_SERVERS_PATH: &str = "/run/systemd/timesyncd.conf.d/60-mos-servers.conf";
/// Where the presentation timezone is published for on-device consumers.
const DEFAULT_TIMEZONE_PATH: &str = "/run/mos/timezone";
/// Where zone files live, for the reconcile-time existence check.
const DEFAULT_ZONEINFO_DIR: &str = "/usr/share/zoneinfo";
/// Mode of both renders: world-readable, owner-writable. Neither carries a
/// secret — a server name and a zone name are published to every
/// authenticated API reader anyway.
const CONFIG_MODE: u32 = 0o644;
/// `ActiveState` of a unit systemd has given up on; see `mqtt.rs` for why the
/// distinction from `inactive` matters only to the caller that must clear it.
const FAILED_STATE: &str = "failed";

/// Reconciler for the `time` settings subtree.
pub struct TimeReconciler<C: UnitControl> {
    /// Path the `NTP=` drop-in is rendered to.
    servers_path: PathBuf,
    /// Path the presentation timezone is rendered to.
    timezone_path: PathBuf,
    /// Root of the tzdata tree the existence check reads.
    zoneinfo_dir: PathBuf,
    control: C,
}

impl<C: UnitControl> TimeReconciler<C> {
    /// Create a time reconciler rendering to `servers_path` and
    /// `timezone_path`, checking zones under `zoneinfo_dir`, and driving the
    /// unit through `control`.
    ///
    /// The paths are parameters so tests run entirely inside a temporary
    /// directory and never touch the host's `/run` or its tzdata.
    pub fn new(
        servers_path: PathBuf,
        timezone_path: PathBuf,
        zoneinfo_dir: PathBuf,
        control: C,
    ) -> Self {
        Self {
            servers_path,
            timezone_path,
            zoneinfo_dir,
            control,
        }
    }
}

impl TimeReconciler<Systemd> {
    /// Production reconciler on the fixed runtime paths.
    pub fn production() -> Self {
        Self::new(
            PathBuf::from(DEFAULT_SERVERS_PATH),
            PathBuf::from(DEFAULT_TIMEZONE_PATH),
            PathBuf::from(DEFAULT_ZONEINFO_DIR),
            Systemd::new(),
        )
    }
}

/// Render the timesyncd drop-in for `time`.
///
/// Pure and deterministic: the same settings always produce the same bytes,
/// which is the signal deciding whether a running timesyncd is restarted.
///
/// An empty list renders a file with NO `NTP=` line rather than `NTP=` bare:
/// timesyncd reads an empty assignment as "clear the list", which would also
/// clear the compiled-in fallback pool and leave the device polling nothing.
/// Absent, the fallback pool applies, which is what an empty list means.
fn render_servers(time: &TimeSettings) -> String {
    let mut out = String::from(
        "# Managed NTP servers, rendered by mosd from `time.ntp.servers`\n\
         # (docs/design/time.md). The pinned base sync policy lives in\n\
         # /etc/systemd/timesyncd.conf.d/50-mos.conf and is never written here.\n\
         [Time]\n",
    );
    if !time.ntp.servers.is_empty() {
        out.push_str("NTP=");
        out.push_str(&time.ntp.servers.join(" "));
        out.push('\n');
    }
    out
}

impl<C: UnitControl> TimeReconciler<C> {
    /// Render `contents` to `path` and report whether its bytes changed.
    ///
    /// An unchanged render is not rewritten, for `mqtt.rs`'s reason: "did the
    /// bytes change" is what decides whether the running unit is restarted.
    fn render(&self, path: &PathBuf, contents: &str) -> Result<bool> {
        if let Ok(current) = std::fs::read_to_string(path)
            && current == contents
        {
            return Ok(false);
        }
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }
        write_config(path, contents, CONFIG_MODE)
            .with_context(|| format!("render {}", path.display()))?;
        Ok(true)
    }

    /// Whether `zone` names a zone file the device actually carries.
    ///
    /// Safe as a path join: [`mosd_settings::validate_timezone_name`] admits
    /// no empty, `.` or `..` component, so a stored zone cannot climb out of
    /// the tzdata tree. A hand-edited settings file could — so the same
    /// grammar is re-checked here before the name touches the filesystem.
    fn zone_available(&self, zone: &str) -> bool {
        mosd_settings::validate_timezone_name(zone).is_ok()
            && self.zoneinfo_dir.join(zone).is_file()
    }

    /// Bring timesyncd to "running against the current render".
    ///
    /// `restart`, not `reload`: the unit carries no `ExecReload`, and a
    /// restart costs one poll cycle, not a session (the sshd trade does not
    /// apply). Enablement is never touched — the image enables the unit
    /// statically in `sysinit.target.wants` and there is no pause control for
    /// a reconciler to spell.
    async fn converge_unit(&self, servers_changed: bool) -> Result<()> {
        let active_state = self.control.active_state(TIMESYNCD_UNIT).await?;
        if is_active(&active_state) {
            if servers_changed {
                self.control.restart(TIMESYNCD_UNIT).await?;
            }
        } else {
            // The mqtt.rs start-limit amendment: a failed unit inside its
            // start-limit window refuses start jobs outright, so clear the
            // failure first — and only when there is one, so the call log
            // still distinguishes the unit that needed rescuing.
            if active_state == FAILED_STATE {
                self.control.reset_failed(TIMESYNCD_UNIT).await?;
            }
            self.control.start(TIMESYNCD_UNIT).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl<C: UnitControl> Reconciler for TimeReconciler<C> {
    fn name(&self) -> &'static str {
        "time"
    }

    fn subtree(&self) -> &'static str {
        "time"
    }

    async fn apply(&self, settings: &Settings) -> Result<serde_json::Value> {
        let time = &settings.time;

        // Both renders before the unit is touched, unconditionally, for the
        // ordering `mqtt.rs` documents: the unit must never be (re)started
        // against a config older than the settings just applied.
        let servers_changed = self.render(&self.servers_path, &render_servers(time))?;
        let timezone_available = self.zone_available(&time.timezone);
        if !timezone_available {
            // A warn and a published fact, never an error: the write surface
            // already enforced the grammar, so this is a zone the device's
            // tzdata does not carry — worth surfacing, not worth failing the
            // NTP half of the subtree over.
            tracing::warn!(
                zone = %time.timezone,
                "time: the configured timezone names no zone file under {}; consumers keep \
                 falling back to UTC until it is corrected",
                self.zoneinfo_dir.display()
            );
        }
        self.render(&self.timezone_path, &format!("{}\n", time.timezone))?;

        // The timezone render never restarts timesyncd: timesyncd does not
        // read it, and machine time is UTC regardless of it.
        self.converge_unit(servers_changed).await?;

        Ok(json!({
            "ntp": {
                "servers": time.ntp.servers,
                "configPath": self.servers_path.display().to_string(),
            },
            "timezone": {
                "name": time.timezone,
                "available": timezone_available,
                "runtimePath": self.timezone_path.display().to_string(),
            },
            "unit": {
                "unit": TIMESYNCD_UNIT,
                "activeState": self.control.active_state(TIMESYNCD_UNIT).await?,
                "unitFileState": self.control.unit_file_state(TIMESYNCD_UNIT).await?,
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use mosd_settings::NtpSettings;

    use super::super::systemd::mock::MockUnitControl;
    use super::*;

    /// What `apply` renders for a configured server list.
    const GOLDEN_SERVERS: &str = "# Managed NTP servers, rendered by mosd from `time.ntp.servers`\n\
         # (docs/design/time.md). The pinned base sync policy lives in\n\
         # /etc/systemd/timesyncd.conf.d/50-mos.conf and is never written here.\n\
         [Time]\nNTP=0.pool.ntp.org 192.0.2.7\n";

    fn settings(servers: &[&str], timezone: &str) -> Settings {
        Settings {
            time: TimeSettings {
                ntp: NtpSettings {
                    servers: servers.iter().map(|s| (*s).to_string()).collect(),
                },
                timezone: timezone.to_string(),
            },
            ..Settings::default()
        }
    }

    /// Reconciler rendering into directories that do not exist yet, over a
    /// fixture zoneinfo tree carrying exactly `UTC` and `Europe/Berlin`.
    fn fixture(dir: &Path, active: &str) -> (TimeReconciler<MockUnitControl>, PathBuf, PathBuf) {
        let servers = dir.join("timesyncd.conf.d").join("60-mos-servers.conf");
        let timezone = dir.join("mos").join("timezone");
        let zoneinfo = dir.join("zoneinfo");
        std::fs::create_dir_all(zoneinfo.join("Europe")).unwrap();
        std::fs::write(zoneinfo.join("UTC"), "TZif").unwrap();
        std::fs::write(zoneinfo.join("Europe").join("Berlin"), "TZif").unwrap();
        (
            TimeReconciler::new(
                servers.clone(),
                timezone.clone(),
                zoneinfo,
                MockUnitControl::new(active, "enabled"),
            ),
            servers,
            timezone,
        )
    }

    #[tokio::test]
    async fn apply_writes_the_golden_renders_creating_their_directories() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, servers, timezone) = fixture(dir.path(), "active");

        reconciler
            .apply(&settings(&["0.pool.ntp.org", "192.0.2.7"], "Europe/Berlin"))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&servers).unwrap(), GOLDEN_SERVERS);
        assert_eq!(
            std::fs::read_to_string(&timezone).unwrap(),
            "Europe/Berlin\n"
        );
    }

    /// An empty list renders NO `NTP=` line — a bare `NTP=` would clear the
    /// compiled-in fallback pool and leave the device polling nothing, which
    /// is not what "no operator override" means.
    #[tokio::test]
    async fn an_empty_server_list_renders_no_ntp_line() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, servers, _timezone) = fixture(dir.path(), "active");

        reconciler.apply(&settings(&[], "UTC")).await.unwrap();

        let rendered = std::fs::read_to_string(&servers).unwrap();
        assert!(!rendered.contains("NTP="), "{rendered}");
        assert!(rendered.contains("[Time]"), "{rendered}");
    }

    #[tokio::test]
    async fn a_changed_server_list_restarts_the_running_unit() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _servers, _timezone) = fixture(dir.path(), "active");

        reconciler
            .apply(&settings(&["0.pool.ntp.org"], "UTC"))
            .await
            .unwrap();
        let after_first = reconciler.control.calls().len();
        reconciler
            .apply(&settings(&["1.pool.ntp.org"], "UTC"))
            .await
            .unwrap();

        assert_eq!(
            reconciler.control.calls()[after_first..],
            ["restart systemd-timesyncd.service".to_string()]
        );
    }

    #[tokio::test]
    async fn a_second_apply_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _servers, _timezone) = fixture(dir.path(), "active");
        let unchanged = settings(&["0.pool.ntp.org"], "Europe/Berlin");

        reconciler.apply(&unchanged).await.unwrap();
        let after_first = reconciler.control.calls();
        reconciler.apply(&unchanged).await.unwrap();

        assert_eq!(
            reconciler.control.calls(),
            after_first,
            "a repeated apply against an unchanged system must issue no calls"
        );
    }

    /// A timezone change alone never restarts timesyncd: the daemon does not
    /// read the timezone render, and machine time is UTC regardless of it.
    #[tokio::test]
    async fn a_timezone_change_does_not_restart_the_unit() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _servers, timezone) = fixture(dir.path(), "active");

        reconciler
            .apply(&settings(&["0.pool.ntp.org"], "UTC"))
            .await
            .unwrap();
        let after_first = reconciler.control.calls();
        reconciler
            .apply(&settings(&["0.pool.ntp.org"], "Europe/Berlin"))
            .await
            .unwrap();

        assert_eq!(reconciler.control.calls(), after_first);
        assert_eq!(
            std::fs::read_to_string(&timezone).unwrap(),
            "Europe/Berlin\n"
        );
    }

    /// There is no enable/pause control anywhere in this subtree, so an
    /// inactive always-running unit is simply started — and never enabled:
    /// the image owns the enablement symlink.
    #[tokio::test]
    async fn an_inactive_unit_is_started_and_never_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _servers, _timezone) = fixture(dir.path(), "inactive");

        reconciler.apply(&settings(&[], "UTC")).await.unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec!["start systemd-timesyncd.service".to_string()]
        );
    }

    /// The start-limit amendment, `mqtt.rs`'s: a failed unit is reset before
    /// the start, and ORDER is the assertion — a reset after the start would
    /// clear the failure and leave the unit still down.
    #[tokio::test]
    async fn a_failed_unit_is_reset_before_it_is_started() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _servers, _timezone) = fixture(dir.path(), "inactive");
        reconciler
            .control
            .set_active_state(TIMESYNCD_UNIT, "failed");

        reconciler.apply(&settings(&[], "UTC")).await.unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "reset-failed systemd-timesyncd.service".to_string(),
                "start systemd-timesyncd.service".to_string(),
            ]
        );
    }

    /// A zone the device's tzdata does not carry is a published fact and a
    /// warning, never an error: the NTP half of the subtree must not be
    /// blocked by a presentation value, and retries toward the configured
    /// servers continue regardless.
    #[tokio::test]
    async fn a_missing_zone_is_reported_and_does_not_fail_the_apply() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _servers, timezone) = fixture(dir.path(), "active");

        let state = reconciler
            .apply(&settings(&["0.pool.ntp.org"], "Atlantis/Made_Up"))
            .await
            .expect("a missing zone file warns; it must never fail the reconcile");

        assert_eq!(state["timezone"]["available"], json!(false));
        // Rendered verbatim anyway: the file describes the settings, and a
        // later tzdata that carries the zone needs no re-save to take effect.
        assert_eq!(
            std::fs::read_to_string(&timezone).unwrap(),
            "Atlantis/Made_Up\n"
        );
    }

    /// A hand-edited settings file can hold a zone the write surface would
    /// have refused; the existence check re-checks the grammar before the
    /// name touches the filesystem, so `../` climbs nowhere.
    #[tokio::test]
    async fn a_traversal_shaped_zone_is_unavailable_not_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _servers, _timezone) = fixture(dir.path(), "active");
        // A file that WOULD be found if the join were taken verbatim.
        std::fs::write(dir.path().join("secret"), "x").unwrap();

        let mut tree = settings(&[], "UTC");
        tree.time.timezone = "../secret".to_string();
        let state = reconciler.apply(&tree).await.unwrap();

        assert_eq!(state["timezone"]["available"], json!(false));
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

    /// The exact published shape: the consumer is the apid time pane in
    /// another crate, which reads these keys by name out of `GetState("time")`.
    #[tokio::test]
    async fn the_published_shape_is_the_contract_with_the_apid_pane() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _servers, _timezone) = fixture(dir.path(), "active");

        let state = reconciler
            .apply(&settings(&["0.pool.ntp.org"], "Europe/Berlin"))
            .await
            .unwrap();

        assert_eq!(key_set(&state), ["ntp", "timezone", "unit"]);
        assert_eq!(key_set(&state["ntp"]), ["configPath", "servers"]);
        assert_eq!(
            key_set(&state["timezone"]),
            ["available", "name", "runtimePath"]
        );
        assert_eq!(
            key_set(&state["unit"]),
            ["activeState", "unit", "unitFileState"]
        );
        assert_eq!(state["ntp"]["servers"], json!(["0.pool.ntp.org"]));
        assert_eq!(state["timezone"]["name"], json!("Europe/Berlin"));
        assert_eq!(state["timezone"]["available"], json!(true));
        assert_eq!(state["unit"]["unit"], json!(TIMESYNCD_UNIT));
    }

    #[test]
    fn the_render_is_deterministic() {
        let time = TimeSettings {
            ntp: NtpSettings {
                servers: vec!["a.example".to_string(), "b.example".to_string()],
            },
            timezone: "UTC".to_string(),
        };
        assert_eq!(render_servers(&time), render_servers(&time));
    }
}

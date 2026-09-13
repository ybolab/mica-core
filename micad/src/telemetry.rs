//! Board telemetry adapters (PLAN-052 / RFCT-288): temperature, watchdog and
//! reset reason, read from sysfs behind a testable root.
//!
//! The shape [`crate::storage_status`] takes: a trait with an unavailable
//! default so a dry-run daemon or a test never inspects its host, a
//! production adapter rooted at a path prefix so a fixture tree can stand in
//! for the device, and a pure rendering function the tests drive with literal
//! evidence. Served as `GetTelemetry` on the bus and
//! `GET /api/v1/system/telemetry` over HTTPS.
//!
//! **Absence is data, and this module never guesses.** A board whose kernel
//! exports no thermal zone reports `available: false` with the reason, not a
//! temperature of zero; a watchdog driver that does not implement
//! `bootstatus` leaves the reset reason `unknown` and says why. The reset
//! reason in particular is assembled from evidence the kernel exports
//! generically — the watchdog's `WDIOF_CARDRESET` boot-status bit and the
//! pstore records a crash leaves behind — and NOT from any board-specific
//! register: Linux has no generic reset-reason interface, and reading a
//! vendor register this code cannot be tested against would be a claim
//! nobody has verified. What this module reports on a physical board is
//! therefore an open validation item, recorded in
//! `docs/design/diagnostics.md`; nothing here fakes that validation.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value as Json, json};

/// Thermal zones, relative to the adapter's root.
pub const THERMAL_DIR: &str = "sys/class/thermal";
/// Hardware-monitor chips, relative to the adapter's root.
pub const HWMON_DIR: &str = "sys/class/hwmon";
/// Watchdog devices, relative to the adapter's root.
pub const WATCHDOG_DIR: &str = "sys/class/watchdog";
/// Persistent-store records of the previous boot, relative to the root.
pub const PSTORE_DIR: &str = "sys/fs/pstore";

/// The most entries of any one kind the surface carries. A board has a
/// handful of each; the cap keeps a runaway sysfs from growing the answer.
pub const MAX_ENTRIES: usize = 32;

/// One temperature reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThermalReading {
    /// The sysfs device: `thermal_zone0`, `hwmon1`.
    pub sensor: String,
    /// The zone `type` or the hwmon `name` (with `temp<N>_label` when there
    /// is one).
    pub label: String,
    /// Millidegrees Celsius, as sysfs reports it.
    pub milli_celsius: i64,
}

/// One watchdog device's exported state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchdogEvidence {
    /// The sysfs device: `watchdog0`.
    pub device: String,
    /// The driver's identity string.
    pub identity: Option<String>,
    /// `active` or `inactive`.
    pub state: Option<String>,
    /// The configured timeout in seconds.
    pub timeout_seconds: Option<u64>,
    /// Seconds until the next expiry, when the driver exports it.
    pub time_left_seconds: Option<u64>,
    /// The `WDIOF_*` boot-status bitmask, when the driver exports it.
    pub bootstatus: Option<u32>,
    /// Whether the watchdog cannot be stopped once started.
    pub nowayout: Option<bool>,
}

/// Everything the adapter reads.
#[derive(Debug, Clone, Default)]
pub struct TelemetryEvidence {
    /// Thermal-zone readings, in sysfs order.
    pub thermal_zones: Vec<ThermalReading>,
    /// Hardware-monitor temperature inputs, in sysfs order.
    pub hwmon: Vec<ThermalReading>,
    /// Every watchdog device, in sysfs order.
    pub watchdogs: Vec<WatchdogEvidence>,
    /// Whether the pstore directory exists at all: absent means the kernel
    /// mounted no pstore, which is different from an empty one.
    pub pstore_mounted: bool,
    /// Record names under pstore, sorted.
    pub pstore_records: Vec<String>,
}

/// `WDIOF_*` bits from `linux/watchdog.h`, in bit order.
const BOOTSTATUS_FLAGS: [(u32, &str); 8] = [
    (0x0001, "overheat"),
    (0x0002, "fanFault"),
    (0x0004, "extern1"),
    (0x0008, "extern2"),
    (0x0010, "powerUnder"),
    (0x0020, "cardReset"),
    (0x0040, "powerOver"),
    (0x8000, "keepalivePing"),
];

/// `WDIOF_CARDRESET`: the last reboot was this watchdog firing.
const CARD_RESET: u32 = 0x0020;

/// The named flags set in a `bootstatus` bitmask.
#[must_use]
pub fn decode_bootstatus(raw: u32) -> Vec<&'static str> {
    BOOTSTATUS_FLAGS
        .iter()
        .filter(|(bit, _)| raw & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// What the evidence says about the last reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// A watchdog reported `WDIOF_CARDRESET`.
    Watchdog,
    /// A `dmesg-*` pstore record exists: the previous kernel panicked or
    /// oopsed and dumped its log before dying.
    KernelCrash,
    /// No source flagged anything. A normal reboot, a power cycle and an
    /// external reset are indistinguishable from here.
    Unknown,
}

impl ResetReason {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Watchdog => "watchdog",
            Self::KernelCrash => "kernel-crash",
            Self::Unknown => "unknown",
        }
    }
}

/// Whether a pstore record is a crash dump rather than a console log.
fn is_crash_record(name: &str) -> bool {
    name.starts_with("dmesg-")
}

/// Classify the reset reason and say whether ANY source spoke.
///
/// The second value separates "the sources exist and flagged nothing" from
/// "there is no source on this board": both classify `Unknown`, and only the
/// first is evidence.
#[must_use]
pub fn classify_reset(evidence: &TelemetryEvidence) -> (ResetReason, bool) {
    let card_reset = evidence
        .watchdogs
        .iter()
        .any(|watchdog| watchdog.bootstatus.is_some_and(|raw| raw & CARD_RESET != 0));
    if card_reset {
        return (ResetReason::Watchdog, true);
    }
    if evidence
        .pstore_records
        .iter()
        .any(|name| is_crash_record(name))
    {
        return (ResetReason::KernelCrash, true);
    }
    let any_source = evidence.pstore_mounted
        || evidence
            .watchdogs
            .iter()
            .any(|watchdog| watchdog.bootstatus.is_some());
    (ResetReason::Unknown, any_source)
}

/// Read-only source of telemetry evidence.
#[async_trait::async_trait]
pub trait TelemetrySource: Send + Sync {
    /// Observe the current evidence.
    ///
    /// # Errors
    ///
    /// Returns an error only when this daemon has no adapter at all; a
    /// production adapter answers with absent evidence instead.
    async fn observe(&self) -> Result<TelemetryEvidence>;
}

/// The adapter a daemon without host access has: none.
pub struct UnavailableTelemetry;

#[async_trait::async_trait]
impl TelemetrySource for UnavailableTelemetry {
    async fn observe(&self) -> Result<TelemetryEvidence> {
        Err(anyhow::anyhow!("this daemon observes no board telemetry"))
    }
}

/// Production adapter over a sysfs root.
pub struct SysfsTelemetry {
    root: PathBuf,
}

impl SysfsTelemetry {
    /// The adapter `main.rs` attaches on a device.
    #[must_use]
    pub fn production() -> Self {
        Self::at("/")
    }

    /// An adapter rooted at `root`.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Sorted entry names under `relative` whose name starts with `prefix`,
    /// capped at [`MAX_ENTRIES`].
    fn entries(&self, relative: &str, prefix: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.path(relative)) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with(prefix))
            .collect();
        names.sort();
        names.truncate(MAX_ENTRIES);
        names
    }

    fn thermal_zones(&self) -> Vec<ThermalReading> {
        self.entries(THERMAL_DIR, "thermal_zone")
            .into_iter()
            .filter_map(|zone| {
                let base = self.path(THERMAL_DIR).join(&zone);
                let milli_celsius = read_i64(&base.join("temp"))?;
                Some(ThermalReading {
                    label: read_trimmed(&base.join("type")).unwrap_or_else(|| zone.clone()),
                    sensor: zone,
                    milli_celsius,
                })
            })
            .collect()
    }

    fn hwmon(&self) -> Vec<ThermalReading> {
        let mut readings = Vec::new();
        for chip in self.entries(HWMON_DIR, "hwmon") {
            let base = self.path(HWMON_DIR).join(&chip);
            let name = read_trimmed(&base.join("name")).unwrap_or_else(|| chip.clone());
            let Ok(files) = std::fs::read_dir(&base) else {
                continue;
            };
            let mut inputs: Vec<String> = files
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .filter(|file| file.starts_with("temp") && file.ends_with("_input"))
                .collect();
            inputs.sort();
            for input in inputs {
                let Some(milli_celsius) = read_i64(&base.join(&input)) else {
                    continue;
                };
                let channel = input.trim_end_matches("_input");
                let label = read_trimmed(&base.join(format!("{channel}_label"))).map_or_else(
                    || format!("{name} {channel}"),
                    |label| format!("{name} {label}"),
                );
                readings.push(ThermalReading {
                    sensor: format!("{chip}/{channel}"),
                    label,
                    milli_celsius,
                });
                if readings.len() >= MAX_ENTRIES {
                    return readings;
                }
            }
        }
        readings
    }

    fn watchdogs(&self) -> Vec<WatchdogEvidence> {
        self.entries(WATCHDOG_DIR, "watchdog")
            .into_iter()
            .map(|device| {
                let base = self.path(WATCHDOG_DIR).join(&device);
                WatchdogEvidence {
                    identity: read_trimmed(&base.join("identity")),
                    state: read_trimmed(&base.join("state")),
                    timeout_seconds: read_u64(&base.join("timeout")),
                    time_left_seconds: read_u64(&base.join("timeleft")),
                    bootstatus: read_trimmed(&base.join("bootstatus"))
                        .and_then(|text| parse_bootstatus(&text)),
                    nowayout: read_trimmed(&base.join("nowayout")).map(|text| text == "1"),
                    device,
                }
            })
            .collect()
    }

    fn pstore(&self) -> (bool, Vec<String>) {
        let path = self.path(PSTORE_DIR);
        if !path.is_dir() {
            return (false, Vec::new());
        }
        (true, self.entries(PSTORE_DIR, ""))
    }
}

/// `bootstatus` is printed as a decimal integer, which may be negative when
/// the driver reports an error; only a non-negative value is a bitmask.
fn parse_bootstatus(text: &str) -> Option<u32> {
    let raw: i64 = text.trim().parse().ok()?;
    u32::try_from(raw).ok()
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn read_i64(path: &Path) -> Option<i64> {
    read_trimmed(path)?.parse().ok()
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

#[async_trait::async_trait]
impl TelemetrySource for SysfsTelemetry {
    async fn observe(&self) -> Result<TelemetryEvidence> {
        let (pstore_mounted, pstore_records) = self.pstore();
        Ok(TelemetryEvidence {
            thermal_zones: self.thermal_zones(),
            hwmon: self.hwmon(),
            watchdogs: self.watchdogs(),
            pstore_mounted,
            pstore_records,
        })
    }
}

fn absent(detail: impl Into<String>) -> Json {
    json!({ "available": false, "detail": detail.into() })
}

fn reading_json(reading: &ThermalReading) -> Json {
    json!({
        "sensor": reading.sensor,
        "label": reading.label,
        "milliCelsius": reading.milli_celsius,
    })
}

fn watchdog_json(watchdog: &WatchdogEvidence) -> Json {
    let mut root = serde_json::Map::new();
    root.insert("device".to_string(), json!(watchdog.device));
    if let Some(identity) = &watchdog.identity {
        root.insert("identity".to_string(), json!(identity));
    }
    if let Some(state) = &watchdog.state {
        root.insert("state".to_string(), json!(state));
    }
    if let Some(timeout) = watchdog.timeout_seconds {
        root.insert("timeoutSeconds".to_string(), json!(timeout));
    }
    if let Some(left) = watchdog.time_left_seconds {
        root.insert("timeLeftSeconds".to_string(), json!(left));
    }
    root.insert(
        "bootstatus".to_string(),
        match watchdog.bootstatus {
            Some(raw) => json!({
                "available": true,
                "raw": raw,
                "flags": decode_bootstatus(raw),
            }),
            None => absent("this watchdog driver does not export bootstatus"),
        },
    );
    if let Some(nowayout) = watchdog.nowayout {
        root.insert("nowayout".to_string(), json!(nowayout));
    }
    Json::Object(root)
}

/// `evidence` rendered as the JSON the bus method serves: `thermal`,
/// `watchdog` and `reset`, each carrying `available`.
#[must_use]
pub fn telemetry_json(evidence: &TelemetryEvidence) -> Json {
    let thermal = if evidence.thermal_zones.is_empty() && evidence.hwmon.is_empty() {
        absent("no thermal zone and no hwmon temperature input is exported under sysfs")
    } else {
        json!({
            "available": true,
            "zones": evidence.thermal_zones.iter().map(reading_json).collect::<Vec<_>>(),
            "hwmon": evidence.hwmon.iter().map(reading_json).collect::<Vec<_>>(),
        })
    };
    let watchdog = if evidence.watchdogs.is_empty() {
        absent("no watchdog device is exported under sysfs")
    } else {
        json!({
            "available": true,
            "devices": evidence.watchdogs.iter().map(watchdog_json).collect::<Vec<_>>(),
        })
    };
    let (reason, any_source) = classify_reset(evidence);
    let mut reset = serde_json::Map::new();
    reset.insert("available".to_string(), json!(any_source));
    reset.insert("reason".to_string(), json!(reason.as_str()));
    reset.insert(
        "detail".to_string(),
        json!(match (reason, any_source) {
            (ResetReason::Watchdog, _) => "a watchdog reports WDIOF_CARDRESET: the last reboot was a watchdog reset",
            (ResetReason::KernelCrash, _) => "a dmesg pstore record exists: the previous kernel panicked or oopsed and dumped its log",
            (ResetReason::Unknown, true) => "no watchdog flagged a reset and no crash record exists; a normal reboot, a power cycle and an external reset are indistinguishable on this board",
            (ResetReason::Unknown, false) => "this board's kernel exports no reset-reason source: no watchdog bootstatus and no pstore",
        }),
    );
    let pstore = if evidence.pstore_mounted {
        json!({ "available": true, "records": evidence.pstore_records })
    } else {
        absent("no pstore is mounted")
    };
    let watchdog_flags: Vec<Json> = evidence
        .watchdogs
        .iter()
        .filter_map(|watchdog| {
            watchdog
                .bootstatus
                .map(|raw| json!({ "device": watchdog.device, "flags": decode_bootstatus(raw) }))
        })
        .collect();
    reset.insert(
        "evidence".to_string(),
        json!({ "watchdogBootstatus": watchdog_flags, "pstore": pstore }),
    );
    json!({
        "thermal": thermal,
        "watchdog": watchdog,
        "reset": Json::Object(reset),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write fixture");
    }

    #[test]
    fn bootstatus_bits_decode_by_name() {
        assert_eq!(decode_bootstatus(0), Vec::<&str>::new());
        assert_eq!(decode_bootstatus(0x20), vec!["cardReset"]);
        assert_eq!(decode_bootstatus(0x21), vec!["overheat", "cardReset"]);
        assert_eq!(decode_bootstatus(0x8000), vec!["keepalivePing"]);
        // A bit the header does not name decodes to nothing rather than to a
        // guess.
        assert_eq!(decode_bootstatus(0x100), Vec::<&str>::new());
    }

    /// The full sysfs shape: two thermal zones, a hwmon chip with a labelled
    /// input, a watchdog that fired, and a crash record.
    #[tokio::test]
    async fn a_populated_sysfs_yields_every_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(
            root,
            "sys/class/thermal/thermal_zone0/type",
            "soc-thermal\n",
        );
        write(root, "sys/class/thermal/thermal_zone0/temp", "48250\n");
        write(
            root,
            "sys/class/thermal/thermal_zone1/type",
            "gpu-thermal\n",
        );
        write(root, "sys/class/thermal/thermal_zone1/temp", "-1500\n");
        write(root, "sys/class/thermal/cooling_device0/type", "fan\n");
        write(root, "sys/class/hwmon/hwmon0/name", "pmic\n");
        write(root, "sys/class/hwmon/hwmon0/temp1_input", "39000\n");
        write(root, "sys/class/hwmon/hwmon0/temp1_label", "die\n");
        write(root, "sys/class/hwmon/hwmon0/temp2_input", "41000\n");
        write(root, "sys/class/watchdog/watchdog0/identity", "dw_wdt\n");
        write(root, "sys/class/watchdog/watchdog0/state", "active\n");
        write(root, "sys/class/watchdog/watchdog0/timeout", "30\n");
        write(root, "sys/class/watchdog/watchdog0/bootstatus", "32\n");
        write(root, "sys/class/watchdog/watchdog0/nowayout", "1\n");
        write(root, "sys/fs/pstore/dmesg-ramoops-0", "Panic#1 Part1\n");
        write(root, "sys/fs/pstore/console-ramoops-0", "...\n");

        let evidence = SysfsTelemetry::at(root).observe().await.expect("observe");
        let telemetry = telemetry_json(&evidence);

        assert_eq!(telemetry["thermal"]["available"], true);
        assert_eq!(telemetry["thermal"]["zones"][0]["label"], "soc-thermal");
        assert_eq!(telemetry["thermal"]["zones"][0]["milliCelsius"], 48250);
        assert_eq!(telemetry["thermal"]["zones"][1]["milliCelsius"], -1500);
        assert_eq!(telemetry["thermal"]["zones"].as_array().unwrap().len(), 2);
        assert_eq!(telemetry["thermal"]["hwmon"][0]["label"], "pmic die");
        assert_eq!(telemetry["thermal"]["hwmon"][0]["sensor"], "hwmon0/temp1");
        assert_eq!(telemetry["thermal"]["hwmon"][1]["label"], "pmic temp2");
        assert_eq!(telemetry["watchdog"]["devices"][0]["identity"], "dw_wdt");
        assert_eq!(telemetry["watchdog"]["devices"][0]["timeoutSeconds"], 30);
        assert_eq!(telemetry["watchdog"]["devices"][0]["nowayout"], true);
        assert_eq!(
            telemetry["watchdog"]["devices"][0]["bootstatus"]["flags"],
            json!(["cardReset"])
        );
        assert!(
            telemetry["watchdog"]["devices"][0]
                .get("timeLeftSeconds")
                .is_none()
        );
        // The watchdog flag outranks the crash record.
        assert_eq!(telemetry["reset"]["available"], true);
        assert_eq!(telemetry["reset"]["reason"], "watchdog");
        assert_eq!(
            telemetry["reset"]["evidence"]["pstore"]["records"],
            json!(["console-ramoops-0", "dmesg-ramoops-0"])
        );
    }

    /// A crash record with a clean watchdog is a kernel crash; a console
    /// record alone is not — it is the previous boot's console, which every
    /// ramoops reboot leaves behind.
    #[test]
    fn a_dmesg_record_is_a_crash_and_a_console_record_is_not() {
        let crashed = TelemetryEvidence {
            watchdogs: vec![WatchdogEvidence {
                bootstatus: Some(0),
                ..WatchdogEvidence::default()
            }],
            pstore_mounted: true,
            pstore_records: vec!["dmesg-ramoops-0".to_string()],
            ..TelemetryEvidence::default()
        };
        assert_eq!(classify_reset(&crashed), (ResetReason::KernelCrash, true));

        let console_only = TelemetryEvidence {
            pstore_records: vec!["console-ramoops-0".to_string()],
            ..crashed.clone()
        };
        assert_eq!(classify_reset(&console_only), (ResetReason::Unknown, true));
        let rendered = telemetry_json(&console_only);
        assert_eq!(rendered["reset"]["reason"], "unknown");
        assert_eq!(rendered["reset"]["available"], true);
        assert!(
            rendered["reset"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("indistinguishable"))
        );
    }

    /// The empty root: every member absent with a reason, the reset reason
    /// `unknown` AND `available: false`, because no source spoke.
    #[tokio::test]
    async fn an_empty_sysfs_reports_absence_not_health() {
        let dir = tempfile::tempdir().expect("tempdir");
        let evidence = SysfsTelemetry::at(dir.path())
            .observe()
            .await
            .expect("observe");
        let telemetry = telemetry_json(&evidence);
        assert_eq!(telemetry["thermal"]["available"], false);
        assert!(telemetry["thermal"]["detail"].is_string());
        assert_eq!(telemetry["watchdog"]["available"], false);
        assert_eq!(telemetry["reset"]["available"], false);
        assert_eq!(telemetry["reset"]["reason"], "unknown");
        assert!(
            telemetry["reset"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("no reset-reason source"))
        );
        assert_eq!(telemetry["reset"]["evidence"]["pstore"]["available"], false);
    }

    /// A watchdog driver without `bootstatus` is reported as such on the
    /// device, and does not count as a source for the reset reason.
    #[tokio::test]
    async fn a_watchdog_without_bootstatus_is_absent_evidence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "sys/class/watchdog/watchdog0/identity", "sp805\n");
        write(root, "sys/class/watchdog/watchdog0/state", "inactive\n");
        let evidence = SysfsTelemetry::at(root).observe().await.expect("observe");
        let telemetry = telemetry_json(&evidence);
        assert_eq!(telemetry["watchdog"]["available"], true);
        assert_eq!(
            telemetry["watchdog"]["devices"][0]["bootstatus"]["available"],
            false
        );
        assert_eq!(telemetry["reset"]["available"], false);
    }

    /// A negative bootstatus (a driver error) is not a bitmask.
    #[test]
    fn a_negative_bootstatus_is_not_a_bitmask() {
        assert_eq!(parse_bootstatus("-1\n"), None);
        assert_eq!(parse_bootstatus("32\n"), Some(32));
        assert_eq!(parse_bootstatus("x"), None);
    }

    #[tokio::test]
    async fn the_unavailable_adapter_is_an_error() {
        assert!(UnavailableTelemetry.observe().await.is_err());
    }
}

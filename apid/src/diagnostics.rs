//! The diagnostic snapshot (PLAN-052 / RFCT-288): collection, redaction and
//! the bounded on-disk store. `docs/design/diagnostics.md` is the contract
//! this module implements; the route layer is `routes.rs`.
//!
//! Three properties are enforced here rather than promised:
//!
//! - **Bounded in time and size.** [`Collector::collect`] reads every
//!   section under a per-section timeout inside one overall deadline, and a
//!   section that does not answer is recorded absent with the reason; the
//!   snapshot is produced whatever the sources do. Every section's size is
//!   bounded at its source (mosd caps the journal, the manifest, the unit
//!   list), and [`SnapshotStore::publish`] refuses a snapshot above
//!   [`MAX_SNAPSHOT_BYTES`] outright.
//! - **Redaction fails closed.** [`redact_snapshot`] walks the assembled
//!   value against an ALLOWLIST schema: a field the schema does not name is
//!   dropped, a field it marks is replaced by the sentinel, and every string
//!   passes [`scrub`]. The denylist `crate::redact` applies to the live
//!   routes is applied first, so a schema mistake naming a secret field
//!   still ships the sentinel and not the value.
//! - **Atomic and flash-wear friendly.** A snapshot is written once to a
//!   staging file, fsynced, and renamed into place; nothing else on disk is
//!   rewritten per snapshot, retention is a count and a byte cap enforced by
//!   removing the oldest, and deletion is an explicit route.
//!
//! What this module deliberately does NOT do: upload anything, open a
//! shell, capture packets, or read a source the contract does not list.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

use crate::redact;
use crate::settings_api::SettingsApi;

/// The snapshot schema version; bumped when a member changes shape.
///
/// 1 named the shape with `system.system.buildEpoch` / `buildDate` /
/// `buildDateDetail`. 2 names the one shipped now, where those are the
/// `commitDate` and `fileEpoch` objects instead. Nothing migrates a stored
/// snapshot and nothing needs to: the number is what lets a reader holding
/// two of them tell which shape each is.
pub const SCHEMA_VERSION: u64 = 2;
/// The redaction schema version; bumped when the allowlist changes.
///
/// 2 is the allowlist that followed the rename above, 3 the one that added
/// the rtnetlink id members. Both changes are real; the second bump was made
/// for the first change by mistake and is left standing, because the number's
/// only job is that two different allowlists never share it. 4 is the one that
/// added `system.trust` -- the grade of the signing material the image was
/// built from, which a support case reads before it reads anything else about
/// a refused update.
pub const REDACTION_SCHEMA_VERSION: u64 = 4;
/// The shipped location of the store: the system-owned DATA namespace, so a
/// snapshot survives a reboot (`/var` is disposable) and a rootfs update.
pub const DEFAULT_ROOT: &str = "/mos/diagnostics";
/// The most snapshots retained; publishing one more removes the oldest.
pub const MAX_SNAPSHOTS: usize = 8;
/// The most bytes the store holds across all snapshots.
pub const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
/// The most bytes one snapshot may be; larger is refused, not trimmed.
pub const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
/// The whole collection's deadline.
pub const COLLECTION_DEADLINE: Duration = Duration::from_secs(20);
/// One section's timeout, inside the deadline. Above the bus client's own
/// 5 s bound so a mosd timeout keeps its own classification.
pub const SECTION_TIMEOUT: Duration = Duration::from_secs(6);
/// The most bytes any one string in a snapshot keeps.
pub const MAX_TEXT_BYTES: usize = 1024;
/// The most failed tasks the `failures.tasks` member carries.
pub const MAX_FAILED_TASKS: usize = 32;
/// What a string carrying a secret marker becomes, whole.
pub const REDACTED_LINE: &str = "<redacted line>";
/// What a hardware-address-shaped token inside a string becomes.
pub const REDACTED_MAC: &str = "<mac>";
/// The longest dynamic key (a unit name, a health component) kept.
const MAX_KEY_LEN: usize = 128;

/// Field names whose values never ship, whatever the allowlist says of
/// them. `crate::redact`'s five plus the names a credential travels under.
const SNAPSHOT_SECRET_FIELDS: [&str; 14] = [
    "token",
    "tokens",
    "secret",
    "secrets",
    "password",
    "passwd",
    "passphrase",
    "credential",
    "credentials",
    "authorization",
    "cookie",
    "apiTokens",
    "key",
    "keys",
];

/// Substrings (matched case-insensitively) that mark a string as carrying
/// a secret; the whole string is replaced.
const SECRET_MARKERS: [&str; 15] = [
    "password",
    "passwd",
    "passphrase",
    "psk",
    "secret",
    "token",
    "private key",
    "privatekey",
    "private_key",
    "authorization:",
    "bearer ",
    "-----begin",
    "api key",
    "apikey",
    "api_key",
];

// ---- redaction ----------------------------------------------------------

/// One node of the allowlist schema.
#[derive(Debug, Clone)]
enum Rule {
    /// A scalar: null, bool, number, or a string (scrubbed). An object or
    /// array here is unclassified and dropped.
    Scalar,
    /// Kept as the sentinel: the field's presence is evidence, its value is
    /// identifying.
    Redact,
    /// An object with these named members. `available` (bool) and `detail`
    /// (string) are allowed in every object, because every absent member
    /// carries them.
    Object(Vec<(&'static str, Rule)>),
    /// An object with dynamic keys, each value by the rule.
    Map(Box<Rule>),
    /// An array of values by the rule.
    Array(Box<Rule>),
}

fn obj(fields: Vec<(&'static str, Rule)>) -> Rule {
    Rule::Object(fields)
}

fn map(rule: Rule) -> Rule {
    Rule::Map(Box::new(rule))
}

fn arr(rule: Rule) -> Rule {
    Rule::Array(Box::new(rule))
}

use Rule::Redact as R;
use Rule::Scalar as S;

/// The whole snapshot schema, version [`REDACTION_SCHEMA_VERSION`].
///
/// Every member mosd or apid produces is named here or it does not ship.
/// Hardware addresses, SSIDs and BSSIDs are the personally identifying
/// network fields the contract redacts; IP addresses, routes and DNS servers
/// are the troubleshooting evidence it keeps.
fn schema() -> Rule {
    let board = || obj(vec![("model", S), ("source", S)]);
    let kernel = || obj(vec![("release", S), ("version", S)]);
    let release = || {
        obj(vec![
            ("name", S),
            ("id", S),
            ("version", S),
            ("versionId", S),
            ("prettyName", S),
            ("buildId", S),
            ("imageId", S),
            ("imageVersion", S),
        ])
    };
    let slot = || {
        obj(vec![
            ("booted", S),
            ("bootname", S),
            ("bundleVersion", S),
            ("bootStatus", S),
            ("primary", S),
        ])
    };
    let uptime = || obj(vec![("seconds", S)]);
    let git_stamp = obj(vec![
        ("commit", S),
        ("dirty", S),
        ("revision", S),
        ("consistent", S),
        ("stamps", arr(S)),
    ]);
    let system_member = obj(vec![
        ("version", S),
        ("package", S),
        ("gitStamp", git_stamp),
        // The two times mosd's system_info reports, which are different facts:
        // `commitDate` is when the source was committed, `fileEpoch` is the
        // pinned SOURCE_DATE_EPOCH every file in the image carries. A key that
        // is not named here is DROPPED from every snapshot without a word, so
        // renaming a field on that surface without renaming it here would ship
        // snapshots that silently lost it.
        ("commitDate", obj(vec![("date", S)])),
        ("fileEpoch", obj(vec![("epoch", S), ("date", S)])),
    ]);
    let packages = obj(vec![
        ("count", S),
        ("mosCount", S),
        ("malformedRows", S),
        ("truncated", S),
        (
            "entries",
            arr(obj(vec![
                ("name", S),
                ("version", S),
                ("architecture", S),
                ("mos", S),
            ])),
        ),
    ]);
    // What the image says about the grade of the material it was signed with,
    // and the domains a development marker names. A support case that opens
    // with "the update was refused" is answered differently depending on this
    // value, so a snapshot that dropped it would send the one fact the reader
    // needed back to the person who has to ask for it again.
    let trust = obj(vec![
        ("grade", S),
        ("developmentDomains", arr(S)),
        ("marker", S),
    ]);
    let system = obj(vec![
        ("machineId", obj(vec![("id", S)])),
        ("board", board()),
        ("kernel", kernel()),
        ("release", release()),
        ("system", system_member),
        ("trust", trust),
        (
            "daemon",
            obj(vec![("name", S), ("version", S), ("commit", S)]),
        ),
        ("packages", packages),
        ("slot", slot()),
        ("uptime", uptime()),
    ]);

    let reading = || obj(vec![("sensor", S), ("label", S), ("milliCelsius", S)]);
    let thermal = obj(vec![("zones", arr(reading())), ("hwmon", arr(reading()))]);
    let watchdog = obj(vec![(
        "devices",
        arr(obj(vec![
            ("device", S),
            ("identity", S),
            ("state", S),
            ("timeoutSeconds", S),
            ("timeLeftSeconds", S),
            ("bootstatus", obj(vec![("raw", S), ("flags", arr(S))])),
            ("nowayout", S),
        ])),
    )]);
    let reset = obj(vec![
        ("reason", S),
        (
            "evidence",
            obj(vec![
                (
                    "watchdogBootstatus",
                    arr(obj(vec![("device", S), ("flags", arr(S))])),
                ),
                ("pstore", obj(vec![("records", arr(S))])),
            ]),
        ),
    ]);
    let update = obj(vec![
        ("operation", S),
        ("last_error", S),
        (
            "progress",
            obj(vec![("percentage", S), ("message", S), ("depth", S)]),
        ),
        (
            "slots",
            map(obj(vec![
                ("state", S),
                ("bootname", S),
                ("boot_status", S),
                ("bundle_version", S),
                ("installed_timestamp", S),
                ("status", S),
            ])),
        ),
        ("booted_slot", S),
        ("primary", S),
        ("pending_not_confirmed", S),
        (
            "install",
            obj(vec![
                ("status", S),
                ("bundle", S),
                ("requested_by", S),
                ("error", S),
            ]),
        ),
        (
            "last_mark",
            obj(vec![
                ("state", S),
                ("slot", S),
                ("message", S),
                ("requested_by", S),
            ]),
        ),
        ("error", S),
    ]);
    let boot = obj(vec![
        ("slot", slot()),
        ("uptime", uptime()),
        ("reset", reset),
        ("update", update),
    ]);

    let journal = obj(vec![
        ("scope", S),
        ("priority", S),
        ("lineCount", S),
        ("sourceLines", S),
        ("sourceBytes", S),
        ("truncated", S),
        (
            "bounds",
            obj(vec![("maxLines", S), ("maxBytes", S), ("maxLineBytes", S)]),
        ),
        ("lines", arr(S)),
    ]);
    let task = obj(vec![
        ("id", S),
        ("operation", S),
        ("dotPath", S),
        ("source", S),
        ("status", S),
        ("enqueuedAt", S),
        ("startedAt", S),
        ("finishedAt", S),
        ("outcome", S),
        ("message", S),
        ("foldedCount", S),
    ]);
    let failures = obj(vec![
        (
            "units",
            obj(vec![
                ("count", S),
                ("truncated", S),
                (
                    "entries",
                    arr(obj(vec![
                        ("name", S),
                        ("description", S),
                        ("loadState", S),
                        ("activeState", S),
                        ("subState", S),
                    ])),
                ),
            ]),
        ),
        ("tasks", arr(task)),
        ("health", map(obj(vec![("status", S)]))),
    ]);

    let space = obj(vec![
        ("totalBytes", S),
        ("usedBytes", S),
        ("freeBytes", S),
        ("reservedBytes", S),
        ("usedPercent", S),
    ]);
    let tier = obj(vec![
        ("name", S),
        ("role", S),
        ("partitionLabel", S),
        ("expectedMount", S),
        ("present", S),
        ("device", S),
        ("partitionBytes", S),
        ("mounted", S),
        ("mount", S),
        ("filesystem", S),
        ("readOnly", S),
        ("space", space.clone()),
        ("pressure", S),
        (
            "updateWorkspace",
            obj(vec![("root", S), ("reservedBytes", S), ("available", S)]),
        ),
        (
            "check",
            obj(vec![
                ("recorded", S),
                ("unit", S),
                ("activeState", S),
                ("result", S),
                ("exitStatus", S),
            ]),
        ),
    ]);
    let bind = obj(vec![
        ("name", S),
        ("mount", S),
        ("source", S),
        ("owner", S),
        ("readiness", S),
        ("mounted", S),
        ("device", S),
        ("readOnly", S),
        ("filesystem", S),
        ("sourceOnData", S),
        ("sourceIsDirectory", S),
        ("space", space),
        ("pressure", S),
        (
            "probe",
            obj(vec![
                ("attempted", S),
                ("passed", S),
                ("error", S),
                ("reason", S),
            ]),
        ),
    ]);
    let health = obj(vec![
        ("supported", S),
        ("reason", S),
        ("source", S),
        ("raw", obj(vec![("lifeTime", S), ("preEolInfo", S)])),
        (
            "lifetimeEstimates",
            arr(obj(vec![
                ("raw", S),
                ("usedPercentMin", S),
                ("usedPercentMax", S),
            ])),
        ),
        ("preEol", S),
    ]);
    let storage = obj(vec![
        ("tiers", arr(tier)),
        (
            "namespaces",
            obj(vec![("sharedCapacityTier", S), ("binds", arr(bind))]),
        ),
        (
            "media",
            arr(obj(vec![
                ("name", S),
                ("kind", S),
                ("sizeBytes", S),
                ("model", S),
                ("rotational", S),
                ("health", health),
            ])),
        ),
        (
            "policy",
            obj(vec![
                ("warningPercent", S),
                ("warningClearPercent", S),
                ("criticalPercent", S),
                ("criticalClearPercent", S),
                ("updateWorkspaceReservedBytes", S),
                ("updateWorkspaceRoot", S),
                ("watchedTiers", arr(S)),
            ]),
        ),
        ("lifecycle", map(S)),
    ]);

    let time = obj(vec![
        ("status", S),
        ("synchronized", S),
        ("server", obj(vec![("name", S), ("address", S)])),
        (
            "sample",
            obj(vec![
                ("leap", S),
                ("stratum", S),
                ("spike", S),
                ("offsetSeconds", S),
                ("packetCount", S),
                ("correction", S),
            ]),
        ),
    ]);

    let address = obj(vec![
        ("family", S),
        ("address", S),
        ("prefixLength", S),
        ("scope", S),
        ("scopeId", S),
        ("configSource", S),
    ]);
    let lease = obj(vec![
        ("address", S),
        ("prefixLength", S),
        ("server", S),
        ("router", S),
        ("lifetimeSeconds", S),
    ]);
    let association = || {
        obj(vec![
            ("interface", S),
            ("state", S),
            ("associated", S),
            ("ssid", R),
            ("bssid", R),
            ("frequencyMhz", S),
            ("keyManagement", S),
            ("rssiDbm", S),
            ("linkSpeedMbps", S),
        ])
    };
    let interface = obj(vec![
        ("name", S),
        ("index", S),
        ("kind", S),
        ("type", S),
        ("driver", S),
        ("mtu", S),
        (
            "link",
            obj(vec![
                ("administrativeState", S),
                ("operationalState", S),
                ("carrierState", S),
                ("carrier", S),
                ("onlineState", S),
                ("addressState", S),
            ]),
        ),
        ("hardwareAddress", R),
        ("addresses", arr(address)),
        (
            "dhcp",
            obj(vec![("inferred", S), ("state", S), ("lease", lease)]),
        ),
        ("dns", arr(S)),
        ("wifi", association()),
    ]);
    let route = obj(vec![
        ("family", S),
        ("gateway", S),
        ("interface", S),
        ("interfaceIndex", S),
        ("metric", S),
        ("protocol", S),
        ("protocolId", S),
        ("table", S),
        ("tableId", S),
        ("configSource", S),
    ]);
    let network = obj(vec![
        (
            "interfaces",
            obj(vec![("count", S), ("entries", arr(interface))]),
        ),
        (
            "defaultRoutes",
            obj(vec![("count", S), ("entries", arr(route))]),
        ),
        (
            "dns",
            obj(vec![
                ("linkServers", arr(S)),
                ("resolverServers", arr(S)),
                (
                    "probe",
                    obj(vec![("name", S), ("reachable", S), ("result", S)]),
                ),
            ]),
        ),
        (
            "wifi",
            obj(vec![
                ("interfaces", arr(S)),
                ("associations", arr(association())),
            ]),
        ),
        (
            "capabilities",
            obj(vec![
                ("wifi", obj(vec![("supported", S), ("interfaces", arr(S))])),
                (
                    "bluetooth",
                    obj(vec![("supported", S), ("adapters", arr(S))]),
                ),
                (
                    "cellular",
                    obj(vec![("supported", S), ("interfaces", arr(S))]),
                ),
            ]),
        ),
    ]);

    obj(vec![
        ("schemaVersion", S),
        ("collectedAt", S),
        (
            "release",
            obj(vec![
                ("board", board()),
                ("release", release()),
                ("kernel", kernel()),
            ]),
        ),
        ("system", system),
        ("boot", boot),
        ("journal", journal),
        ("failures", failures),
        ("storage", storage),
        ("time", time),
        (
            "telemetry",
            obj(vec![("thermal", thermal), ("watchdog", watchdog)]),
        ),
        ("network", network),
    ])
}

/// What one redaction pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RedactionStats {
    /// Fields the schema did not name, and values of the wrong shape: gone.
    pub dropped_fields: usize,
    /// Fields replaced by a sentinel, strings replaced whole, and tokens
    /// replaced inside strings.
    pub redacted_fields: usize,
}

/// Whether `token` is a hardware address: six pairs of hex separated by
/// colons, and nothing else.
fn is_mac_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 17
        && bytes.iter().enumerate().all(|(index, byte)| {
            if index % 3 == 2 {
                *byte == b':'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

/// Scrub one string: a secret marker anywhere replaces it whole, a
/// hardware-address-shaped token is replaced in place, and the result is
/// capped at [`MAX_TEXT_BYTES`]. Returns the text and how many
/// replacements were made.
#[must_use]
pub fn scrub(text: &str) -> (String, usize) {
    let lowered = text.to_ascii_lowercase();
    if SECRET_MARKERS.iter().any(|marker| lowered.contains(marker)) {
        return (REDACTED_LINE.to_string(), 1);
    }
    let mut replaced = 0;
    let mut out = String::with_capacity(text.len());
    for (index, token) in text.split(' ').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let trimmed = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if is_mac_token(trimmed) {
            out.push_str(&token.replace(trimmed, REDACTED_MAC));
            replaced += 1;
        } else {
            out.push_str(token);
        }
    }
    if out.len() > MAX_TEXT_BYTES {
        let mut cut = MAX_TEXT_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("…[cut]");
    }
    (out, replaced)
}

fn is_snapshot_secret(name: &str) -> bool {
    SNAPSHOT_SECRET_FIELDS.contains(&name)
}

/// Whether a dynamic key is one the store will carry: printable, bounded,
/// and not a secret field name.
fn key_allowed(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_LEN
        && !is_snapshot_secret(key)
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '@' | '/'))
}

fn apply(rule: &Rule, value: Value, stats: &mut RedactionStats) -> Option<Value> {
    if value.is_null() {
        return Some(Value::Null);
    }
    match rule {
        Rule::Scalar => match value {
            Value::String(text) => {
                let (text, replaced) = scrub(&text);
                stats.redacted_fields += replaced;
                Some(Value::String(text))
            }
            Value::Bool(_) | Value::Number(_) => Some(value),
            Value::Object(_) | Value::Array(_) | Value::Null => {
                stats.dropped_fields += 1;
                None
            }
        },
        Rule::Redact => {
            stats.redacted_fields += 1;
            Some(Value::String(redact::REDACTED.to_string()))
        }
        Rule::Object(fields) => {
            let Value::Object(members) = value else {
                stats.dropped_fields += 1;
                return None;
            };
            let mut out = Map::new();
            for (key, member) in members {
                if is_snapshot_secret(&key) {
                    stats.redacted_fields += 1;
                    out.insert(key, Value::String(redact::REDACTED.to_string()));
                    continue;
                }
                let rule = match key.as_str() {
                    "available" => Some(&S),
                    "detail" => Some(&S),
                    other => fields
                        .iter()
                        .find(|(name, _)| *name == other)
                        .map(|(_, rule)| rule),
                };
                match rule {
                    Some(rule) => {
                        if let Some(kept) = apply(rule, member, stats) {
                            out.insert(key, kept);
                        }
                    }
                    None => stats.dropped_fields += 1,
                }
            }
            Some(Value::Object(out))
        }
        Rule::Map(rule) => {
            let Value::Object(members) = value else {
                stats.dropped_fields += 1;
                return None;
            };
            let mut out = Map::new();
            for (key, member) in members {
                if !key_allowed(&key) {
                    stats.dropped_fields += 1;
                    continue;
                }
                if let Some(kept) = apply(rule, member, stats) {
                    out.insert(key, kept);
                }
            }
            Some(Value::Object(out))
        }
        Rule::Array(rule) => {
            let Value::Array(items) = value else {
                stats.dropped_fields += 1;
                return None;
            };
            Some(Value::Array(
                items
                    .into_iter()
                    .filter_map(|item| apply(rule, item, stats))
                    .collect(),
            ))
        }
    }
}

/// `snapshot` with the denylist applied, then the allowlist, then
/// [`scrub`] on every string. Fails closed: what the schema does not name
/// does not come back.
#[must_use]
pub fn redact_snapshot(snapshot: Value) -> (Value, RedactionStats) {
    let mut stats = RedactionStats::default();
    let denied = redact::redact(snapshot, "");
    let kept = apply(&schema(), denied, &mut stats).unwrap_or(Value::Object(Map::new()));
    (kept, stats)
}

// ---- collection ---------------------------------------------------------

/// How one section's read ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionStatus {
    /// The source answered.
    Ok,
    /// The source answered with an error; the member says which.
    Unavailable,
    /// The source did not answer within its bound, or the deadline had
    /// already passed when its turn came.
    Timeout,
}

impl SectionStatus {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
        }
    }
}

/// What one collection did.
#[derive(Debug, Clone)]
pub struct CollectionReport {
    /// Wall time the whole collection took.
    pub elapsed: Duration,
    /// Per source, how the read ended.
    pub sections: BTreeMap<&'static str, SectionStatus>,
    /// The redaction pass's counts.
    pub redaction: RedactionStats,
}

/// A produced snapshot: the redacted value and the report.
#[derive(Debug, Clone)]
pub struct Collected {
    /// The redacted, versioned snapshot.
    pub snapshot: Value,
    /// How the collection went.
    pub report: CollectionReport,
}

/// Assembles a snapshot from the mosd surfaces.
pub struct Collector<'a> {
    api: &'a dyn SettingsApi,
    deadline: Duration,
    section_timeout: Duration,
}

fn absent(detail: impl Into<String>) -> Value {
    json!({ "available": false, "detail": detail.into() })
}

/// `section[key]`, or an absent member explaining that the section itself
/// was not collected.
fn pick(section: &Value, key: &str) -> Value {
    match section.get(key) {
        Some(member) => member.clone(),
        None => {
            let detail = section
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("the source did not carry this member");
            absent(format!("not collected: {detail}"))
        }
    }
}

/// The tasks in the live-state history whose outcome was not success.
fn failed_tasks(state: &Value) -> Value {
    let Some(tasks) = state.get("tasks").and_then(Value::as_array) else {
        return absent("the live-state tree carries no task history");
    };
    let failed: Vec<Value> = tasks
        .iter()
        .filter(|task| {
            task.get("outcome")
                .and_then(Value::as_str)
                .is_some_and(|outcome| outcome != "succeeded")
        })
        .take(MAX_FAILED_TASKS)
        .cloned()
        .collect();
    Value::Array(failed)
}

impl<'a> Collector<'a> {
    /// A collector over `api` on the shipped bounds.
    #[must_use]
    pub fn new(api: &'a dyn SettingsApi) -> Self {
        Self {
            api,
            deadline: COLLECTION_DEADLINE,
            section_timeout: SECTION_TIMEOUT,
        }
    }

    /// Override both bounds, for the tests that prove they are enforced.
    #[cfg(test)]
    #[must_use]
    pub fn with_bounds(mut self, deadline: Duration, section_timeout: Duration) -> Self {
        self.deadline = deadline;
        self.section_timeout = section_timeout;
        self
    }

    /// One section's read under the remaining deadline and the section
    /// timeout, whichever is shorter.
    async fn section<F>(&self, started: Instant, read: F) -> (Value, SectionStatus)
    where
        F: Future<Output = anyhow::Result<Value>>,
    {
        let remaining = self.deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return (
                absent(format!(
                    "the collection deadline of {:?} passed before this source was read",
                    self.deadline
                )),
                SectionStatus::Timeout,
            );
        }
        let bound = remaining.min(self.section_timeout);
        match tokio::time::timeout(bound, read).await {
            Ok(Ok(value)) => (value, SectionStatus::Ok),
            Ok(Err(err)) => (absent(format!("{err:#}")), SectionStatus::Unavailable),
            Err(_) => (
                absent(format!("no answer within {bound:?}")),
                SectionStatus::Timeout,
            ),
        }
    }

    /// Collect, assemble and redact one snapshot. Never fails: a source that
    /// does not answer is an absent member, and the snapshot is produced
    /// within the deadline whatever the sources do.
    pub async fn collect(&self) -> Collected {
        let started = Instant::now();
        let mut sections = BTreeMap::new();
        let (system, status) = self.section(started, self.api.get_system_info()).await;
        sections.insert("system", status);
        let (telemetry, status) = self.section(started, self.api.get_telemetry()).await;
        sections.insert("telemetry", status);
        let (failures, status) = self.section(started, self.api.get_failure_evidence()).await;
        sections.insert("failures", status);
        let (storage, status) = self.section(started, self.api.get_storage_status()).await;
        sections.insert("storage", status);
        let (time, status) = self.section(started, self.api.get_time_status()).await;
        sections.insert("time", status);
        let (network, status) = self.section(started, self.api.get_observed_network()).await;
        sections.insert("network", status);
        let (state, status) = self.section(started, self.api.get_state("")).await;
        sections.insert("state", status);

        let raw = json!({
            "schemaVersion": SCHEMA_VERSION,
            // Wall-clock time as this appliance has it; the `time` member says
            // whether that clock is disciplined, and `boot.uptime` is the
            // monotonic reference.
            "collectedAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "release": {
                "board": pick(&system, "board"),
                "release": pick(&system, "release"),
                "kernel": pick(&system, "kernel"),
            },
            "system": system,
            "boot": {
                "slot": pick(&system, "slot"),
                "uptime": pick(&system, "uptime"),
                "reset": pick(&telemetry, "reset"),
                "update": pick(&state, "update"),
            },
            "journal": pick(&failures, "journal"),
            "failures": {
                "units": pick(&failures, "units"),
                "tasks": failed_tasks(&state),
                "health": pick(&state, "health"),
            },
            "storage": storage,
            "time": time,
            "telemetry": {
                "thermal": pick(&telemetry, "thermal"),
                "watchdog": pick(&telemetry, "watchdog"),
            },
            "network": network,
        });
        let (mut snapshot, redaction) = redact_snapshot(raw);
        let elapsed = started.elapsed();
        // The collection record is metadata this module wrote itself; it is
        // added after the pass so the schema above names only evidence.
        if let Some(root) = snapshot.as_object_mut() {
            root.insert(
                "collection".to_string(),
                json!({
                    "deadlineSeconds": self.deadline.as_secs_f64(),
                    "sectionTimeoutSeconds": self.section_timeout.as_secs_f64(),
                    "elapsedMillis": elapsed.as_millis(),
                    "sections": sections
                        .iter()
                        .map(|(name, status)| ((*name).to_string(), Value::from(status.as_str())))
                        .collect::<Map<String, Value>>(),
                    "redaction": {
                        "schemaVersion": REDACTION_SCHEMA_VERSION,
                        "droppedFields": redaction.dropped_fields,
                        "redactedFields": redaction.redacted_fields,
                    },
                }),
            );
        }
        Collected {
            snapshot,
            report: CollectionReport {
                elapsed,
                sections,
                redaction,
            },
        }
    }
}

// ---- the store ------------------------------------------------------------

/// One stored snapshot, as the list route describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSummary {
    /// The store's sequence number, and the id in the routes.
    pub id: u64,
    /// The snapshot's size on disk.
    pub bytes: u64,
    /// The snapshot's `collectedAt`, when it carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collected_at: Option<String>,
    /// The snapshot's `schemaVersion`, when it carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u64>,
    /// The snapshot's machine id, when it carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
}

/// The bounded on-disk store.
///
/// Layout, all of it under one root (`/mos/diagnostics` on a device, a
/// temporary directory in tests):
///
/// ```text
/// <root>/<id>.json            a published snapshot
/// <root>/.staging-<id>.json   one being written; never listed
/// ```
///
/// Absence of the root is a defined state — the shipped state of every
/// device — and it is created on the first publish, never at start-up.
pub struct SnapshotStore {
    root: PathBuf,
    max_snapshots: usize,
    max_total_bytes: u64,
}

impl SnapshotStore {
    /// A store rooted at `root`, on the shipped caps.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_snapshots: MAX_SNAPSHOTS,
            max_total_bytes: MAX_TOTAL_BYTES,
        }
    }

    /// The store at [`DEFAULT_ROOT`]. A path and no syscall.
    #[must_use]
    pub fn at_default() -> Self {
        Self::new(DEFAULT_ROOT)
    }

    /// Override the retention caps, for the tests that prove them.
    #[cfg(test)]
    #[must_use]
    pub fn with_caps(mut self, max_snapshots: usize, max_total_bytes: u64) -> Self {
        self.max_snapshots = max_snapshots;
        self.max_total_bytes = max_total_bytes;
        self
    }

    /// Where the store lives, for the tests that look at the directory.
    #[cfg(test)]
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_of(&self, id: u64) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    /// Every published snapshot, oldest first.
    pub fn list(&self) -> anyhow::Result<Vec<SnapshotSummary>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("read {}", self.root.display()));
            }
        };
        let mut summaries = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("read {}", self.root.display()))?;
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|stem| stem.parse::<u64>().ok())
            else {
                continue;
            };
            let metadata = entry.metadata()?;
            if !metadata.is_file() {
                continue;
            }
            let (collected_at, schema_version, machine_id) = fs::read(entry.path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .map(|value| {
                    (
                        value
                            .get("collectedAt")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        value.get("schemaVersion").and_then(Value::as_u64),
                        value
                            .pointer("/system/machineId/id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    )
                })
                .unwrap_or_default();
            summaries.push(SnapshotSummary {
                id,
                bytes: metadata.len(),
                collected_at,
                schema_version,
                machine_id,
            });
        }
        summaries.sort_by_key(|summary| summary.id);
        Ok(summaries)
    }

    /// The bytes of snapshot `id`, or `None` when there is no such snapshot.
    pub fn read(&self, id: u64) -> anyhow::Result<Option<Vec<u8>>> {
        match fs::read(self.path_of(id)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("read snapshot {id}")),
        }
    }

    /// Remove snapshot `id`; `false` when there was none.
    pub fn delete(&self, id: u64) -> anyhow::Result<bool> {
        match fs::remove_file(self.path_of(id)) {
            Ok(()) => {
                sync_dir(&self.root)?;
                Ok(true)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err).with_context(|| format!("delete snapshot {id}")),
        }
    }

    /// Publish `snapshot`: refuse it above [`MAX_SNAPSHOT_BYTES`], make room
    /// under the caps by removing the oldest, write it to a staging file,
    /// fsync, rename into place, fsync the directory.
    ///
    /// Whole or not at all: until the rename the store lists nothing new,
    /// and a failure anywhere leaves at most a staging file the next publish
    /// sweeps.
    pub fn publish(&self, snapshot: &Value) -> anyhow::Result<SnapshotSummary> {
        let bytes = serde_json::to_vec(snapshot).context("encode snapshot")?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            bail!(
                "the snapshot is {} bytes, above the {MAX_SNAPSHOT_BYTES} byte cap; nothing was written",
                bytes.len()
            );
        }
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        self.sweep_staging();
        let existing = self.list()?;
        let id = existing.iter().map(|summary| summary.id).max().unwrap_or(0) + 1;
        let mut count = existing.len();
        let mut total: u64 = existing.iter().map(|summary| summary.bytes).sum();
        for old in &existing {
            if count < self.max_snapshots && total + bytes.len() as u64 <= self.max_total_bytes {
                break;
            }
            fs::remove_file(self.path_of(old.id))
                .with_context(|| format!("evict snapshot {}", old.id))?;
            count -= 1;
            total -= old.bytes;
        }
        let staging = self.root.join(format!(".staging-{id}.json"));
        let target = self.path_of(id);
        let written = (|| {
            let mut file = File::create(&staging)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&staging, &target)?;
            sync_dir(&self.root)
        })();
        if let Err(err) = written {
            let _ = fs::remove_file(&staging);
            return Err(err).with_context(|| format!("publish snapshot {id}"));
        }
        Ok(SnapshotSummary {
            id,
            bytes: bytes.len() as u64,
            collected_at: snapshot
                .get("collectedAt")
                .and_then(Value::as_str)
                .map(str::to_string),
            schema_version: snapshot.get("schemaVersion").and_then(Value::as_u64),
            machine_id: snapshot
                .pointer("/system/machineId/id")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    /// Remove staging files a crashed publish left behind.
    fn sweep_staging(&self) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".staging-"))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_api::FakeSettings;

    #[test]
    fn a_secret_marker_replaces_the_whole_string() {
        for text in [
            "wpa_supplicant[420]: psk=deadbeef",
            "Set PASSWORD for root",
            "Authorization: Bearer abc.def",
            "-----BEGIN PRIVATE KEY-----",
            "apid: token minted",
            "private_key=/mnt/state/wg0.key",
        ] {
            assert_eq!(scrub(text), (REDACTED_LINE.to_string(), 1), "{text}");
        }
    }

    #[test]
    fn a_hardware_address_is_replaced_and_an_ip_is_not() {
        let (text, replaced) = scrub(
            "eth0: link 02:42:ac:11:00:02 (02:42:AC:11:00:02), gw 192.0.2.1 fe80::42:acff:fe11:2",
        );
        assert_eq!(
            text,
            "eth0: link <mac> (<mac>), gw 192.0.2.1 fe80::42:acff:fe11:2"
        );
        assert_eq!(replaced, 2);
        assert_eq!(scrub("plain text").0, "plain text");
        assert_eq!(
            scrub("2026-09-02T00:00:00+00:00 ok").0,
            "2026-09-02T00:00:00+00:00 ok"
        );
    }

    #[test]
    fn a_long_string_is_cut_on_a_character_boundary() {
        let long = "€".repeat(MAX_TEXT_BYTES);
        let (text, _) = scrub(&long);
        assert!(text.len() <= MAX_TEXT_BYTES + "…[cut]".len());
        assert!(text.ends_with("…[cut]"));
    }

    /// A snapshot carrying every documented benign member, with a secret
    /// planted at every kind of place a secret could land.
    fn fixture() -> Value {
        json!({
            "schemaVersion": 2,
            "collectedAt": "2026-09-02T00:00:00Z",
            "release": {
                "board": { "available": true, "model": "Vendor CX3576", "source": "devicetree" },
                "release": { "available": true, "id": "debian", "prettyName": "Debian 13" },
                "kernel": { "available": true, "release": "6.1.115-mos", "version": "#1 SMP" },
            },
            "system": {
                "machineId": { "available": true, "id": "0123456789abcdef0123456789abcdef" },
                "board": { "available": true, "model": "Vendor CX3576", "source": "devicetree" },
                "kernel": { "available": true, "release": "6.1.115-mos", "version": "#1 SMP" },
                "release": { "available": true, "id": "debian" },
                "system": {
                    "available": true, "version": "0.1.0+git00b674ec0ffe-1", "package": "mosd",
                    "gitStamp": { "available": true, "commit": "00b674ec0ffe", "dirty": false, "revision": "1", "consistent": true, "stamps": ["git00b674ec0ffe-1"] },
                    "commitDate": { "available": true, "date": "2026-09-02T00:00:00Z" },
                    "fileEpoch": { "available": true, "epoch": 1577836800, "date": "2020-01-01T00:00:00Z" },
                },
                "daemon": { "name": "mosd", "version": "0.1.0", "commit": "00b674ec0ffe" },
                "packages": { "available": true, "count": 2, "mosCount": 1, "malformedRows": 0, "truncated": false,
                    "entries": [{ "name": "mosd", "version": "0.1.0+git00b674ec0ffe-1", "architecture": "arm64", "mos": true },
                                { "name": "systemd", "version": "257.7-1", "architecture": "arm64", "mos": false }] },
                "slot": { "available": true, "booted": "rootfs.0", "bootname": "A", "primary": "rootfs.0" },
                "uptime": { "available": true, "seconds": 4242 },
                // Unclassified: a member no schema names must not ship.
                "wifiPassphraseCache": "hunter2-marker",
            },
            "boot": {
                "slot": { "available": true, "booted": "rootfs.0" },
                "uptime": { "available": true, "seconds": 4242 },
                "reset": { "available": true, "reason": "watchdog", "detail": "d",
                    "evidence": { "watchdogBootstatus": [{ "device": "watchdog0", "flags": ["cardReset"] }], "pstore": { "available": false, "detail": "no pstore" } } },
                "update": { "operation": "idle", "last_error": "", "progress": { "percentage": 0, "message": "", "depth": 0 },
                    "slots": { "rootfs.0": { "state": "booted", "bootname": "A", "boot_status": "good", "bundle_version": "0.1.0" },
                               "rootfs.1": { "state": "inactive" } },
                    "booted_slot": "rootfs.0", "primary": "rootfs.0", "pending_not_confirmed": false,
                    "install": { "status": "done", "bundle": "/mos/updates/x.raucb", "requested_by": ":1.7" },
                    "last_mark": { "state": "good", "slot": "booted", "message": "ok", "requested_by": ":1.7" } },
            },
            "journal": { "available": true, "scope": "current boot", "priority": "warning", "lineCount": 4, "sourceLines": 4, "sourceBytes": 100, "truncated": false,
                "bounds": { "maxLines": 400, "maxBytes": 131072, "maxLineBytes": 1024 },
                "lines": [
                    "2026-09-02T00:00:00+0000 systemd[1]: Failed to start x.service.",
                    "2026-09-02T00:00:01+0000 wpa_supplicant[42]: wlan0: psk=cafebabe-marker",
                    "2026-09-02T00:00:02+0000 networkd[7]: eth0: link 02:42:ac:11:00:02 up",
                    "2026-09-02T00:00:03+0000 sshd[9]: -----BEGIN OPENSSH PRIVATE KEY----- pemmarker",
                ] },
            "failures": {
                "units": { "available": true, "count": 1, "truncated": false,
                    "entries": [{ "name": "x.service", "description": "X", "loadState": "loaded", "activeState": "failed", "subState": "failed" }] },
                "tasks": [{ "id": "t-1", "operation": "settings-write", "dotPath": "network", "source": "api", "status": "finished",
                            "enqueuedAt": "2026-09-02T00:00:00Z", "outcome": "failed", "message": "render failed", "foldedCount": 0 }],
                "health": { "var": { "status": "degraded", "detail": "/var at 91%" },
                            "psk": { "status": "should-be-redacted-marker", "detail": "x" } },
            },
            "storage": {
                "tiers": [{ "name": "data", "role": "ext4", "partitionLabel": "data", "present": true, "device": "/dev/mmcblk0p11", "mounted": true, "mount": "/mnt/data", "filesystem": "ext4", "readOnly": false,
                            "space": { "totalBytes": 1000, "usedBytes": 850, "freeBytes": 100, "reservedBytes": 50, "usedPercent": 85 }, "pressure": "warning",
                            "updateWorkspace": { "root": "/mos/updates", "reservedBytes": 268435456, "available": false },
                            "check": { "recorded": true, "unit": "systemd-fsck@dev-mmcblk0p11.service", "activeState": "inactive", "result": "success", "exitStatus": 0 } }],
                "namespaces": { "sharedCapacityTier": "data", "detail": "one pool",
                    "binds": [{ "name": "mos", "mount": "/mos", "source": "/mnt/data/mos", "owner": "system", "readiness": "ready", "mounted": true, "device": "/dev/mmcblk0p11", "readOnly": false, "sourceIsDirectory": true, "probe": { "attempted": true, "passed": true } }] },
                "media": [{ "name": "mmcblk0", "kind": "emmc", "sizeBytes": 32000000000u64, "model": "DG4032", "rotational": false,
                            "health": { "supported": true, "source": "sysfs", "raw": { "lifeTime": "0x01 0x01", "preEolInfo": "0x01" }, "lifetimeEstimates": [{ "raw": "0x01", "usedPercentMin": 0, "usedPercentMax": 10 }], "preEol": "normal" } }],
                "policy": { "warningPercent": 80, "warningClearPercent": 75, "criticalPercent": 90, "criticalClearPercent": 85, "updateWorkspaceReservedBytes": 268435456, "updateWorkspaceRoot": "/mos/updates", "watchedTiers": ["data", "state"] },
                "lifecycle": { "encryption": "unsupported", "factoryReset": "unsupported" },
            },
            "time": { "status": "synchronized", "synchronized": true, "server": { "name": "0.pool.ntp.org", "address": "192.0.2.7" },
                      "sample": { "leap": 0, "stratum": 2, "spike": false, "offsetSeconds": 0.012, "packetCount": 7, "correction": "slew" } },
            "telemetry": {
                "thermal": { "available": true, "zones": [{ "sensor": "thermal_zone0", "label": "soc-thermal", "milliCelsius": 48250 }], "hwmon": [] },
                "watchdog": { "available": true, "devices": [{ "device": "watchdog0", "identity": "dw_wdt", "state": "active", "timeoutSeconds": 30, "bootstatus": { "available": true, "raw": 32, "flags": ["cardReset"] }, "nowayout": true }] },
            },
            "network": {
                "interfaces": { "available": true, "count": 2, "entries": [
                    { "name": "eth0", "index": 2, "type": "ether", "driver": "stmmac", "mtu": 1500,
                      "link": { "administrativeState": "configured", "operationalState": "routable", "carrierState": "carrier", "carrier": true, "onlineState": "online", "addressState": "routable" },
                      "hardwareAddress": "02:42:ac:11:00:02",
                      "addresses": [{ "family": "ipv4", "address": "192.0.2.10", "prefixLength": 24, "scope": "global", "configSource": "DHCPv4" }],
                      "dhcp": { "available": true, "inferred": false, "state": "bound", "lease": { "address": "192.0.2.10", "prefixLength": 24, "server": "192.0.2.1", "router": "192.0.2.1", "lifetimeSeconds": 86400 } },
                      "dns": ["192.0.2.1"] },
                    { "name": "wlan0", "index": 3, "type": "wlan", "link": { "carrierState": "carrier", "carrier": true },
                      "hardwareAddress": "aa:bb:cc:dd:ee:01", "addresses": [], "dhcp": { "available": false, "detail": "none" }, "dns": [],
                      "wifi": { "interface": "wlan0", "available": true, "state": "COMPLETED", "associated": true, "ssid": "HomeNet-marker", "bssid": "aa:bb:cc:dd:ee:ff", "frequencyMhz": 5180, "keyManagement": "WPA2-PSK", "rssiDbm": -51, "linkSpeedMbps": 433,
                                "psk": "wifi-psk-marker" } },
                ] },
                "defaultRoutes": { "available": true, "count": 1, "entries": [{ "family": "ipv4", "gateway": "192.0.2.1", "interface": "eth0", "interfaceIndex": 2, "metric": 1024, "protocol": "dhcp", "table": "main", "configSource": "DHCPv4" }] },
                "dns": { "available": true, "linkServers": ["192.0.2.1"], "resolverServers": ["192.0.2.1"], "probe": { "available": true, "name": "0.debian.pool.ntp.org", "reachable": true, "result": "resolved", "detail": "4 address(es)" } },
                "wifi": { "available": true, "associations": [{ "interface": "wlan0", "available": true, "state": "COMPLETED", "associated": true, "ssid": "HomeNet-marker", "bssid": "aa:bb:cc:dd:ee:ff" }] },
                "capabilities": { "wifi": { "supported": true, "interfaces": ["wlan0"], "detail": "d" }, "bluetooth": { "supported": false, "adapters": [], "detail": "d" }, "cellular": { "supported": false, "interfaces": [], "detail": "d" } },
            },
            // Secrets under the names the live routes already deny, and
            // under names the snapshot denies, at the top level and nested.
            "access": { "webAdmin": { "password_hash": "hash-marker" }, "apiTokens": [{ "hash": "tokenhash-marker" }] },
            "privateKey": "wg-private-marker",
            "token": "bearer-marker",
            "registryAuth": { "auth": "registry-marker" },
        })
    }

    /// The negative fixture: every planted secret is absent from the
    /// produced bytes, whichever way it was planted.
    #[test]
    fn every_planted_secret_is_absent_from_the_produced_snapshot() {
        let (redacted, stats) = redact_snapshot(fixture());
        let text = redacted.to_string();
        for marker in [
            "hunter2-marker",
            "cafebabe-marker",
            "pemmarker",
            "should-be-redacted-marker",
            "HomeNet-marker",
            "aa:bb:cc:dd:ee:ff",
            "aa:bb:cc:dd:ee:01",
            "02:42:ac:11:00:02",
            "wifi-psk-marker",
            "hash-marker",
            "tokenhash-marker",
            "wg-private-marker",
            "bearer-marker",
            "registry-marker",
            "wifiPassphraseCache",
            "registryAuth",
        ] {
            assert!(!text.contains(marker), "{marker} shipped:\n{text}");
        }
        assert!(stats.dropped_fields > 0);
        assert!(stats.redacted_fields > 0);
        // The identifying network fields are present as the sentinel, so a
        // reader can tell "redacted" from "the device had none".
        assert_eq!(
            redacted["network"]["interfaces"]["entries"][0]["hardwareAddress"],
            redact::REDACTED
        );
        assert_eq!(
            redacted["network"]["interfaces"]["entries"][1]["wifi"]["ssid"],
            redact::REDACTED
        );
        assert_eq!(
            redacted["network"]["wifi"]["associations"][0]["bssid"],
            redact::REDACTED
        );
        // The journal line carrying a PSK is gone whole; the one carrying a
        // MAC keeps its message with the MAC replaced.
        assert_eq!(redacted["journal"]["lines"][1], REDACTED_LINE);
        assert_eq!(redacted["journal"]["lines"][3], REDACTED_LINE);
        assert_eq!(
            redacted["journal"]["lines"][2],
            "2026-09-02T00:00:02+0000 networkd[7]: eth0: link <mac> up"
        );
        // A dynamic key that is itself a secret name is dropped with its
        // value; the benign neighbour stays.
        assert!(redacted["failures"]["health"].get("psk").is_none());
        assert_eq!(redacted["failures"]["health"]["var"]["status"], "degraded");
    }

    /// The positive fixture: the benign evidence the contract keeps is all
    /// present after the pass, so the allowlist is not passing by dropping
    /// everything.
    #[test]
    fn every_benign_member_survives_the_pass() {
        let (redacted, _) = redact_snapshot(fixture());
        for (pointer, expected) in [
            ("/schemaVersion", json!(2)),
            ("/collectedAt", json!("2026-09-02T00:00:00Z")),
            ("/release/board/model", json!("Vendor CX3576")),
            ("/release/kernel/release", json!("6.1.115-mos")),
            (
                "/system/machineId/id",
                json!("0123456789abcdef0123456789abcdef"),
            ),
            ("/system/system/gitStamp/commit", json!("00b674ec0ffe")),
            (
                "/system/system/commitDate/date",
                json!("2026-09-02T00:00:00Z"),
            ),
            ("/system/system/fileEpoch/epoch", json!(1_577_836_800)),
            (
                "/system/packages/entries/0/version",
                json!("0.1.0+git00b674ec0ffe-1"),
            ),
            ("/system/slot/booted", json!("rootfs.0")),
            ("/boot/reset/reason", json!("watchdog")),
            (
                "/boot/reset/evidence/watchdogBootstatus/0/flags/0",
                json!("cardReset"),
            ),
            ("/boot/update/slots/rootfs.0/boot_status", json!("good")),
            ("/boot/update/install/bundle", json!("/mos/updates/x.raucb")),
            (
                "/journal/lines/0",
                json!("2026-09-02T00:00:00+0000 systemd[1]: Failed to start x.service."),
            ),
            ("/journal/bounds/maxLines", json!(400)),
            ("/failures/units/entries/0/name", json!("x.service")),
            ("/failures/tasks/0/outcome", json!("failed")),
            ("/failures/tasks/0/message", json!("render failed")),
            ("/failures/health/var/status", json!("degraded")),
            ("/failures/health/var/detail", json!("/var at 91%")),
            ("/storage/tiers/0/space/usedPercent", json!(85)),
            (
                "/storage/tiers/0/check/unit",
                json!("systemd-fsck@dev-mmcblk0p11.service"),
            ),
            ("/storage/namespaces/binds/0/probe/passed", json!(true)),
            (
                "/storage/media/0/health/lifetimeEstimates/0/usedPercentMax",
                json!(10),
            ),
            ("/storage/policy/watchedTiers/1", json!("state")),
            ("/storage/lifecycle/encryption", json!("unsupported")),
            ("/time/status", json!("synchronized")),
            ("/time/sample/correction", json!("slew")),
            ("/telemetry/thermal/zones/0/milliCelsius", json!(48250)),
            ("/telemetry/watchdog/devices/0/bootstatus/raw", json!(32)),
            ("/network/interfaces/entries/0/link/carrier", json!(true)),
            (
                "/network/interfaces/entries/0/addresses/0/address",
                json!("192.0.2.10"),
            ),
            (
                "/network/interfaces/entries/0/dhcp/lease/server",
                json!("192.0.2.1"),
            ),
            ("/network/interfaces/entries/0/dns/0", json!("192.0.2.1")),
            ("/network/interfaces/entries/1/wifi/associated", json!(true)),
            ("/network/interfaces/entries/1/wifi/rssiDbm", json!(-51)),
            (
                "/network/defaultRoutes/entries/0/gateway",
                json!("192.0.2.1"),
            ),
            ("/network/dns/probe/result", json!("resolved")),
            ("/network/dns/probe/detail", json!("4 address(es)")),
            ("/network/capabilities/cellular/supported", json!(false)),
        ] {
            assert_eq!(
                redacted.pointer(pointer),
                Some(&expected),
                "{pointer} did not survive: {}",
                redacted
            );
        }
    }

    /// The fail-closed rule stated as a rule: an object under a scalar rule,
    /// a member of the wrong shape, and a whole unnamed section all go.
    #[test]
    fn what_the_schema_does_not_name_does_not_ship() {
        let (redacted, stats) = redact_snapshot(json!({
            "schemaVersion": { "nested": "object under a scalar rule" },
            "time": { "status": "synchronized", "server": "a string under an object rule" },
            "unnamedSection": { "anything": 1 },
            "journal": { "lines": "not an array" },
        }));
        assert_eq!(
            redacted,
            json!({ "time": { "status": "synchronized" }, "journal": { } })
        );
        assert_eq!(stats.dropped_fields, 4);
    }

    /// The store: publish is atomic and listed by id, read returns the
    /// bytes, delete is explicit and idempotent, and staging is never listed.
    #[test]
    fn the_store_publishes_lists_reads_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("diagnostics"));
        assert!(
            store.list().unwrap().is_empty(),
            "an absent root is an empty store"
        );

        let first = store
            .publish(&json!({ "schemaVersion": 1, "collectedAt": "2026-09-02T00:00:00Z", "system": { "machineId": { "id": "0123456789abcdef0123456789abcdef" } } }))
            .unwrap();
        assert_eq!(first.id, 1);
        assert_eq!(first.schema_version, Some(1));
        assert_eq!(
            first.machine_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        let second = store.publish(&json!({ "schemaVersion": 1 })).unwrap();
        assert_eq!(second.id, 2);

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, 1);
        assert_eq!(
            listed[0].collected_at.as_deref(),
            Some("2026-09-02T00:00:00Z")
        );
        assert_eq!(listed[1].id, 2);
        assert!(listed[1].collected_at.is_none());
        // Nothing but published snapshots is on disk: no staging file.
        let names: Vec<String> = fs::read_dir(store.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            names.iter().all(|name| !name.starts_with(".staging")),
            "{names:?}"
        );

        let bytes = store.read(1).unwrap().expect("snapshot 1");
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["schemaVersion"], 1);
        assert!(store.read(9).unwrap().is_none());

        assert!(store.delete(1).unwrap());
        assert!(!store.delete(1).unwrap());
        assert_eq!(store.list().unwrap().len(), 1);
        // A stray file that is not a snapshot is ignored, and a stale
        // staging file is swept by the next publish.
        fs::write(store.root().join("notes.txt"), "x").unwrap();
        fs::write(store.root().join(".staging-7.json"), "{").unwrap();
        let third = store.publish(&json!({ "schemaVersion": 1 })).unwrap();
        assert_eq!(third.id, 3);
        assert!(!store.root().join(".staging-7.json").exists());
        assert_eq!(store.list().unwrap().len(), 2);
    }

    /// The count cap: the oldest goes when one more arrives.
    #[test]
    fn the_count_cap_evicts_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path()).with_caps(3, MAX_TOTAL_BYTES);
        for _ in 0..5 {
            store.publish(&json!({ "schemaVersion": 1 })).unwrap();
        }
        let ids: Vec<u64> = store
            .list()
            .unwrap()
            .iter()
            .map(|summary| summary.id)
            .collect();
        assert_eq!(ids, vec![3, 4, 5]);
    }

    /// The byte cap: the oldest go until the newcomer fits.
    #[test]
    fn the_byte_cap_evicts_until_the_newcomer_fits() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path()).with_caps(100, 1000);
        let filler = |n: usize| json!({ "schemaVersion": 1, "pad": "x".repeat(n) });
        store.publish(&filler(300)).unwrap();
        store.publish(&filler(300)).unwrap();
        store.publish(&filler(300)).unwrap();
        // Three of ~330 bytes fit under 1000; a fourth does not, so the
        // oldest goes.
        store.publish(&filler(300)).unwrap();
        let listed = store.list().unwrap();
        let ids: Vec<u64> = listed.iter().map(|summary| summary.id).collect();
        assert_eq!(ids, vec![2, 3, 4]);
        let total: u64 = listed.iter().map(|summary| summary.bytes).sum();
        assert!(total <= 1000, "{total}");
    }

    /// The per-snapshot cap: an oversized snapshot is refused, and nothing
    /// is written — not a staging file, not a truncated snapshot.
    #[test]
    fn an_oversized_snapshot_is_refused_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("diagnostics"));
        let huge = json!({ "schemaVersion": 1, "pad": "x".repeat(MAX_SNAPSHOT_BYTES + 1) });
        let err = store.publish(&huge).expect_err("refused");
        assert!(err.to_string().contains("above the"), "{err}");
        assert!(!store.root().exists() || store.list().unwrap().is_empty());
    }

    /// A publish that cannot rename leaves nothing new listed: the store
    /// root is a file, so the write fails, and the error names the id.
    #[test]
    fn a_failed_publish_leaves_nothing_published() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, "x").unwrap();
        let store = SnapshotStore::new(&file);
        assert!(store.publish(&json!({ "schemaVersion": 1 })).is_err());
        assert!(store.list().is_err() || store.list().unwrap().is_empty());
    }

    /// The collection over a fake that answers: every section ok, the
    /// snapshot versioned, redacted and carrying the collection record.
    #[tokio::test]
    async fn a_collection_over_answering_sources_is_complete() {
        let fake = FakeSettings::new(json!({ "hostname": "mos", "network": {}, "access": {} }));
        fake.set_state_entry(
            "update",
            json!({ "operation": "idle", "booted_slot": "rootfs.0" }),
        );
        fake.set_state_entry("health", json!({ "var": { "status": "ok", "detail": "" } }));
        let collected = Collector::new(&fake).collect().await;
        let snapshot = &collected.snapshot;
        assert_eq!(snapshot["schemaVersion"], SCHEMA_VERSION);
        assert!(
            snapshot["collectedAt"]
                .as_str()
                .is_some_and(|t| t.ends_with('Z'))
        );
        assert_eq!(
            snapshot["system"]["machineId"]["id"],
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(snapshot["boot"]["update"]["booted_slot"], "rootfs.0");
        assert_eq!(snapshot["boot"]["uptime"]["seconds"], 7);
        assert_eq!(snapshot["failures"]["health"]["var"]["status"], "ok");
        assert_eq!(snapshot["failures"]["tasks"], json!([]));
        assert_eq!(snapshot["time"]["status"], "synchronized");
        assert_eq!(
            snapshot["collection"]["redaction"]["schemaVersion"],
            REDACTION_SCHEMA_VERSION
        );
        for (name, status) in &collected.report.sections {
            assert_eq!(*status, SectionStatus::Ok, "{name}");
            assert_eq!(snapshot["collection"]["sections"][*name], "ok");
        }
        assert_eq!(collected.report.sections.len(), 7);
    }

    /// The version names THIS shape, and the number is a literal rather than
    /// the constant it came from.
    ///
    /// Under version 1 `system.system` carried `buildEpoch`, `buildDate` and
    /// `buildDateDetail`. 7d759112 replaced them with the `commitDate` and
    /// `fileEpoch` objects and left the version at 1, so two documents of
    /// different shapes both claimed it -- the one thing a schema version
    /// exists to make impossible. Version 2 names the shape asserted below.
    ///
    /// Asserting against [`SCHEMA_VERSION`] cannot catch that: it puts the
    /// same value on both sides. The number is written out here beside the
    /// members that define it, so a rename that leaves the number alone --
    /// which is exactly what happened -- fails.
    #[tokio::test]
    async fn the_snapshot_version_names_the_shape_it_ships() {
        let fake = FakeSettings::new(json!({ "hostname": "mos", "network": {}, "access": {} }));
        fake.set_system_info(json!({
            "machineId": { "available": true, "id": "0123456789abcdef0123456789abcdef" },
            "system": {
                "available": true, "version": "0.1.0+git00b674ec0ffe-1", "package": "mosd",
                "commitDate": { "available": true, "date": "2026-09-02T00:00:00Z" },
                "fileEpoch": {
                    "available": true, "epoch": 1_577_836_800,
                    "date": "2020-01-01T00:00:00Z",
                },
            },
            "uptime": { "available": true, "seconds": 7 },
        }));
        let snapshot = Collector::new(&fake).collect().await.snapshot;

        assert_eq!(
            snapshot["schemaVersion"], 2,
            "the shipped version does not name the shape below"
        );
        let system = &snapshot["system"]["system"];
        assert_eq!(system["commitDate"]["date"], "2026-09-02T00:00:00Z");
        assert_eq!(system["fileEpoch"]["epoch"], 1_577_836_800);
        assert!(
            system.get("buildDate").is_none() && system.get("buildEpoch").is_none(),
            "version 1's members are still shipped, so 2 is the wrong number: {system}"
        );
    }

    /// The time bound, enforced: sources that answer too slowly are
    /// abandoned per section, the deadline stops the rest before they are
    /// asked, and the snapshot is still produced — within the bound.
    #[tokio::test]
    async fn slow_sources_are_abandoned_within_the_deadline() {
        let fake = FakeSettings::new(json!({ "hostname": "mos", "network": {}, "access": {} }));
        fake.set_diagnostic_delay(Duration::from_millis(300));
        let collector = Collector::new(&fake)
            .with_bounds(Duration::from_millis(500), Duration::from_millis(200));
        let started = Instant::now();
        let collected = collector.collect().await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the collection waited on slow sources: {:?}",
            started.elapsed()
        );
        assert!(collected.report.elapsed < Duration::from_secs(3));
        let snapshot = &collected.snapshot;
        assert_eq!(snapshot["schemaVersion"], SCHEMA_VERSION);
        // Every source timed out one way or the other, and the members say
        // so rather than reading as healthy.
        for (name, status) in &collected.report.sections {
            assert_eq!(*status, SectionStatus::Timeout, "{name}");
        }
        assert_eq!(snapshot["system"]["available"], false);
        assert!(
            snapshot["system"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("no answer within"))
        );
        assert_eq!(snapshot["boot"]["slot"]["available"], false);
        assert!(
            snapshot["boot"]["slot"]["detail"]
                .as_str()
                .is_some_and(|d| d.starts_with("not collected"))
        );
        // The deadline, not only the per-section bound, did the stopping:
        // the last source was never asked.
        assert!(
            snapshot["network"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("deadline")),
            "{}",
            snapshot["network"]
        );
    }

    /// A source that errors is `unavailable`, with the error, and the rest
    /// of the snapshot is unaffected.
    #[tokio::test]
    async fn an_erroring_source_is_unavailable_not_fatal() {
        struct HalfFailing(FakeSettings);

        #[async_trait::async_trait]
        impl SettingsApi for HalfFailing {
            async fn get_settings(&self, path: &str) -> anyhow::Result<Value> {
                self.0.get_settings(path).await
            }
            async fn set_settings(&self, path: &str, value: &Value) -> anyhow::Result<String> {
                self.0.set_settings(path, value).await
            }
            async fn get_state(&self, path: &str) -> anyhow::Result<Value> {
                self.0.get_state(path).await
            }
            async fn get_time_status(&self) -> anyhow::Result<Value> {
                anyhow::bail!("timesyncd exploded")
            }
            async fn get_storage_status(&self) -> anyhow::Result<Value> {
                self.0.get_storage_status().await
            }
            async fn get_system_info(&self) -> anyhow::Result<Value> {
                self.0.get_system_info().await
            }
            async fn get_telemetry(&self) -> anyhow::Result<Value> {
                self.0.get_telemetry().await
            }
            async fn get_observed_network(&self) -> anyhow::Result<Value> {
                self.0.get_observed_network().await
            }
            async fn get_failure_evidence(&self) -> anyhow::Result<Value> {
                self.0.get_failure_evidence().await
            }
            async fn reboot(&self) -> anyhow::Result<()> {
                self.0.reboot().await
            }
            async fn power_off(&self) -> anyhow::Result<()> {
                self.0.power_off().await
            }
            async fn set_transient_root_password(&self, password: &str) -> anyhow::Result<String> {
                self.0.set_transient_root_password(password).await
            }
            async fn rotate_wireguard_key(&self, iface: &str) -> anyhow::Result<String> {
                self.0.rotate_wireguard_key(iface).await
            }
            async fn get_update_state(&self) -> anyhow::Result<Value> {
                self.0.get_update_state().await
            }
            async fn check_update(&self) -> anyhow::Result<()> {
                self.0.check_update().await
            }
            async fn fetch_update(&self) -> anyhow::Result<()> {
                self.0.fetch_update().await
            }
            async fn install_update(&self, bundle: &str) -> anyhow::Result<()> {
                self.0.install_update(bundle).await
            }
            async fn mark_update(
                &self,
                state: &str,
                slot: &str,
            ) -> anyhow::Result<(String, String)> {
                self.0.mark_update(state, slot).await
            }
            async fn set_reboot_override(&self, seconds: u32) -> anyhow::Result<Value> {
                self.0.set_reboot_override(seconds).await
            }
        }

        let api = HalfFailing(FakeSettings::new(
            json!({ "hostname": "mos", "network": {}, "access": {} }),
        ));
        let collected = Collector::new(&api).collect().await;
        assert_eq!(
            collected.report.sections["time"],
            SectionStatus::Unavailable
        );
        assert_eq!(collected.report.sections["system"], SectionStatus::Ok);
        assert_eq!(collected.snapshot["time"]["available"], false);
        assert!(
            collected.snapshot["time"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("exploded"))
        );
        assert_eq!(collected.snapshot["system"]["uptime"]["seconds"], 7);
    }
}

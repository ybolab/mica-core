//! Failure evidence for the diagnostic snapshot (PLAN-052 / RFCT-288): the
//! units systemd holds in the `failed` state, and a bounded excerpt of the
//! current boot's journal.
//!
//! Both reads are BOUNDED IN SIZE AND TIME by construction, not by intention:
//! the journal excerpt is capped in lines, bytes and bytes-per-line by
//! [`bound_excerpt`], and both reads run under a timeout after which the
//! member is reported absent with the reason. A `journalctl` that hangs is
//! killed when its future is dropped; it can never hold the bus dispatcher.
//!
//! The journal is the ONLY place this daemon runs a program to answer a read,
//! and the reason is structural: journald's files are a binary format with no
//! pure-Rust reader in this workspace's dependency graph, and `journalctl` is
//! the tool systemd ships to read them. The invocation is fixed (this boot,
//! warnings and worse, the newest N lines, no pager) and takes nothing from
//! the caller, and the per-line hostname is left out. journald runs
//! `Storage=volatile` on the image
//! (`rootfs/overlay/etc/systemd/journald.conf.d/00-volatile.conf`), so there
//! is no previous boot to ask for: the excerpt is this boot's, by construction.
//!
//! What this module deliberately does NOT do: read the journal at any other
//! priority, follow it, accept a filter from the caller, or run anything
//! else. The redaction of what it returns is apid's, at the snapshot boundary.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value as Json, json};

/// The newest lines the excerpt keeps.
pub const JOURNAL_MAX_LINES: usize = 400;
/// The most bytes the excerpt keeps, after per-line truncation.
pub const JOURNAL_MAX_BYTES: usize = 128 * 1024;
/// The most bytes one line keeps; longer lines are cut and marked.
pub const JOURNAL_MAX_LINE_BYTES: usize = 1024;
/// The `journalctl -p` floor: warnings and worse.
pub const JOURNAL_PRIORITY: &str = "warning";
/// How long `journalctl` may take before the excerpt is reported absent.
pub const JOURNAL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the failed-unit read may take before it is reported absent.
pub const UNITS_TIMEOUT: Duration = Duration::from_secs(3);
/// The most failed units reported; a system with more is a system with a
/// bigger problem than the list, and the count says so.
pub const MAX_FAILED_UNITS: usize = 64;

/// Reads the journal's newest lines.
#[async_trait::async_trait]
pub trait JournalReader: Send + Sync {
    /// The newest `max_lines` lines at [`JOURNAL_PRIORITY`] and worse, raw.
    async fn read(&self, max_lines: usize) -> Result<Vec<u8>>;
}

/// The production reader: `journalctl` on the host, killed on drop.
pub struct Journalctl;

#[async_trait::async_trait]
impl JournalReader for Journalctl {
    async fn read(&self, max_lines: usize) -> Result<Vec<u8>> {
        let output = tokio::process::Command::new("journalctl")
            .args([
                "-b",
                "-q",
                "--no-pager",
                // The hostname is an operator-chosen string and identifies
                // the site; it is not evidence, so it is not read.
                "--no-hostname",
                "-o",
                "short-iso",
                "-p",
                JOURNAL_PRIORITY,
                "-n",
                &max_lines.to_string(),
            ])
            .stdin(std::process::Stdio::null())
            // Dropping the `output` future — which is what a timeout does —
            // must not leave a journalctl behind holding a pipe nobody reads.
            .kill_on_drop(true)
            .output()
            .await
            .context("run journalctl")?;
        anyhow::ensure!(
            output.status.success(),
            "journalctl exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }
}

/// One unit systemd holds in the `failed` state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedUnit {
    /// The unit name.
    pub name: String,
    /// The unit's description.
    pub description: String,
    /// `loaded`, `not-found`, ...
    pub load_state: String,
    /// `failed`, by selection.
    pub active_state: String,
    /// The sub-state: `failed`, `dead`, ...
    pub sub_state: String,
}

/// Lists failed units.
#[async_trait::async_trait]
pub trait UnitLister: Send + Sync {
    /// Every unit in the `failed` state.
    async fn failed_units(&self) -> Result<Vec<FailedUnit>>;
}

/// The production lister: `ListUnitsFiltered(["failed"])` on the system bus.
pub struct SystemdUnits;

/// `ListUnitsFiltered` returns `a(ssssssouso)`: name, description, load
/// state, active state, sub state, followed unit, object path, job id, job
/// type, job object path.
type SystemdUnit = (
    String,
    String,
    String,
    String,
    String,
    String,
    zbus::zvariant::OwnedObjectPath,
    u32,
    String,
    zbus::zvariant::OwnedObjectPath,
);

#[async_trait::async_trait]
impl UnitLister for SystemdUnits {
    async fn failed_units(&self) -> Result<Vec<FailedUnit>> {
        let connection = zbus::Connection::system()
            .await
            .context("connect to the system bus")?;
        let manager = zbus::Proxy::new(
            &connection,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await
        .context("connect to systemd")?;
        let listed: Vec<SystemdUnit> = manager
            .call("ListUnitsFiltered", &(vec!["failed".to_string()],))
            .await
            .context("ListUnitsFiltered")?;
        Ok(listed
            .into_iter()
            .map(|unit| FailedUnit {
                name: unit.0,
                description: unit.1,
                load_state: unit.2,
                active_state: unit.3,
                sub_state: unit.4,
            })
            .collect())
    }
}

/// A bounded journal excerpt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Excerpt {
    /// The newest lines that fit, oldest first.
    pub lines: Vec<String>,
    /// Whether any line or any byte was dropped to fit the caps.
    pub truncated: bool,
    /// How many lines the raw output carried.
    pub source_lines: usize,
    /// How many bytes the raw output carried.
    pub source_bytes: usize,
}

/// Bound `raw` to the newest `max_lines` lines, each cut to
/// `max_line_bytes`, dropping the OLDEST of what remains until the whole
/// excerpt fits in `max_bytes`.
///
/// Newest-first is the point: when something has to go, it is the line
/// furthest from the failure the snapshot was taken for. Cuts land on a
/// character boundary, so an excerpt is always valid UTF-8.
#[must_use]
pub fn bound_excerpt(
    raw: &[u8],
    max_lines: usize,
    max_bytes: usize,
    max_line_bytes: usize,
) -> Excerpt {
    let text = String::from_utf8_lossy(raw);
    let all: Vec<&str> = text.lines().collect();
    let source_lines = all.len();
    let mut truncated = false;
    let keep_from = source_lines.saturating_sub(max_lines);
    if keep_from > 0 {
        truncated = true;
    }
    let mut lines: Vec<String> = all[keep_from..]
        .iter()
        .map(|line| {
            if line.len() > max_line_bytes {
                truncated = true;
                let mut cut = max_line_bytes;
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…[cut]", &line[..cut])
            } else {
                (*line).to_string()
            }
        })
        .collect();
    let mut total: usize = lines.iter().map(|line| line.len() + 1).sum();
    let mut drop = 0;
    while total > max_bytes && drop < lines.len() {
        total -= lines[drop].len() + 1;
        drop += 1;
    }
    if drop > 0 {
        truncated = true;
        lines.drain(..drop);
    }
    Excerpt {
        lines,
        truncated,
        source_lines,
        source_bytes: raw.len(),
    }
}

/// Read-only source of failure evidence, already rendered: the bus method
/// serves what this returns.
#[async_trait::async_trait]
pub trait FailureEvidenceSource: Send + Sync {
    /// Observe and render.
    ///
    /// # Errors
    ///
    /// Returns an error only when this daemon has no source at all; a
    /// production source answers with absent members instead.
    async fn observe(&self) -> Result<Json>;
}

/// The source a daemon without host access has: none.
pub struct UnavailableFailureEvidence;

#[async_trait::async_trait]
impl FailureEvidenceSource for UnavailableFailureEvidence {
    async fn observe(&self) -> Result<Json> {
        Err(anyhow::anyhow!("this daemon observes no failure evidence"))
    }
}

/// Production source: a journal reader and a unit lister, each under its
/// own timeout.
pub struct HostFailureEvidence {
    journal: Box<dyn JournalReader>,
    units: Box<dyn UnitLister>,
    journal_timeout: Duration,
    units_timeout: Duration,
}

impl HostFailureEvidence {
    /// The source `main.rs` attaches on a device.
    #[must_use]
    pub fn production() -> Self {
        Self::new(Box::new(Journalctl), Box::new(SystemdUnits))
    }

    /// A source over the given reader and lister, on the shipped timeouts.
    #[must_use]
    pub fn new(journal: Box<dyn JournalReader>, units: Box<dyn UnitLister>) -> Self {
        Self {
            journal,
            units,
            journal_timeout: JOURNAL_TIMEOUT,
            units_timeout: UNITS_TIMEOUT,
        }
    }

    /// Override both timeouts, for the tests that prove they are enforced.
    /// Test-only: the shipped bounds are the constants above, and nothing
    /// configures them.
    #[cfg(test)]
    #[must_use]
    pub fn with_timeouts(mut self, journal: Duration, units: Duration) -> Self {
        self.journal_timeout = journal;
        self.units_timeout = units;
        self
    }
}

fn absent(detail: impl Into<String>) -> Json {
    json!({ "available": false, "detail": detail.into() })
}

/// The `journal` member for one read outcome.
fn journal_json(outcome: Result<Result<Vec<u8>>, Duration>) -> Json {
    match outcome {
        Err(timeout) => absent(format!(
            "journalctl did not answer within {timeout:?}; the excerpt was abandoned"
        )),
        Ok(Err(err)) => absent(format!("journal read failed: {err:#}")),
        Ok(Ok(raw)) => {
            let excerpt = bound_excerpt(
                &raw,
                JOURNAL_MAX_LINES,
                JOURNAL_MAX_BYTES,
                JOURNAL_MAX_LINE_BYTES,
            );
            json!({
                "available": true,
                "scope": "current boot",
                "priority": JOURNAL_PRIORITY,
                "lineCount": excerpt.lines.len(),
                "sourceLines": excerpt.source_lines,
                "sourceBytes": excerpt.source_bytes,
                "truncated": excerpt.truncated,
                "bounds": {
                    "maxLines": JOURNAL_MAX_LINES,
                    "maxBytes": JOURNAL_MAX_BYTES,
                    "maxLineBytes": JOURNAL_MAX_LINE_BYTES,
                },
                "lines": excerpt.lines,
            })
        }
    }
}

/// The `units` member for one read outcome.
fn units_json(outcome: Result<Result<Vec<FailedUnit>>, Duration>) -> Json {
    match outcome {
        Err(timeout) => absent(format!("systemd did not answer within {timeout:?}")),
        Ok(Err(err)) => absent(format!("failed-unit read failed: {err:#}")),
        Ok(Ok(units)) => {
            let total = units.len();
            let entries: Vec<Json> = units
                .iter()
                .take(MAX_FAILED_UNITS)
                .map(|unit| {
                    json!({
                        "name": unit.name,
                        "description": unit.description,
                        "loadState": unit.load_state,
                        "activeState": unit.active_state,
                        "subState": unit.sub_state,
                    })
                })
                .collect();
            json!({
                "available": true,
                "count": total,
                "truncated": total > MAX_FAILED_UNITS,
                "entries": entries,
            })
        }
    }
}

#[async_trait::async_trait]
impl FailureEvidenceSource for HostFailureEvidence {
    async fn observe(&self) -> Result<Json> {
        let journal =
            tokio::time::timeout(self.journal_timeout, self.journal.read(JOURNAL_MAX_LINES));
        let units = tokio::time::timeout(self.units_timeout, self.units.failed_units());
        let (journal, units) = tokio::join!(journal, units);
        Ok(json!({
            "journal": journal_json(journal.map_err(|_| self.journal_timeout)),
            "units": units_json(units.map_err(|_| self.units_timeout)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedJournal(Vec<u8>);

    #[async_trait::async_trait]
    impl JournalReader for FixedJournal {
        async fn read(&self, _max_lines: usize) -> Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    struct SlowJournal;

    #[async_trait::async_trait]
    impl JournalReader for SlowJournal {
        async fn read(&self, _max_lines: usize) -> Result<Vec<u8>> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(b"too late\n".to_vec())
        }
    }

    struct FixedUnits(Vec<FailedUnit>);

    #[async_trait::async_trait]
    impl UnitLister for FixedUnits {
        async fn failed_units(&self) -> Result<Vec<FailedUnit>> {
            Ok(self.0.clone())
        }
    }

    struct SlowUnits;

    #[async_trait::async_trait]
    impl UnitLister for SlowUnits {
        async fn failed_units(&self) -> Result<Vec<FailedUnit>> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(Vec::new())
        }
    }

    fn unit(name: &str) -> FailedUnit {
        FailedUnit {
            name: name.to_string(),
            description: format!("{name} description"),
            load_state: "loaded".to_string(),
            active_state: "failed".to_string(),
            sub_state: "failed".to_string(),
        }
    }

    /// The line cap keeps the NEWEST lines.
    #[test]
    fn the_line_cap_keeps_the_newest_lines() {
        let raw: String = (0..10).map(|i| format!("line {i}\n")).collect();
        let excerpt = bound_excerpt(raw.as_bytes(), 3, 1 << 20, 1 << 10);
        assert_eq!(excerpt.lines, vec!["line 7", "line 8", "line 9"]);
        assert!(excerpt.truncated);
        assert_eq!(excerpt.source_lines, 10);
        assert_eq!(excerpt.source_bytes, raw.len());

        let fits = bound_excerpt(raw.as_bytes(), 10, 1 << 20, 1 << 10);
        assert_eq!(fits.lines.len(), 10);
        assert!(!fits.truncated);
    }

    /// The byte cap drops the OLDEST lines until the rest fit.
    #[test]
    fn the_byte_cap_drops_the_oldest_lines() {
        // Ten lines of 7 bytes plus a newline each: 80 bytes.
        let raw: String = (0..10).map(|i| format!("line {i}\n")).collect();
        let excerpt = bound_excerpt(raw.as_bytes(), 100, 25, 1 << 10);
        // 3 lines x 8 bytes = 24 <= 25; 4 lines would be 32.
        assert_eq!(excerpt.lines, vec!["line 7", "line 8", "line 9"]);
        assert!(excerpt.truncated);
    }

    /// A long line is cut on a character boundary and marked.
    #[test]
    fn a_long_line_is_cut_on_a_character_boundary() {
        // Three-byte characters: a cap of 7 lands inside the third one and
        // has to step back to 6.
        let raw = "€€€€\nshort\n";
        let excerpt = bound_excerpt(raw.as_bytes(), 10, 1 << 20, 7);
        assert_eq!(excerpt.lines[0], "€€…[cut]");
        assert_eq!(excerpt.lines[1], "short");
        assert!(excerpt.truncated);
    }

    /// The whole line cap, byte cap and line cut are enforced together on
    /// the shipped constants, over an input bigger than every one of them.
    #[test]
    fn the_shipped_bounds_hold_over_an_oversized_journal() {
        let long = "x".repeat(JOURNAL_MAX_LINE_BYTES * 2);
        let raw: String = (0..(JOURNAL_MAX_LINES * 2))
            .map(|i| format!("{i} {long}\n"))
            .collect();
        let excerpt = bound_excerpt(
            raw.as_bytes(),
            JOURNAL_MAX_LINES,
            JOURNAL_MAX_BYTES,
            JOURNAL_MAX_LINE_BYTES,
        );
        assert!(excerpt.truncated);
        assert!(excerpt.lines.len() <= JOURNAL_MAX_LINES);
        let total: usize = excerpt.lines.iter().map(|line| line.len() + 1).sum();
        assert!(total <= JOURNAL_MAX_BYTES, "{total}");
        assert!(
            excerpt
                .lines
                .iter()
                .all(|line| line.len() <= JOURNAL_MAX_LINE_BYTES + "…[cut]".len())
        );
        // And the newest line survived.
        assert!(
            excerpt
                .lines
                .last()
                .is_some_and(|line| line.starts_with(&format!("{} ", JOURNAL_MAX_LINES * 2 - 1)))
        );
    }

    /// The rendered members over fixed readers.
    #[tokio::test]
    async fn fixed_readers_render_both_members_available() {
        let source = HostFailureEvidence::new(
            Box::new(FixedJournal(
                b"2026-09-02T00:00:00+0000 mos systemd[1]: Failed to start x.service.\n".to_vec(),
            )),
            Box::new(FixedUnits(vec![unit("x.service")])),
        );
        let evidence = source.observe().await.expect("observe");
        assert_eq!(evidence["journal"]["available"], true);
        assert_eq!(evidence["journal"]["lineCount"], 1);
        assert_eq!(evidence["journal"]["truncated"], false);
        assert_eq!(evidence["journal"]["scope"], "current boot");
        assert_eq!(evidence["journal"]["bounds"]["maxLines"], JOURNAL_MAX_LINES);
        assert!(
            evidence["journal"]["lines"][0]
                .as_str()
                .is_some_and(|l| l.contains("x.service"))
        );
        assert_eq!(evidence["units"]["available"], true);
        assert_eq!(evidence["units"]["count"], 1);
        assert_eq!(evidence["units"]["entries"][0]["name"], "x.service");
        assert_eq!(evidence["units"]["entries"][0]["subState"], "failed");
    }

    /// The time bound, enforced: readers that never answer in time leave
    /// absent members with the reason, and the observation itself returns
    /// within the bound rather than waiting on them.
    #[tokio::test]
    async fn slow_readers_are_abandoned_within_the_bound() {
        let source = HostFailureEvidence::new(Box::new(SlowJournal), Box::new(SlowUnits))
            .with_timeouts(Duration::from_millis(50), Duration::from_millis(50));
        let started = std::time::Instant::now();
        let evidence = source.observe().await.expect("observe");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the observation waited on a slow reader: {:?}",
            started.elapsed()
        );
        assert_eq!(evidence["journal"]["available"], false);
        assert!(
            evidence["journal"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("did not answer within"))
        );
        assert_eq!(evidence["units"]["available"], false);
        assert!(
            evidence["units"]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("did not answer within"))
        );
    }

    /// A unit list past the cap is cut and counted.
    #[test]
    fn a_unit_list_past_the_cap_is_cut_and_counted() {
        let units: Vec<FailedUnit> = (0..(MAX_FAILED_UNITS + 3))
            .map(|i| unit(&format!("u{i}.service")))
            .collect();
        let rendered = units_json(Ok(Ok(units)));
        assert_eq!(rendered["count"], MAX_FAILED_UNITS + 3);
        assert_eq!(rendered["truncated"], true);
        assert_eq!(
            rendered["entries"].as_array().unwrap().len(),
            MAX_FAILED_UNITS
        );
        // And an empty list is real evidence: zero failed units.
        let empty = units_json(Ok(Ok(Vec::new())));
        assert_eq!(empty["available"], true);
        assert_eq!(empty["count"], 0);
    }

    #[tokio::test]
    async fn the_unavailable_source_is_an_error() {
        assert!(UnavailableFailureEvidence.observe().await.is_err());
    }
}

//! Bounded persistent audit trail (`docs/design/access.md` §6): one JSONL
//! line per security-relevant action, in a two-file ring, mirrored to
//! tracing so the (volatile) journal tells the same story.
//!
//! A line carries a UTC timestamp, the event, its outcome and the source
//! address — and **never** a password, a hash or any other credential
//! material. Callers pass fixed strings and a peer address; nothing operator-
//! typed flows in, and a test asserts the file stays free of password
//! material.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use axum::extract::FromRequestParts;
use axum::extract::connect_info::ConnectInfo;
use axum::http::request::Parts;

/// Per-file rotation threshold. Two files bound the trail at ~512 KiB —
/// noise on the 64 MiB STATE partition, yet thousands of events per file at
/// ~130 bytes a line, which on an appliance whose audited actions are human
/// logins and power requests is months of history. Writes are appends plus
/// one rename per rotation, so eMMC wear is a handful of sectors per event.
const ROTATE_BYTES: u64 = 256 * 1024;

/// The live log file, appended to.
const LOG: &str = "audit.log";

/// The previous generation; each rotation replaces it, which is what drops
/// the oldest events and bounds the total.
const LOG_PREVIOUS: &str = "audit.log.1";

/// Append-only audit sink.
pub struct Audit {
    /// `None` = journal-only: tests that construct [`super::routes::AppState`]
    /// without persistence. Production always carries a directory.
    dir: Option<PathBuf>,
    /// Serializes append+rotate so two events cannot interleave a rotation
    /// and lose each other's lines.
    lock: Mutex<()>,
}

impl Audit {
    /// A sink that only mirrors to tracing.
    pub fn journal_only() -> Self {
        Self {
            dir: None,
            lock: Mutex::new(()),
        }
    }

    /// A sink writing the ring under `dir` (which must already exist).
    pub fn at(dir: PathBuf) -> Self {
        Self {
            dir: Some(dir),
            lock: Mutex::new(()),
        }
    }

    /// Record one event.
    ///
    /// A failed write is logged and swallowed, deliberately: §6's ideal is
    /// "no audit trail ⇒ no shell", but refusing to serve management when the
    /// disk fails is a lockdown decision this campaign does not take —
    /// recorded as such in `docs/design/access.md` §6. The journal mirror is
    /// emitted first so a failing disk cannot silence the event entirely.
    pub fn record(&self, event: &str, outcome: &str, source: &str) {
        tracing::info!(target: "audit", event, outcome, source, "audit event");
        let Some(dir) = &self.dir else { return };
        let line = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "event": event,
            "outcome": outcome,
            "source": source,
        })
        .to_string();
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(err) = append(dir, &line) {
            tracing::warn!(error = %err, "audit line could not be written");
        }
    }
}

/// Append `line` to the ring, rotating first when it would breach the cap.
fn append(dir: &Path, line: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let path = dir.join(LOG);
    let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    if size + line.len() as u64 + 1 > ROTATE_BYTES {
        std::fs::rename(&path, dir.join(LOG_PREVIOUS))
            .with_context(|| format!("rotate {}", path.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.write_all(line.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .with_context(|| format!("append to {}", path.display()))?;
    // Synced per line because the two most consequential events — reboot and
    // poweroff requests — are immediately followed by the power state the
    // fsync protects against, and every audited event is human-rate.
    file.sync_all()
        .with_context(|| format!("flush {}", path.display()))?;
    Ok(())
}

/// The requesting peer's address as text, for audit lines.
///
/// Read from the [`ConnectInfo`] extension that
/// `into_make_service_with_connect_info` installs (`main.rs`). `unknown`
/// rather than a rejection when it is absent — router-level tests drive the
/// service with `oneshot` and no connection — so wiring the audit trail can
/// never be what makes a login fail.
pub struct Source(pub String);

impl<S: Send + Sync> FromRequestParts<S> for Source {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map_or_else(|| "unknown".to_string(), |info| info.0.to_string()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_journal_only_sink_writes_nothing() {
        // Must not panic or create files anywhere.
        Audit::journal_only().record("login", "success", "unknown");
    }

    #[test]
    fn lines_are_jsonl_with_the_documented_fields_and_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::at(dir.path().to_path_buf());
        audit.record("login", "wrong-password", "192.0.2.7:1234");
        let path = dir.path().join(LOG);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let contents = std::fs::read_to_string(&path).unwrap();
        let line: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(line["event"], "login");
        assert_eq!(line["outcome"], "wrong-password");
        assert_eq!(line["source"], "192.0.2.7:1234");
        // RFC 3339, UTC ("Z"): parseable without knowing the writer's zone.
        let ts = line["ts"].as_str().unwrap();
        assert!(ts.ends_with('Z'), "timestamp must be UTC: {ts}");
        chrono::DateTime::parse_from_rfc3339(ts).unwrap();
    }

    #[test]
    fn the_ring_is_bounded_and_keeps_the_newest_events() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::at(dir.path().to_path_buf());
        // Long sources make each line ~1 KiB so the test crosses the cap in
        // hundreds of writes rather than thousands.
        let padding = "x".repeat(1000);
        audit.record("marker", "oldest", &padding);
        for _ in 0..3 * (ROTATE_BYTES / 1000) {
            audit.record("filler", "ok", &padding);
        }
        audit.record("marker", "newest", &padding);

        let live = std::fs::read_to_string(dir.path().join(LOG)).unwrap();
        let previous = std::fs::read_to_string(dir.path().join(LOG_PREVIOUS)).unwrap();
        for contents in [&live, &previous] {
            assert!(contents.len() as u64 <= ROTATE_BYTES + 1100);
            for line in contents.lines() {
                serde_json::from_str::<serde_json::Value>(line).expect("every line parses");
            }
        }
        assert!(live.contains("newest"), "the newest event is retained");
        assert!(
            !live.contains("oldest") && !previous.contains("oldest"),
            "the oldest event was dropped by rotation"
        );
    }
}

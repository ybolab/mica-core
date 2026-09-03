//! The device's ONE audit ring (`docs/design/access.md` §6): one JSONL line
//! per security-relevant action, in a two-file ring, fsynced per line.
//!
//! **Here, rather than in apid, because there are two writers.** apid records
//! what an operator asked over the API; mosd records what a board-declared
//! physical recovery action did at boot (`docs/design/recovery.md` §4), which
//! happens before apid is serving anything. Two implementations of one ring
//! would be two rotation policies and two line shapes over one file, so the
//! shape, the cap and the rotation live in the crate both binaries link and
//! neither owns a private copy.
//!
//! A line carries a UTC timestamp, the event, the outcome and the source — and
//! **never** a password, a hash or any other credential material. Callers pass
//! fixed strings; nothing operator-typed flows in.

use std::path::{Path, PathBuf};

/// Per-file rotation threshold. Two files bound the trail at ~512 KiB —
/// noise on the 64 MiB STATE partition, yet thousands of events per file at
/// ~130 bytes a line, which on an appliance whose audited actions are human
/// logins and power requests is months of history. Writes are appends plus
/// one rename per rotation, so eMMC wear is a handful of sectors per event.
pub const AUDIT_ROTATE_BYTES: u64 = 256 * 1024;

/// The live log file, appended to.
pub const AUDIT_LOG: &str = "audit.log";

/// The previous generation; each rotation replaces it, which is what drops
/// the oldest events and bounds the total.
pub const AUDIT_LOG_PREVIOUS: &str = "audit.log.1";

/// The directory holding the ring on a device.
///
/// It is apid's STATE-backed state directory because apid created it first;
/// the ring is the DEVICE's, not that daemon's, which is what makes a line
/// written by mosd belong in the same file rather than in a second one an
/// operator would have to know to read.
pub const DEFAULT_AUDIT_RING_DIR: &str = "/var/lib/mos/apid";

/// Test hook relocating [`DEFAULT_AUDIT_RING_DIR`].
pub const AUDIT_RING_DIR_ENV: &str = "MOS_AUDIT_RING_DIR";

/// The ring directory this device uses, honouring the test hook.
#[must_use]
pub fn audit_ring_dir() -> PathBuf {
    std::env::var_os(AUDIT_RING_DIR_ENV)
        .map_or_else(|| PathBuf::from(DEFAULT_AUDIT_RING_DIR), PathBuf::from)
}

/// One audit line: four members, and there is never a fifth.
#[must_use]
pub fn audit_line(event: &str, outcome: &str, source: &str) -> String {
    serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "event": event,
        "outcome": outcome,
        "source": source,
    })
    .to_string()
}

/// Append `line` to the ring under `dir`, rotating first when it would breach
/// the cap.
///
/// # Errors
///
/// Returns the underlying I/O failure, named with the path it happened on.
/// Every caller logs and swallows it: §6's ideal is "no audit trail ⇒ no
/// shell", but refusing to serve management when the disk fails is a lockdown
/// decision this campaign does not take.
pub fn append_audit_line(dir: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let path = dir.join(AUDIT_LOG);
    let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    if size + line.len() as u64 + 1 > AUDIT_ROTATE_BYTES {
        std::fs::rename(&path, dir.join(AUDIT_LOG_PREVIOUS))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(line.as_bytes())
        .and_then(|()| file.write_all(b"\n"))?;
    // Synced per line because the two most consequential events — reboot and
    // poweroff requests — are immediately followed by the power state the
    // fsync protects against, and every audited event is human-rate.
    file.sync_all()
}

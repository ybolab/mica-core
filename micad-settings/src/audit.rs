//! The device's ONE audit ring (`docs/design/access.md` §6): one JSONL line
//! per security-relevant action, in a two-file ring, fsynced per line.
//!
//! **Here, rather than in apid, because there are two writers.** apid records
//! what an operator asked over the API; micad records what a board-declared
//! physical recovery action did at boot (`docs/design/recovery.md` §4), which
//! happens before apid is serving anything, and what the update policy's
//! automatic driver did with nobody watching (PLAN-071 §3). Two
//! implementations of one ring would be two rotation policies and two line
//! shapes over one file, so the shape, the cap and the rotation live in the
//! crate both binaries link and neither owns a private copy.
//!
//! A line carries a UTC timestamp, the event, the outcome, the source and the
//! actor — and **never** a password, a hash or any other credential material.
//! Callers pass fixed strings; nothing operator-typed flows in.

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
/// written by micad belong in the same file rather than in a second one an
/// operator would have to know to read.
pub const DEFAULT_AUDIT_RING_DIR: &str = "/var/lib/mica/apid";

/// Test hook relocating [`DEFAULT_AUDIT_RING_DIR`].
pub const AUDIT_RING_DIR_ENV: &str = "MOS_AUDIT_RING_DIR";

/// The ring directory this device uses, honouring the test hook.
#[must_use]
pub fn audit_ring_dir() -> PathBuf {
    std::env::var_os(AUDIT_RING_DIR_ENV)
        .map_or_else(|| PathBuf::from(DEFAULT_AUDIT_RING_DIR), PathBuf::from)
}

/// The actor of an action a human took: an authenticated operator, through
/// the management API.
///
/// Not the session's own identifier, deliberately. The cookie is a
/// credential and has no place in a file this one; the peer address is
/// already the line's `source`; and the question PLAN-071 §3 requires the
/// trail to answer is *did a human do this*, which two values answer and a
/// session id does not answer any better.
pub const ACTOR_OPERATOR: &str = "operator";

/// The actor of an action the device took on its own: the update policy's
/// automatic driver (PLAN-071 §2).
pub const ACTOR_POLICY: &str = "policy";

/// The actor of an action the device took that no policy and no operator
/// asked for: a boot-time recovery mechanism, or a daemon acting on what it
/// found on the disk at startup.
pub const ACTOR_DEVICE: &str = "device";

/// The update events, whose whole point is that **both daemons spell them the
/// same** (PLAN-071 §3).
///
/// apid records them when an operator asked; micad's automatic driver records
/// them when the policy did. If the two spellings drifted, the actor field
/// would still be there and the trail would answer *did a human do this* with
/// two event sets that never meet — so the names are constants in the crate
/// both binaries link rather than literals in each.
pub const UPDATE_CHECK_EVENT: &str = "update-check";
/// See [`UPDATE_CHECK_EVENT`].
pub const UPDATE_FETCH_EVENT: &str = "update-fetch";
/// See [`UPDATE_CHECK_EVENT`].
pub const UPDATE_INSTALL_EVENT: &str = "update-install";

/// The event recorded when the operator's update document is written
/// (PLAN-071 §3). Only ever an operator's: micad is the file's writer and no
/// automatic path asks it to write one.
pub const UPDATE_CONFIG_EVENT: &str = "update-config";

/// The outcome the three update events carry when the action was admitted and
/// is running. The work itself lands in `update.lifecycle`, which carries far
/// more than an audit line could; what this file answers is *who asked*.
pub const REQUESTED: &str = "requested";

/// One audit line: five members, and there is never a sixth.
///
/// **`actor` is the fifth and PLAN-071 §3 is why it exists**: the same event
/// names are recorded whether an operator asked or the automatic driver did,
/// so without it an update trail cannot answer *did a human do this* — and an
/// event set that cannot is a support tool that lies during exactly the
/// incident it exists for. It is one of [`ACTOR_OPERATOR`], [`ACTOR_POLICY`]
/// and [`ACTOR_DEVICE`]; nothing operator-typed reaches it, the same rule the
/// other four members follow.
#[must_use]
pub fn audit_line(event: &str, outcome: &str, source: &str, actor: &str) -> String {
    serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "event": event,
        "outcome": outcome,
        "source": source,
        "actor": actor,
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
    // ONE write, not a line write followed by a newline write. Two processes
    // append here now -- apid on a management action, micad on a boot-time
    // recovery action -- and O_APPEND makes a single write atomic against the
    // other writer while two writes can interleave into `A-lineB-line\n\n`.
    // A record that cannot be read back line by line is worth nothing in the
    // one situation this file exists for.
    let mut record = String::with_capacity(line.len() + 1);
    record.push_str(line);
    record.push('\n');
    file.write_all(record.as_bytes())?;
    // Synced per line because the two most consequential events — reboot and
    // poweroff requests — are immediately followed by the power state the
    // fsync protects against, and every audited event is human-rate.
    file.sync_all()
}

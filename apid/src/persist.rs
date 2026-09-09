//! Small durable-write helpers shared by the login-guard state and the audit
//! ring (`docs/design/access.md` §6).

use std::path::Path;

use anyhow::Context;

/// Seconds since the UNIX epoch, saturating at zero for a clock before 1970.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Write `contents` to `path` atomically: a temporary file in the same
/// directory, fsynced, renamed over the target, then the directory entry
/// fsynced. Mirrors `mosd/mosd/src/transient.rs`'s `write_atomically`, minus
/// the ownership handling apid (which runs and stays root) does not need.
///
/// Same directory because `rename` is only atomic within one filesystem; the
/// directory fsync is what makes the rename itself survive a power cut, and a
/// power cut is exactly the event the guard state exists to survive.
pub fn write_atomically(path: &Path, contents: impl AsRef<[u8]>, mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?;
    let temp = directory.join(format!(".{file_name}.apid-tmp"));

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temp)
        .with_context(|| format!("create {}", temp.display()))?;
    // Reset the mode before syncing, including an interrupted temporary file.
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("set mode on {}", temp.display()))?;
    file.write_all(contents.as_ref())
        .with_context(|| format!("write {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("flush {}", temp.display()))?;
    drop(file);

    std::fs::rename(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    std::fs::File::open(directory)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("flush {}", directory.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn writes_with_the_requested_mode_and_replaces_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_atomically(&path, "first", 0o600).unwrap();
        write_atomically(&path, "second", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // No temporary residue is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

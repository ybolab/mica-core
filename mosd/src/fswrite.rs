//! One place that knows how to write a configuration file on this appliance.
//!
//! mosd renders configuration into paths that are STATE-backed bind mounts,
//! and the correct way to write one depends on WHAT is bound:
//!
//!   * a bound DIRECTORY -- /etc/ssh, /etc/hostapd, /etc/wpa_supplicant,
//!     /etc/containers/systemd, /var/lib/mos. The files inside are ordinary
//!     files in a writable directory, so a temporary file and a rename gives an
//!     atomic replace.
//!
//!   * a bound FILE -- /etc/hostname, the only one. The file is writable and
//!     the squashfs directory around it is not, so no temporary file can be
//!     created beside it. rename(2) onto it fails with EBUSY because it is a
//!     mount point. And renaming over the SOURCE, /mnt/state/hostname, swaps
//!     the inode: the bind at /etc/hostname keeps pointing at the old one, so
//!     the write succeeds, the file is right, and reading /etc/hostname still
//!     gives the previous value until the next mount. That last failure passes
//!     every test that does not run on a device.
//!
//! One place, because a caller choosing for itself has to know whether its own
//! path is a mount point, and that is not visible in the code.

use std::path::Path;

use anyhow::{Context, Result, anyhow};

/// Write `contents` to `path` with `mode`, atomically where that is possible.
///
/// Replaces the file through a temporary sibling and a rename when `path` sits
/// in a writable directory; writes through the existing inode when `path` is
/// itself a mount point, which is the only case where a rename cannot work.
///
/// # Errors
///
/// Returns an error when the path has no parent or file name, or when any of
/// the create/write/permission/rename steps fails.
pub fn write_config(path: &Path, contents: &str, mode: u32) -> Result<()> {
    write_config_owned(path, contents, mode, None)
}

/// Same, and additionally `chown`s the result to `owner`.
///
/// Separate entry point rather than a fourth argument on every call site: one
/// file in mosd needs an owner (the shadow file transient.rs rewrites) and the
/// rest do not, and a `None` repeated at every other call reads as if the
/// question had been asked there.
///
/// # Errors
///
/// As [`write_config`], plus a failure to change ownership.
pub fn write_config_owned(
    path: &Path,
    contents: &str,
    mode: u32,
    owner: Option<(u32, u32)>,
) -> Result<()> {
    if is_mount_point(path) {
        write_in_place(path, contents, mode, owner)
    } else {
        write_via_rename(path, contents, mode, owner)
    }
}

/// True when `path` is a mount point: its device differs from its parent's.
///
/// Asked of the filesystem rather than configured, because a caller that had
/// to be TOLD which of its paths are bind mounts is a caller that will be
/// wrong after the next change to rootfs/overlay. A path that does not
/// exist yet is not a mount point -- systemd cannot bind onto nothing.
fn is_mount_point(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Some(parent) = path.parent() else {
        return false;
    };
    let (Ok(here), Ok(up)) = (std::fs::metadata(path), std::fs::metadata(parent)) else {
        return false;
    };
    here.dev() != up.dev()
}

/// Replace the file atomically: temporary sibling, then rename.
fn write_via_rename(
    path: &Path,
    contents: &str,
    mode: u32,
    owner: Option<(u32, u32)>,
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    let temp = directory.join(format!(".{file_name}.mosd-tmp"));

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temp)
        .with_context(|| format!("create {}", temp.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("write {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("flush {}", temp.display()))?;
    drop(file);

    // The mode above only takes effect when the temporary file is created, and
    // is masked by the umask even then; a leftover from an interrupted run
    // would keep its old mode. Both are fixed here, still before the rename.
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("set mode on {}", temp.display()))?;

    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(&temp, Some(uid), Some(gid))
            .with_context(|| format!("set owner on {}", temp.display()))?;
    }

    std::fs::rename(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    Ok(())
}

/// Write through the existing inode, for a path that is itself a mount point.
///
/// The atomicity that costs is real and worth naming: a reader that opens the
/// file between the truncate and the write sees it empty. It is accepted here
/// because the alternative is not a worse-atomicity write, it is NO WRITE AT
/// ALL -- and because the one file in this class, /etc/hostname, is read by
/// mos-apply-hostname.service at boot, which is not running when mosd applies.
fn write_in_place(path: &Path, contents: &str, mode: u32, owner: Option<(u32, u32)>) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("flush {}", path.display()))?;
    drop(file);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("set mode on {}", path.display()))?;
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))
            .with_context(|| format!("set owner on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[test]
    fn an_ordinary_path_is_replaced_atomically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conf");
        std::fs::write(&path, "old").expect("seed");
        let before = std::fs::metadata(&path).expect("stat").ino();

        write_config(&path, "new\n", 0o644).expect("write");

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "new\n");
        assert_ne!(
            before,
            std::fs::metadata(&path).expect("stat").ino(),
            "a writable directory must get the rename path, which replaces the inode"
        );
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_config(&dir.path().join("conf"), "x\n", 0o644).expect("write");
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("mosd-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn the_mode_is_applied_even_over_an_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conf");
        std::fs::write(&path, "old").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        write_config(&path, "new\n", 0o644).expect("write");

        assert_eq!(
            std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
            0o644
        );
    }

    /// The discriminator, against a mount point that exists everywhere.
    ///
    /// /proc is mounted on any running Linux, so this needs no privileges and
    /// no fixture. Without it the branch that matters on a device -- the one
    /// /etc/hostname takes -- would be chosen by code no test ever reaches.
    #[test]
    fn a_real_mount_point_is_recognised() {
        assert!(
            is_mount_point(Path::new("/proc")),
            "/proc is a mount point on every Linux; if this fails the device check is not comparing what it thinks"
        );
    }

    #[test]
    fn an_ordinary_file_is_not_a_mount_point() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conf");
        std::fs::write(&path, "x").expect("seed");
        assert!(!is_mount_point(&path));
    }

    /// The in-place branch writes through the existing inode.
    ///
    /// Called directly, because the only way to reach it through write_config
    /// is to have a real mount point as the target, and creating one needs
    /// privileges a test suite should not require.
    #[test]
    fn the_in_place_branch_keeps_the_inode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conf");
        std::fs::write(&path, "old").expect("seed");
        let before = std::fs::metadata(&path).expect("stat").ino();

        write_in_place(&path, "new\n", 0o644, None).expect("write");

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "new\n");
        assert_eq!(
            before,
            std::fs::metadata(&path).expect("stat").ino(),
            "the in-place branch exists because /etc/hostname is a bind mount: replacing the inode would leave the mount pointing at the old one"
        );
    }

    #[test]
    fn a_path_that_does_not_exist_is_not_taken_for_a_mount_point() {
        // metadata() on a missing file fails, and a `false` default is the
        // right reading: systemd cannot bind onto a path that is not there.
        // Reading the failure as "mount point" would send every first write
        // down the in-place branch, where it would succeed -- and silently
        // stop being atomic for every file mosd renders.
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_mount_point(&dir.path().join("absent")));
    }
}

//! Authenticated new-root validation and bounded native startup operations.

use anyhow::{Result, ensure};
use std::{fs, os::unix::fs::MetadataExt, path::Path};

pub mod native;
pub mod verity;

pub fn validate_target_init(root: &Path) -> Result<()> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
    // Absolute symlinks belong to the authenticated new root, not the initramfs.
    let root = fs::File::open(root)?;
    let init = fs::File::from(openat2(
        &root,
        "sbin/init",
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
    )?);
    let metadata = init.metadata()?;
    ensure!(
        metadata.is_file() && metadata.mode() & 0o111 != 0,
        "target init is not executable"
    );
    Ok(())
}

pub fn validate_new_root(root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(root)?;
    ensure!(
        metadata.is_dir() && metadata.dev() != fs::metadata("/")?.dev(),
        "new root is not a separate mounted filesystem"
    );
    validate_target_init(root)
}

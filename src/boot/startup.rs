//! Explicit commands for the pinned early-userspace BusyBox configuration.

use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    process::Command,
};

pub mod native;
pub mod verity;

fn busybox(args: &[&str]) -> Command {
    let mut command = Command::new("/bin/busybox");
    command.args(args);
    command
}

pub fn mount(source: &str, target: &str, kind: &str, options: &str) -> Command {
    busybox(&["mount", "-t", kind, "-o", options, source, target])
}

pub fn bind(source: &str, target: &str) -> Command {
    busybox(&["mount", "-o", "bind", source, target])
}

pub fn remount(source: &str, target: &str, options: &str) -> Command {
    // Both operands avoid BusyBox's single-target lookup in /proc/mounts.
    busybox(&["mount", "-o", options, source, target])
}

pub fn move_mount(source: &str, target: &str) -> Command {
    busybox(&["mount", "-o", "move", source, target])
}

pub fn switch_root() -> Command {
    busybox(&["switch_root", "/newroot", "/sbin/init"])
}

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

fn loop_number(device: &str) -> Result<u32> {
    let number = device
        .strip_prefix("/dev/loop")
        .context("invalid loop device")?;
    let index: u32 = number.parse().context("invalid loop device number")?;
    ensure!(
        index.to_string() == number,
        "noncanonical loop device number"
    );
    Ok(index)
}

pub struct LoopState {
    pub backing_file: PathBuf,
    pub read_only: bool,
    pub offset: u64,
    pub size_limit: u64,
}

pub fn read_loop_state(sysfs: &Path) -> Result<Option<LoopState>> {
    let backing_file = match fs::read_to_string(sysfs.join("loop/backing_file")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let backing_file = backing_file.strip_suffix('\n').unwrap_or(&backing_file);
    ensure!(backing_file.starts_with('/'), "invalid loop backing file");
    let read_only = match fs::read_to_string(sysfs.join("ro"))?.trim() {
        "1" => true,
        "0" => false,
        _ => bail!("invalid loop read-only state"),
    };
    Ok(Some(LoopState {
        backing_file: backing_file.into(),
        read_only,
        offset: fs::read_to_string(sysfs.join("loop/offset"))?
            .trim()
            .parse()?,
        size_limit: fs::read_to_string(sysfs.join("loop/sizelimit"))?
            .trim()
            .parse()?,
    }))
}

pub fn inspect_loop(device: &str) -> Result<Option<LoopState>> {
    let index = loop_number(device)?;
    let metadata = fs::symlink_metadata(device)?;
    ensure!(
        metadata.file_type().is_block_device()
            && rustix::fs::major(metadata.rdev()) == 7
            && rustix::fs::minor(metadata.rdev()) == index,
        "loop device node identity mismatch"
    );
    let sysfs = PathBuf::from(format!("/sys/class/block/loop{index}"));
    ensure!(
        fs::read_to_string(sysfs.join("dev"))?.trim() == format!("7:{index}"),
        "loop kernel device identity mismatch"
    );
    read_loop_state(&sysfs)
}

/// A free-device query is not a reservation. Recheck occupied candidates and
/// verify the successful binding; never detach a device acquired by a racer.
pub fn attach_read_only_loop(
    image: &Path,
    mut run: impl FnMut(Command) -> Result<String>,
    mut inspect: impl FnMut(&str) -> Result<Option<LoopState>>,
) -> Result<String> {
    let expected = fs::symlink_metadata(image)?;
    ensure!(
        expected.is_file(),
        "loop backing image is not a regular file"
    );
    for _ in 0..3 {
        let device = run(busybox(&["losetup", "-f"]))?;
        loop_number(&device)?;
        if inspect(&device)?.is_some() {
            continue;
        }
        let mut bind = busybox(&["losetup", "-r", &device]);
        bind.arg(image);
        if let Err(error) = run(bind) {
            // BusyBox reports a generic nonzero status for EBUSY. Only a now
            // occupied candidate warrants rediscovery; other failures stop.
            if inspect(&device)?.is_some() {
                continue;
            }
            return Err(error).context("read-only loop binding failed");
        }
        let state = inspect(&device)?.context("loop binding is absent")?;
        let attached = fs::metadata(&state.backing_file)?;
        ensure!(
            state.read_only
                && state.offset == 0
                && state.size_limit == 0
                && attached.dev() == expected.dev()
                && attached.ino() == expected.ino(),
            "loop binding is not the read-only whole selected image"
        );
        return Ok(device);
    }
    bail!("free loop device remained occupied after three attempts")
}

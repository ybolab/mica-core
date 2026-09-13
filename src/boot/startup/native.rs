//! Fixed startup operations executed under the existing watchdog supervisor.
use crate::components::VerityImage;
use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{Mode, OFlags},
    mount::MountFlags,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::Path,
    process::Command,
};

pub mod gpt;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Operation {
    Mount {
        source: String,
        target: String,
        kind: String,
        options: String,
    },
    Bind {
        source: String,
        target: String,
    },
    Remount {
        target: String,
        options: String,
    },
    Move {
        source: String,
        target: String,
    },
    Loop {
        image: String,
    },
    Partition {
        uuid: String,
    },
    Verity {
        device: String,
        name: String,
        signature: String,
        image: VerityImage,
    },
}

pub fn command(op: Operation) -> Result<Command> {
    let argument = serde_json::to_string(&op)?;
    ensure!(argument.len() <= 16384, "excessive startup operation");
    let mut command = Command::new("/init");
    command.args(["--startup-worker", &argument]);
    Ok(command)
}

pub fn mount_options(options: &str) -> Result<(MountFlags, CString)> {
    let mut flags = MountFlags::empty();
    let mut data = Vec::new();
    for option in options.split(',') {
        flags |= match option {
            "ro" => MountFlags::RDONLY,
            "rw" => MountFlags::empty(),
            "nosuid" => MountFlags::NOSUID,
            "nodev" => MountFlags::NODEV,
            "noexec" => MountFlags::NOEXEC,
            "noatime" => MountFlags::NOATIME,
            "bind" => MountFlags::BIND,
            "remount" => continue,
            "prjquota" | "mode=0755" | "mode=0700" | "nr_inodes=8192" | "size=32M" => {
                data.push(option);
                continue;
            }
            other
                if other
                    .strip_prefix("size=")
                    .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())) =>
            {
                data.push(option);
                continue;
            }
            _ => bail!("unsupported startup mount option"),
        };
    }
    Ok((flags, CString::new(data.join(","))?))
}

pub fn worker(args: &[String]) -> Option<Result<()>> {
    if args.first().map(String::as_str) != Some("--startup-worker") {
        return None;
    }
    Some((|| {
        ensure!(
            rustix::process::getppid() == rustix::process::Pid::from_raw(1),
            "startup worker requires PID1 parent"
        );
        ensure!(
            args.len() == 2 && args[1].len() <= 16384,
            "invalid startup worker request"
        );
        let operation: Operation = serde_json::from_str(&args[1])?;
        perform(operation)
    })())
}

fn perform(operation: Operation) -> Result<()> {
    match operation {
        Operation::Mount {
            source,
            target,
            kind,
            options,
        } => {
            ensure!(
                [
                    "devtmpfs", "proc", "sysfs", "tmpfs", "efivarfs", "ext4", "squashfs"
                ]
                .contains(&kind.as_str()),
                "unsupported startup filesystem"
            );
            let (flags, data) = mount_options(&options)?;
            rustix::mount::mount(&source, &target, &kind, flags, data.as_c_str())?;
        }
        Operation::Bind { source, target } => rustix::mount::mount_bind(source, target)?,
        Operation::Remount { target, options } => {
            let (flags, data) = mount_options(&options)?;
            rustix::mount::mount_remount(target, flags, data.as_c_str())?;
        }
        Operation::Move { source, target } => rustix::mount::mount_move(source, target)?,
        Operation::Loop { image } => println!("{}", attach_loop(Path::new(&image))?),
        Operation::Partition { uuid } => println!("{}", gpt::find_partition(&uuid)?),
        Operation::Verity {
            device,
            name,
            signature,
            image,
        } => super::verity::open(&device, &name, Path::new(&signature), &image)?,
    }
    Ok(())
}

pub fn attach_loop(image: &Path) -> Result<String> {
    let backing = File::from(rustix::fs::open(
        image,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let expected = backing.metadata()?;
    ensure!(expected.is_file(), "loop backing is not a regular file");
    let control = File::from(rustix::fs::open(
        "/dev/loop-control",
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let meta = control.metadata()?;
    ensure!(
        meta.file_type().is_char_device()
            && fs::read_to_string("/sys/class/misc/loop-control/dev")?.trim()
                == format!(
                    "{}:{}",
                    rustix::fs::major(meta.rdev()),
                    rustix::fs::minor(meta.rdev())
                ),
        "loop control identity mismatch"
    );
    for _ in 0..3 {
        let index = lifecycle_sys::loop_free(&control)?;
        let path = format!("/dev/loop{index}");
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags((OFlags::CLOEXEC | OFlags::NOFOLLOW).bits() as i32)
            .open(&path)?;
        let meta = fd.metadata()?;
        ensure!(
            meta.file_type().is_block_device() && meta.rdev() == rustix::fs::makedev(7, index),
            "loop device node identity mismatch"
        );
        ensure!(
            fs::read_to_string(format!("/sys/class/block/loop{index}/dev"))?.trim()
                == format!("7:{index}"),
            "loop sysfs identity mismatch"
        );
        if lifecycle_sys::loop_status(&fd)?.is_some() {
            continue;
        }
        match lifecycle_sys::loop_attach(&fd, &backing) {
            Err(rustix::io::Errno::BUSY) => continue,
            result => result?,
        }
        return Ok(path);
    }
    bail!("free loop device remained occupied after three attempts")
}

/// Remove only the old root's own filesystem, never a mount or symlink target.
pub fn reclaim_old_root(root: &Path, device: u64) -> Result<()> {
    fn remove(directory: &Path, device: u64, depth: usize, budget: &mut usize) -> Result<()> {
        ensure!(depth <= 64, "old root nesting exceeds bound");
        for entry in fs::read_dir(directory)? {
            ensure!(*budget > 0, "old root entry count exceeds bound");
            *budget -= 1;
            let path = entry?.path();
            let meta = fs::symlink_metadata(&path)?;
            if meta.dev() != device {
                continue;
            }
            if meta.is_dir() {
                remove(&path, device, depth + 1, budget)?;
                fs::remove_dir(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
    ensure!(
        fs::symlink_metadata(root)?.is_dir() && fs::metadata(root)?.dev() == device,
        "old root identity mismatch"
    );
    remove(root, device, 0, &mut 65536)
}

pub fn switch_root() -> Result<()> {
    ensure!(std::process::id() == 1, "switch-root requires PID1");
    super::validate_new_root(Path::new("/newroot"))?;
    let root = File::open("/")?;
    let kind = rustix::fs::fstatfs(&root)?.f_type;
    ensure!(
        kind == 0x858458f6 || kind == 0x01021994,
        "old root is not ramfs or tmpfs"
    );
    let device = root.metadata()?.dev();
    std::env::set_current_dir("/newroot")?;
    reclaim_old_root(Path::new("/"), device)?;
    for entry in fs::read_dir("/")? {
        ensure!(
            fs::symlink_metadata(entry?.path())?.dev() != device,
            "old root still contains startup files"
        );
    }
    eprintln!("mica-init: old root startup files reclaimed");
    rustix::mount::mount_move(".", "/")?;
    rustix::process::chroot(".")?;
    std::env::set_current_dir("/")?;
    drop(root);
    Err(Command::new("/sbin/init")
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .exec())
    .context("exec authenticated system init")
}

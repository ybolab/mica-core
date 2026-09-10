use mos_deploy::boot::startup::{self, LoopState};
use std::{collections::VecDeque, fs, path::PathBuf, process::Command};

fn argv(command: &Command) -> Vec<String> {
    assert_eq!(command.get_program(), "/bin/busybox");
    command
        .get_args()
        .map(|s| s.to_str().unwrap().into())
        .collect()
}

#[test]
fn explicit_mount_commands_supply_both_operands_without_fstab() {
    for (kind, options) in [
        ("devtmpfs", "nosuid,mode=0755"),
        ("proc", "nosuid,nodev,noexec"),
        ("sysfs", "nosuid,nodev,noexec"),
        ("tmpfs", "nosuid,nodev,mode=0755,size=32M"),
        ("ext4", "ro,nodev,nosuid,noexec"),
        ("squashfs", "ro,nodev"),
    ] {
        assert_eq!(
            argv(&startup::mount("source", "/target", kind, options)),
            ["mount", "-t", kind, "-o", options, "source", "/target"]
        );
    }
    assert_eq!(
        argv(&startup::bind(
            "/support/modules",
            "/newroot/usr/lib/modules"
        )),
        [
            "mount",
            "-o",
            "bind",
            "/support/modules",
            "/newroot/usr/lib/modules"
        ]
    );
    for options in [
        "remount,bind,ro,nodev,nosuid",
        "remount,bind,ro,nodev,nosuid,noexec",
        "remount,ro,nodev,nosuid,noexec",
    ] {
        assert_eq!(
            argv(&startup::remount("source", "/target", options)),
            ["mount", "-o", options, "source", "/target"]
        );
    }
    for dir in ["dev", "proc", "sys", "run"] {
        assert_eq!(
            argv(&startup::move_mount(
                &format!("/{dir}"),
                &format!("/newroot/{dir}")
            )),
            [
                "mount",
                "-o",
                "move",
                &format!("/{dir}"),
                &format!("/newroot/{dir}")
            ]
        );
    }
    assert_eq!(
        argv(&startup::switch_root()),
        ["switch_root", "/newroot", "/sbin/init"]
    );
}

fn bound(path: PathBuf) -> LoopState {
    LoopState {
        backing_file: path,
        read_only: true,
        offset: 0,
        size_limit: 0,
    }
}

#[test]
fn read_only_loop_binds_and_returns_the_queried_device() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("root image");
    fs::write(&image, "signed image").unwrap();
    let mut calls = Vec::new();
    let mut states = VecDeque::from([None, Some(bound(image.clone()))]);
    let device = startup::attach_read_only_loop(
        &image,
        |command| {
            let args = argv(&command);
            calls.push(args.clone());
            Ok(if args == ["losetup", "-f"] {
                "/dev/loop7".into()
            } else {
                String::new()
            })
        },
        |device| {
            assert_eq!(device, "/dev/loop7");
            Ok(states.pop_front().unwrap())
        },
    )
    .unwrap();
    assert_eq!(device, "/dev/loop7");
    assert_eq!(
        calls,
        [
            vec!["losetup", "-f"],
            vec!["losetup", "-r", "/dev/loop7", image.to_str().unwrap()]
        ]
    );
    assert!(states.is_empty());
}

#[test]
fn malformed_or_missing_loop_names_never_reach_binding() {
    let image = tempfile::NamedTempFile::new().unwrap();
    for name in [
        "",
        "/dev/loop",
        "/dev/loop-1",
        "/dev/loop00",
        "/dev/loop1\n/dev/loop2",
        "/dev/loop1 extra",
        "/dev/loop1/../sda",
        "/dev/sda",
        "/dev/loop4294967296",
    ] {
        let mut calls = 0;
        assert!(
            startup::attach_read_only_loop(
                image.path(),
                |command| {
                    assert_eq!(argv(&command), ["losetup", "-f"]);
                    calls += 1;
                    Ok(name.into())
                },
                |_| panic!("invalid device inspected")
            )
            .is_err(),
            "{name}"
        );
        assert_eq!(calls, 1);
    }
}

#[test]
fn raced_loop_is_never_detached_and_retry_is_bounded() {
    let image = tempfile::NamedTempFile::new().unwrap();
    let mut calls = Vec::new();
    let mut states = VecDeque::from([
        None,
        Some(bound("/another-owner".into())),
        None,
        Some(bound(image.path().into())),
    ]);
    let mut query = 0;
    let result = startup::attach_read_only_loop(
        image.path(),
        |command| {
            let args = argv(&command);
            calls.push(args.clone());
            if args == ["losetup", "-f"] {
                query += 1;
                return Ok(format!("/dev/loop{query}"));
            }
            assert_eq!(&args[..2], ["losetup", "-r"]);
            if query == 1 {
                anyhow::bail!("Device or resource busy");
            }
            Ok(String::new())
        },
        |_| Ok(states.pop_front().unwrap()),
    )
    .unwrap();
    assert_eq!(result, "/dev/loop2");
    assert_eq!(calls.len(), 4);
    assert!(states.is_empty());

    let mut attempts = 0;
    assert!(
        startup::attach_read_only_loop(
            image.path(),
            |command| {
                assert_eq!(argv(&command), ["losetup", "-f"]);
                attempts += 1;
                Ok("/dev/loop3".into())
            },
            |_| Ok(Some(bound("/another-owner".into())))
        )
        .is_err()
    );
    assert_eq!(attempts, 3);
}

#[test]
fn loop_setup_failure_and_unverified_associations_are_refused() {
    let image = tempfile::NamedTempFile::new().unwrap();
    let other = tempfile::NamedTempFile::new().unwrap();
    for state in [
        None,
        Some(bound(other.path().into())),
        Some(LoopState {
            read_only: false,
            ..bound(image.path().into())
        }),
        Some(LoopState {
            offset: 4096,
            ..bound(image.path().into())
        }),
        Some(LoopState {
            size_limit: 4096,
            ..bound(image.path().into())
        }),
    ] {
        let mut states = VecDeque::from([None, state]);
        assert!(
            startup::attach_read_only_loop(
                image.path(),
                |command| {
                    let args = argv(&command);
                    Ok(if args == ["losetup", "-f"] {
                        "/dev/loop4".into()
                    } else {
                        String::new()
                    })
                },
                |_| Ok(states.pop_front().unwrap())
            )
            .is_err()
        );
        assert!(states.is_empty());
    }
    let mut calls = 0;
    assert!(
        startup::attach_read_only_loop(
            image.path(),
            |command| {
                calls += 1;
                if argv(&command) == ["losetup", "-f"] {
                    return Ok("/dev/loop4".into());
                }
                anyhow::bail!("Permission denied")
            },
            |_| Ok(None)
        )
        .is_err()
    );
    assert_eq!(calls, 2);
    assert!(startup::inspect_loop("/dev/null").is_err());
}

#[test]
fn sysfs_loop_state_requires_read_only_whole_file_identity() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("loop")).unwrap();
    assert!(startup::read_loop_state(dir.path()).unwrap().is_none());
    fs::write(dir.path().join("loop/backing_file"), "/system/image\n").unwrap();
    assert!(startup::read_loop_state(dir.path()).is_err());
    fs::write(dir.path().join("ro"), "1\n").unwrap();
    fs::write(dir.path().join("loop/offset"), "0\n").unwrap();
    fs::write(dir.path().join("loop/sizelimit"), "0\n").unwrap();
    let state = startup::read_loop_state(dir.path()).unwrap().unwrap();
    assert!(state.read_only);
    assert_eq!(state.backing_file, PathBuf::from("/system/image"));
    fs::write(dir.path().join("ro"), "garbage\n").unwrap();
    assert!(startup::read_loop_state(dir.path()).is_err());
}

#[test]
fn target_init_resolves_inside_new_root_and_must_be_executable() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("sbin")).unwrap();
    fs::create_dir_all(root.path().join("usr/lib/systemd")).unwrap();
    symlink("/usr/lib/systemd/systemd", root.path().join("sbin/init")).unwrap();
    assert!(startup::validate_target_init(root.path()).is_err());
    let init = root.path().join("usr/lib/systemd/systemd");
    fs::write(&init, "init fixture").unwrap();
    assert!(startup::validate_target_init(root.path()).is_err());
    fs::set_permissions(&init, fs::Permissions::from_mode(0o755)).unwrap();
    startup::validate_target_init(root.path()).unwrap();
    // A directory on the old root is never a valid switch_root target.
    assert!(startup::validate_new_root(root.path()).is_err());
    fs::remove_file(root.path().join("sbin/init")).unwrap();
    symlink("/bin/sh", root.path().join("sbin/init")).unwrap();
    assert!(startup::validate_target_init(root.path()).is_err());
}

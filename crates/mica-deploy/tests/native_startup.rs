use mica_deploy::boot::startup::native::{self, Operation};
use rustix::mount::MountFlags;
use std::{
    fs,
    os::unix::fs::{MetadataExt, symlink},
};

#[test]
fn mount_flags_preserve_readonly_bind_and_fs_options_without_fstab() {
    let (flags, data) = native::mount_options("ro,nodev,nosuid,noexec").unwrap();
    assert_eq!(
        flags,
        MountFlags::RDONLY | MountFlags::NODEV | MountFlags::NOSUID | MountFlags::NOEXEC
    );
    assert_eq!(data.as_bytes(), b"");
    let (flags, data) =
        native::mount_options("nosuid,nodev,mode=0700,size=5242880,nr_inodes=8192").unwrap();
    assert_eq!(flags, MountFlags::NOSUID | MountFlags::NODEV);
    assert_eq!(data.as_bytes(), b"mode=0700,size=5242880,nr_inodes=8192");
    let (flags, _) = native::mount_options("remount,bind,ro,nodev,nosuid").unwrap();
    assert!(flags.contains(MountFlags::BIND | MountFlags::RDONLY));
    for options in [
        "",
        "defaults",
        "loop",
        "size=-1",
        "size=",
        "x-mount.mkdir",
        "ro;reboot",
    ] {
        assert!(native::mount_options(options).is_err(), "{options}");
    }
}

#[test]
fn typed_worker_has_one_executable_and_no_shell_or_arbitrary_program_field() {
    let command = native::command(Operation::Partition {
        uuid: "abababab-abab-abab-abab-abababababab".into(),
    })
    .unwrap();
    assert_eq!(command.get_program(), "/init");
    assert_eq!(command.get_args().next().unwrap(), "--startup-worker");
    assert!(serde_json::from_str::<Operation>(r#"{"Program":{"path":"/bin/sh"}}"#).is_err());
    assert!(
        serde_json::from_str::<Operation>(r#"{"Partition":{"uuid":"x","program":"sh"}}"#).is_err()
    );
}

#[test]
fn old_root_reclamation_removes_regular_files_and_symlinks_without_following() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), "outside").unwrap();
    fs::create_dir(root.path().join("bin")).unwrap();
    fs::write(root.path().join("bin/helper"), "startup").unwrap();
    symlink(outside.path(), root.path().join("outside")).unwrap();
    symlink("/", root.path().join("cycle")).unwrap();
    assert!(
        native::reclaim_old_root(root.path(), root.path().metadata().unwrap().dev() + 1).is_err()
    );
    native::reclaim_old_root(root.path(), root.path().metadata().unwrap().dev()).unwrap();
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    assert_eq!(
        fs::read_to_string(outside.path().join("keep")).unwrap(),
        "outside"
    );
}

#[test]
fn loop_and_dm_creation_refuse_regular_descriptors() {
    let file = tempfile::tempfile().unwrap();
    assert_eq!(
        lifecycle_sys::loop_free(&file).unwrap_err(),
        rustix::io::Errno::NOTTY
    );
    assert_eq!(
        lifecycle_sys::loop_attach(&file, &file).unwrap_err(),
        rustix::io::Errno::NOTTY
    );
    assert_eq!(
        lifecycle_sys::dm_create(&file, "mica-root", "MICA-test").unwrap_err(),
        rustix::io::Errno::NOTTY
    );
    assert_eq!(file.metadata().unwrap().len(), 0);
}

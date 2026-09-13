use mica_deploy::boot::startup;
use std::fs;

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

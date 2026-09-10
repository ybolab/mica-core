use mos_deploy::boot::copy_exitrd;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};

fn member(root: &Path, name: &str, mode: u32) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"retained payload").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn refuses(source: &Path, manifest: &str, reason: &str) {
    let target = tempfile::tempdir().unwrap();
    let error = copy_exitrd(source, target.path(), manifest).unwrap_err();
    assert!(error.to_string().contains(reason), "{error:#}");
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
}

#[test]
fn rejects_empty_duplicate_and_noncanonical_manifests_before_writes() {
    let source = tempfile::tempdir().unwrap();
    member(source.path(), "shutdown", 0o755);
    member(source.path(), "lib/libc.so", 0o644);
    for manifest in ["", "\n", "shutdown\nshutdown\n"] {
        refuses(source.path(), manifest, "exitrd manifest");
    }
    for name in [
        "lib//libc.so",
        "lib/./libc.so",
        "./shutdown",
        "shutdown/",
        "../shutdown",
        "/shutdown",
        "lib/../shutdown",
        "shutdown\r",
        "shutdown\0",
    ] {
        refuses(source.path(), &format!("shutdown\n{name}\n"), "exitrd path");
    }
}

#[test]
fn rejects_source_links_including_ancestors_and_broken_links() {
    let source = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    member(source.path(), "shutdown", 0o755);
    member(outside.path(), "libc.so", 0o644);
    symlink(outside.path(), source.path().join("lib")).unwrap();
    refuses(source.path(), "shutdown\nlib/libc.so\n", "exitrd member");
    symlink("missing", source.path().join("broken")).unwrap();
    refuses(source.path(), "shutdown\nbroken\n", "exitrd member");
    symlink("shutdown", source.path().join("alias")).unwrap();
    refuses(source.path(), "shutdown\nalias\n", "exitrd member");
    symlink(source.path(), outside.path().join("alias")).unwrap();
    refuses(&outside.path().join("alias"), "shutdown\n", "exitrd source");
    refuses(
        &outside.path().join("alias/subdir"),
        "shutdown\n",
        "exitrd source",
    );
}

#[test]
fn requires_an_empty_real_destination_and_never_overwrites_other_inodes() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    member(source.path(), "shutdown", 0o755);
    member(source.path(), "lib/libc.so", 0o644);
    member(outside.path(), "precious", 0o644);
    fs::write(outside.path().join("precious"), b"must not overwrite").unwrap();
    let original = fs::read(outside.path().join("precious")).unwrap();
    fs::hard_link(
        outside.path().join("precious"),
        target.path().join("shutdown"),
    )
    .unwrap();
    assert!(copy_exitrd(source.path(), target.path(), "shutdown\n").is_err());
    assert_eq!(fs::read(outside.path().join("precious")).unwrap(), original);
    fs::remove_file(target.path().join("shutdown")).unwrap();
    symlink(outside.path(), target.path().join("lib")).unwrap();
    assert!(copy_exitrd(source.path(), target.path(), "shutdown\nlib/libc.so\n").is_err());
    assert!(!target.path().join("shutdown").exists());
    assert!(!outside.path().join("libc.so").exists());
    symlink(target.path(), outside.path().join("target")).unwrap();
    assert!(copy_exitrd(source.path(), &outside.path().join("target"), "shutdown\n").is_err());
}

#[test]
fn validates_permissions_and_entrypoint_before_copying_any_member() {
    let source = tempfile::tempdir().unwrap();
    member(source.path(), "shutdown", 0o755);
    for mode in [0o000, 0o111, 0o666, 0o777, 0o4755, 0o2755] {
        member(source.path(), "lib/libc.so", mode);
        refuses(
            source.path(),
            "shutdown\nlib/libc.so\n",
            "exitrd member permissions",
        );
    }
    member(source.path(), "lib/libc.so", 0o644);
    refuses(source.path(), "lib/libc.so\n", "exitrd shutdown");
    member(source.path(), "shutdown", 0o644);
    refuses(source.path(), "shutdown\n", "exitrd shutdown");
}

#[test]
fn rejects_generated_nodes_and_file_directory_conflicts_before_writes() {
    let source = tempfile::tempdir().unwrap();
    member(source.path(), "shutdown", 0o755);
    for name in [
        "dev",
        "proc",
        "sys",
        "run",
        "oldroot",
        "etc",
        "etc/initrd-release",
    ] {
        let extra = tempfile::tempdir().unwrap();
        member(extra.path(), "shutdown", 0o755);
        member(extra.path(), name, 0o644);
        refuses(
            extra.path(),
            &format!("shutdown\n{name}\n"),
            "exitrd reserved path",
        );
    }
    member(source.path(), "lib", 0o644);
    refuses(
        source.path(),
        "shutdown\nlib\nlib/libc.so\n",
        "exitrd path conflict",
    );
}

#[test]
fn preserves_data_modes_and_materializes_runtime_directories() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    member(source.path(), "shutdown", 0o755);
    member(source.path(), "lib/arch/libc.so", 0o644);
    copy_exitrd(source.path(), target.path(), "shutdown\nlib/arch/libc.so\n").unwrap();
    for (name, mode) in [("shutdown", 0o755), ("lib/arch/libc.so", 0o644)] {
        assert_eq!(
            fs::read(target.path().join(name)).unwrap(),
            b"retained payload"
        );
        assert_eq!(
            fs::metadata(target.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            mode
        );
    }
    for name in ["dev", "proc", "sys", "run", "oldroot", "etc"] {
        assert!(target.path().join(name).is_dir());
    }
    assert_eq!(
        fs::read(target.path().join("etc/initrd-release")).unwrap(),
        b"ID=mos-exitrd\n"
    );
}

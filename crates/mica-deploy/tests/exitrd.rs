use mica_deploy::boot::copy_exitrd;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};

fn elf(needed: Option<&str>) -> Vec<u8> {
    let mut bytes = vec![0; 512];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16] = 3;
    bytes[18] = if cfg!(target_arch = "x86_64") {
        62
    } else {
        183
    };
    bytes[20] = 1;
    bytes[52] = 64;
    bytes[32] = 64;
    bytes[54] = 56;
    bytes[56] = 1;
    bytes[64] = 1;
    bytes[96..104].copy_from_slice(&512_u64.to_le_bytes());
    if let Some(needed) = needed {
        bytes[32] = 64;
        bytes[54] = 56;
        bytes[56] = 2;
        bytes[64] = 1;
        bytes[96..104].copy_from_slice(&512_u64.to_le_bytes());
        bytes[120] = 2;
        bytes[128] = 176;
        bytes[152] = 64;
        for (n, (tag, value)) in [
            (5_u64, 240_u64),
            (10, needed.len() as u64 + 2),
            (1, 1),
            (0, 0),
        ]
        .iter()
        .enumerate()
        {
            bytes[176 + n * 16..184 + n * 16].copy_from_slice(&tag.to_le_bytes());
            bytes[184 + n * 16..192 + n * 16].copy_from_slice(&value.to_le_bytes());
        }
        bytes[241..241 + needed.len()].copy_from_slice(needed.as_bytes());
    }
    bytes
}
fn native_library() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "usr/lib/x86_64-linux-gnu/libc.so.6"
    } else {
        "usr/lib/aarch64-linux-gnu/libc.so.6"
    }
}

fn member(root: &Path, name: &str, mode: u32) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, elf(None)).unwrap();
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
    let manifest = "shutdown\n";
    let capacity = mica_deploy::boot::exitrd_tmpfs_bytes(source.path(), manifest).unwrap();
    assert_eq!(capacity, (11 + 1) * 65536 + 1024 * 1024);
    copy_exitrd(source.path(), target.path(), manifest).unwrap();
    assert_eq!(
        fs::read(target.path().join("shutdown")).unwrap(),
        fs::read(source.path().join("shutdown")).unwrap()
    );
    assert_eq!(
        fs::metadata(target.path().join("shutdown"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
    for name in ["dev", "proc", "sys", "run", "oldroot", "etc", "backing"] {
        assert!(target.path().join(name).is_dir());
    }
    assert_eq!(
        fs::read(target.path().join("etc/initrd-release")).unwrap(),
        b"ID=mica-exitrd\n"
    );
}

#[test]
fn rejects_old_or_incomplete_layout_and_measures_retained_budget() {
    let source = tempfile::tempdir().unwrap();
    member(source.path(), "shutdown", 0o755);
    let target = tempfile::tempdir().unwrap();
    for name in ["bin/busybox", "sbin/dmsetup"] {
        member(source.path(), name, 0o755);
    }
    let manifest = "bin/busybox\nsbin/dmsetup\nshutdown\n";
    member(source.path(), "usr/lib/systemd/libsystemd-shared.so", 0o755);
    assert!(
        copy_exitrd(
            source.path(),
            target.path(),
            &format!("{manifest}usr/lib/systemd/libsystemd-shared.so\n")
        )
        .is_err()
    );
}

#[test]
fn rejects_missing_loader_library_foreign_elf_and_unneeded_files() {
    for case in [
        "missing-dependency",
        "wrong-architecture",
        "script",
        "unneeded",
        "interpreter",
        "program-table",
        "owner",
    ] {
        let source = tempfile::tempdir().unwrap();
        member(source.path(), "shutdown", 0o755);
        let mut manifest = "shutdown\n".to_owned();
        match case {
            "missing-dependency" => {
                fs::write(source.path().join("shutdown"), elf(Some("missing.so"))).unwrap()
            }
            "wrong-architecture" => {
                let mut bytes = elf(None);
                bytes[18] = 0;
                fs::write(source.path().join("shutdown"), bytes).unwrap();
            }
            "script" => fs::write(source.path().join("shutdown"), b"#!/bin/busybox sh\n").unwrap(),
            "unneeded" => {
                member(source.path(), native_library(), 0o755);
                manifest.push_str(native_library());
                manifest.push('\n');
            }
            "interpreter" => {
                let mut bytes = elf(None);
                bytes[56] = 2;
                bytes[120] = 3;
                bytes[128..136].copy_from_slice(&240_u64.to_le_bytes());
                bytes[152..160].copy_from_slice(&8_u64.to_le_bytes());
                bytes[240..248].copy_from_slice(b"/loader\0");
                fs::write(source.path().join("shutdown"), bytes).unwrap();
            }
            "program-table" => {
                let mut bytes = elf(None);
                bytes[56] = 0;
                fs::write(source.path().join("shutdown"), bytes).unwrap();
            }
            "owner" => rustix::fs::chown(
                source.path().join("shutdown"),
                Some(rustix::process::Uid::from_raw(65534)),
                None,
            )
            .unwrap(),
            _ => unreachable!(),
        }
        let target = tempfile::tempdir().unwrap();
        assert!(
            copy_exitrd(source.path(), target.path(), &manifest).is_err(),
            "{case}"
        );
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    }
}

#[test]
fn static_refinement_requires_exactly_one_static_shutdown() {
    let source = tempfile::tempdir().unwrap();
    member(source.path(), "shutdown", 0o755);
    let target = tempfile::tempdir().unwrap();
    copy_exitrd(source.path(), target.path(), "shutdown\n").unwrap();
    member(source.path(), "bin/busybox", 0o755);
    member(source.path(), "sbin/dmsetup", 0o755);
    refuses(
        source.path(),
        "shutdown\nbin/busybox\nsbin/dmsetup\n",
        "single",
    );
    fs::write(source.path().join("shutdown"), elf(Some("libc.so.6"))).unwrap();
    refuses(source.path(), "shutdown\n", "static");
}

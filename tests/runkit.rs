//! `mica-runkit` is one executable reached through links; the link name alone
//! selects the lifecycle entry point.
use std::{os::unix::fs::symlink, process::Command};

fn run(name: &str) -> (i32, String) {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join(name);
    symlink(env!("CARGO_BIN_EXE_mica-runkit"), &link).unwrap();
    let output = Command::new(&link).env_clear().output().unwrap();
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn init_link_selects_startup() {
    assert_eq!(run("init"), (1, "mica-init must run as PID 1\n".into()));
}

#[test]
fn shutdown_links_select_retained_shutdown() {
    for name in ["shutdown", "mica-shutdown"] {
        assert_eq!(run(name), (1, "mica-shutdown requires PID 1\n".into()));
    }
}

#[test]
fn any_other_name_refuses() {
    for name in ["mica-runkit", "initrd", "reboot"] {
        let (code, stderr) = run(name);
        assert_eq!(code, 2, "{name}");
        assert!(
            stderr.contains("must be invoked as init or shutdown"),
            "{name}: {stderr}"
        );
    }
}

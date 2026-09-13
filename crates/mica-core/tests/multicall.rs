//! `micad` is one binary for two daemons: the name it is invoked as selects
//! micad or apid, and any other name refuses.
use std::{os::unix::fs::symlink, process::Command};

fn run(name: &str, args: &[&str]) -> (bool, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join(name);
    symlink(env!("CARGO_BIN_EXE_micad"), &link).unwrap();
    let output = Command::new(&link).args(args).env_clear().output().unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn micad_name_selects_micad() {
    let (success, stdout, _) = run("micad", &["--version"]);
    assert!(success);
    assert!(stdout.starts_with("micad "), "{stdout}");
}

#[test]
fn apid_name_selects_apid() {
    let (success, stdout, _) = run("apid", &["--version"]);
    assert!(success);
    assert!(stdout.starts_with("apid "), "{stdout}");
    let (success, stdout, _) = run("apid", &["--openapi"]);
    assert!(success);
    assert!(stdout.starts_with('{'), "{stdout}");
}

#[test]
fn any_other_name_refuses() {
    for name in ["mica", "apidx", "mica-mqttd"] {
        let (success, stdout, stderr) = run(name, &["--version"]);
        assert!(!success, "{name}");
        assert!(stdout.is_empty(), "{name}: {stdout}");
        assert!(
            stderr.contains("must be invoked as micad or apid"),
            "{name}: {stderr}"
        );
    }
}

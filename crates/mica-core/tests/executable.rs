//! `micad` is the settings and reconciliation daemon and nothing else: the API
//! daemon is its own executable, `mica-apid`, so upgrading one never repacks the
//! other. Whatever name micad is started under, it is micad.
use std::{os::unix::fs::symlink, process::Command};

fn run(name: &str, args: &[&str]) -> (bool, String) {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join(name);
    symlink(env!("CARGO_BIN_EXE_micad"), &link).unwrap();
    let output = Command::new(&link).args(args).env_clear().output().unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

#[test]
fn micad_answers_as_micad() {
    let (success, stdout) = run("micad", &["--version"]);
    assert!(success);
    assert!(stdout.starts_with("micad "), "{stdout}");
}

#[test]
fn micad_does_not_carry_the_api_daemon() {
    for name in ["mica-apid", "apid"] {
        let (success, stdout) = run(name, &["--version"]);
        assert!(success, "{name}");
        assert!(stdout.starts_with("micad "), "{name}: {stdout}");
    }
}

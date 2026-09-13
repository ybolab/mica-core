//! `mica-apid` is its own executable, packaged on its own.
use std::process::Command;

fn run(args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_mica-apid"))
        .args(args)
        .env_clear()
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

#[test]
fn version_names_mica_apid() {
    let (success, stdout) = run(&["--version"]);
    assert!(success);
    assert!(stdout.starts_with("mica-apid "), "{stdout}");
}

#[test]
fn openapi_prints_the_document() {
    let (success, stdout) = run(&["--openapi"]);
    assert!(success);
    assert!(stdout.starts_with('{'), "{stdout}");
}

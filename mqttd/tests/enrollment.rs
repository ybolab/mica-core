//! Contract tests for the positive MQTT application enrollment boundary.

use mos_mqttd::enrollment::Enrollment;

#[test]
fn a_direct_mos_service_is_admitted_only_when_explicitly_enrolled() {
    let enrollment = Enrollment::from_names(["com.mos.sensor.abc123"])
        .expect("the application package enrolled a valid direct service name");

    let application = enrollment
        .application("com.mos.sensor.abc123")
        .expect("the exact enrolled name is admitted");
    assert_eq!(application.bus_name(), "com.mos.sensor.abc123");
    assert_eq!(application.class(), "sensor");
    assert!(
        enrollment.application("com.mos.sensor.other").is_none(),
        "a sibling name is not covered by an exact enrollment"
    );
}

#[test]
fn mosd_can_never_be_enrolled_in_the_mqtt_application_data_plane() {
    let error = Enrollment::from_names(["com.mos.mosd"])
        .expect_err("mosd must be structurally excluded from MQTT");

    assert!(
        error.to_string().contains("com.mos.mosd"),
        "the refusal should name the unsafe enrollment: {error}"
    );
}

#[test]
fn enrollment_rejects_wildcards_and_non_bus_names() {
    for name in ["com.mos.sensor.*", "com.mos.sensor.+", "com.mos.1sensor"] {
        assert!(
            Enrollment::from_names([name]).is_err(),
            "{name} must not become an exact application enrollment"
        );
    }
}

#[tokio::test]
async fn directory_entries_are_exact_enrollments() {
    let directory = tempfile::tempdir().expect("create enrollment directory");
    std::fs::write(directory.path().join("com.mos.sensor.abc123"), "ignored\n")
        .expect("write enrollment");

    let enrollment = Enrollment::load(directory.path())
        .await
        .expect("load application package enrollment");

    assert!(enrollment.application("com.mos.sensor.abc123").is_some());
    assert!(enrollment.application("com.mos.sensor.other").is_none());
}

#[tokio::test]
async fn a_directory_entry_cannot_enroll_mosd() {
    let directory = tempfile::tempdir().expect("create enrollment directory");
    std::fs::write(directory.path().join("com.mos.mosd"), "").expect("write unsafe enrollment");

    let error = Enrollment::load(directory.path())
        .await
        .expect_err("mosd must remain outside MQTT even when a file names it");

    assert!(error.to_string().contains("com.mos.mosd"));
}

#[cfg(unix)]
#[tokio::test]
async fn non_regular_directory_entries_fail_closed() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("create enrollment directory");
    symlink(
        directory.path().join("missing"),
        directory.path().join("com.mos.sensor.abc123"),
    )
    .expect("create enrollment symlink");

    let error = Enrollment::load(directory.path())
        .await
        .expect_err("symlinks cannot enroll an application");

    assert!(error.to_string().contains("not a regular file"));
}

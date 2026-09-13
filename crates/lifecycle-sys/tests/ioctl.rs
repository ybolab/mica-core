use lifecycle_sys::{
    clear_loop, loop_status, watchdog_keepalive, watchdog_support, watchdog_timeout,
};

#[test]
fn unrelated_regular_descriptor_returns_errno_without_writes_or_close() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"must remain intact").unwrap();
    assert_eq!(
        watchdog_support(&file).unwrap_err(),
        rustix::io::Errno::NOTTY
    );
    assert_eq!(
        watchdog_timeout(&file).unwrap_err(),
        rustix::io::Errno::NOTTY
    );
    assert_eq!(
        watchdog_keepalive(&file).unwrap_err(),
        rustix::io::Errno::NOTTY
    );
    assert_eq!(loop_status(&file).unwrap_err(), rustix::io::Errno::NOTTY);
    assert_eq!(clear_loop(&file).unwrap_err(), rustix::io::Errno::NOTTY);
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut data = String::new();
    file.read_to_string(&mut data).unwrap();
    assert_eq!(data, "must remain intact");
}

#[test]
fn device_mapper_requests_refuse_unrelated_descriptors() {
    let file = tempfile::tempfile().unwrap();
    assert_eq!(
        lifecycle_sys::dm_status(&file, 0xfd00).unwrap_err(),
        rustix::io::Errno::NOTTY
    );
    let status = lifecycle_sys::DmStatus {
        device: 0xfd00,
        name: "mica-root".into(),
        uuid: "CRYPT-VERITY-owned".into(),
        targets: 1,
        open_count: 0,
        event: 0,
    };
    assert_eq!(
        lifecycle_sys::dm_table(&file, &status).unwrap_err(),
        rustix::io::Errno::NOTTY
    );
    assert_eq!(
        lifecycle_sys::dm_remove(&file, &status).unwrap_err(),
        rustix::io::Errno::NOTTY
    );
    assert_eq!(file.metadata().unwrap().len(), 0);
}

use mos_deploy::fit_env::{
    ENV_OFFSETS, ENV_SIZE, Environment, Record, encode, parse_records, render_records,
};
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
};

fn records() -> Vec<Record> {
    vec![
        Record {
            id: "a".repeat(64),
            kernel_id: "c".repeat(64),
            generation: 2,
            tries_left: Some(3),
        },
        Record {
            id: "b".repeat(64),
            kernel_id: "c".repeat(64),
            generation: 1,
            tries_left: None,
        },
    ]
}

#[test]
fn boot_records_are_bounded_canonical_and_unambiguous() {
    let text = render_records(&records()).unwrap();
    assert_eq!(parse_records(&text).unwrap(), records());
    for bad in [
        text.replace(",2,", ",02,"),
        text.replace(",2,", ",9007199254740992,"),
        text.replace(",3;", ",4;"),
        text.replace(&"b".repeat(64), &"a".repeat(64)),
        format!("{text};{}", &text[3..]),
        text.to_uppercase(),
        "v1|".into(),
    ] {
        assert!(parse_records(&bad).is_err(), "accepted invalid boot record");
    }
}

#[test]
fn environment_crc_matches_an_independent_zlib_fixture() {
    // Python zlib.crc32 over the 65,531-byte, NUL-padded environment data.
    let bytes = encode(&records(), 7).unwrap();
    assert_eq!(&bytes[..4], &0x11c9_ba95_u32.to_le_bytes());
    assert_eq!(
        hex::encode(ring::digest::digest(&ring::digest::SHA256, &bytes)),
        "d838abdadeb95278a750625f26e8f018df50123da48b0e2970bd36453a689565"
    );
}

fn region() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("firmware.img");
    fs::write(&path, vec![0x55; 18 * 1048576 - 32768]).unwrap();
    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    for (slot, flag) in [255, 0].into_iter().enumerate() {
        file.seek(SeekFrom::Start(ENV_OFFSETS[slot])).unwrap();
        file.write_all(&encode(&records(), flag).unwrap()).unwrap();
    }
    file.sync_all().unwrap();
    (dir, path)
}

#[test]
fn redundant_environment_rolls_flags_and_writes_only_the_inactive_copy() {
    let (_dir, path) = region();
    let before = fs::read(&path).unwrap();
    let mut env = Environment::load(&path).unwrap();
    assert_eq!(env.slot, 1);
    env.records[0].tries_left = Some(2);
    env.save(&path).unwrap();
    let saved = Environment::load(&path).unwrap();
    assert_eq!(saved.slot, 0);
    assert_eq!(saved.flag, 1);
    assert_eq!(saved.records[0].tries_left, Some(2));
    let after = fs::read(&path).unwrap();
    let offset = ENV_OFFSETS[0] as usize;
    assert_eq!(before[..offset], after[..offset]);
    assert_eq!(before[offset + ENV_SIZE..], after[offset + ENV_SIZE..]);
}

#[test]
fn torn_or_invalid_environment_copies_never_refill_attempts() {
    let (_dir, path) = region();
    let mut bytes = fs::read(&path).unwrap();
    bytes[ENV_OFFSETS[1] as usize + 20] ^= 1;
    fs::write(&path, &bytes).unwrap();
    let valid = Environment::load(&path).unwrap();
    assert_eq!(valid.slot, 0);
    assert_eq!(valid.flag, 255);
    bytes[ENV_OFFSETS[0] as usize + 20] ^= 1;
    fs::write(&path, bytes).unwrap();
    assert!(Environment::load(&path).is_err());
}

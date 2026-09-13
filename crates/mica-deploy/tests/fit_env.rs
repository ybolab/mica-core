use mica_deploy::fit_env::{
    ENV_SIZE, Environment, FitLayout, Record, encode, parse_records, render_records,
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
        format!(
            "v1|{},{},3,3;{}",
            "d".repeat(64),
            "c".repeat(64),
            &text[3..]
        ),
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
    assert_eq!(&bytes[..4], &0xc280_0412_u32.to_le_bytes());
    assert_eq!(
        hex::encode(ring::digest::digest(&ring::digest::SHA256, &bytes)),
        "91f6bfd298a72e948eff9b04908c9c01487e102bc5a806a6827ba4e4954554a3"
    );
}

fn region() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("firmware.img");
    fs::write(&path, vec![0x55; 18 * 1048576 - 32768]).unwrap();
    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    for (slot, flag) in [255, 0].into_iter().enumerate() {
        file.seek(SeekFrom::Start(FitLayout::Cx3576.offsets()[slot]))
            .unwrap();
        file.write_all(&encode(&records(), flag).unwrap()).unwrap();
    }
    file.sync_all().unwrap();
    (dir, path)
}

#[test]
fn redundant_environment_rolls_flags_and_writes_only_the_inactive_copy() {
    let (_dir, path) = region();
    let before = fs::read(&path).unwrap();
    let mut env = Environment::load(&path, FitLayout::Cx3576).unwrap();
    assert_eq!(env.slot, 1);
    env.records[0].tries_left = Some(2);
    env.save(&path).unwrap();
    let saved = Environment::load(&path, FitLayout::Cx3576).unwrap();
    assert_eq!(saved.slot, 0);
    assert_eq!(saved.flag, 1);
    assert_eq!(saved.records[0].tries_left, Some(2));
    let after = fs::read(&path).unwrap();
    let offset = FitLayout::Cx3576.offsets()[0] as usize;
    assert_eq!(before[..offset], after[..offset]);
    assert_eq!(before[offset + ENV_SIZE..], after[offset + ENV_SIZE..]);
}

#[test]
fn torn_or_invalid_environment_copies_never_refill_attempts() {
    let (_dir, path) = region();
    let mut bytes = fs::read(&path).unwrap();
    bytes[FitLayout::Cx3576.offsets()[1] as usize + 20] ^= 1;
    fs::write(&path, &bytes).unwrap();
    let valid = Environment::load(&path, FitLayout::Cx3576).unwrap();
    assert_eq!(valid.slot, 0);
    assert_eq!(valid.flag, 255);
    bytes[FitLayout::Cx3576.offsets()[0] as usize + 20] ^= 1;
    fs::write(&path, bytes).unwrap();
    assert!(Environment::load(&path, FitLayout::Cx3576).is_err());
}

#[test]
fn s905x5m_records_preserve_all_vendor_ranges_and_reject_the_other_board_offsets() {
    let layout = FitLayout::S905x5m;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("firmware.img");
    let mut file = fs::File::create(&path).unwrap();
    file.set_len(layout.sectors() * 512).unwrap();
    for offset in [36 * 1048576 - 32768, 108 * 1048576 - 32768] {
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(b"vendor sentinel").unwrap();
    }
    for (slot, offset) in layout.offsets().into_iter().enumerate() {
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&encode(&records(), slot as u8).unwrap())
            .unwrap();
    }
    file.sync_all().unwrap();
    assert!(Environment::load(&path, FitLayout::Cx3576).is_err());
    let before = fs::read(&path).unwrap();
    let mut env = Environment::load(&path, layout).unwrap();
    env.records[0].tries_left = Some(2);
    env.save(&path).unwrap();
    assert_eq!(
        Environment::load(&path, layout).unwrap().records[0].tries_left,
        Some(2)
    );
    let after = fs::read(&path).unwrap();
    let offset = layout.offsets()[0] as usize;
    assert_eq!(before[..offset], after[..offset]);
    assert_eq!(before[offset + ENV_SIZE..], after[offset + ENV_SIZE..]);
}

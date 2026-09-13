//! GPT-only lookup for the image producer's fixed 512-byte sector layout.
use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags};
use std::{
    fs::{self, File},
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt},
};

#[derive(Debug, PartialEq, Eq)]
pub struct Partition {
    pub number: u32,
    pub uuid: String,
    pub start: u64,
    pub sectors: u64,
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn u32le(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes.get(at..at + 4).context("truncated GPT")?.try_into()?,
    ))
}
fn u64le(bytes: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        bytes.get(at..at + 8).context("truncated GPT")?.try_into()?,
    ))
}
pub fn valid_uuid(uuid: &str) -> bool {
    uuid.len() == 36
        && uuid.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

pub fn parse(bytes: &[u8], sectors: u64) -> Result<Vec<Partition>> {
    ensure!(
        bytes.len() == 34 * 512 && sectors > 67,
        "invalid bounded GPT disk"
    );
    ensure!(
        bytes[510..512] == [0x55, 0xaa]
            && bytes[450] == 0xee
            && bytes[446] == 0
            && u32le(bytes, 454)? == 1
            && bytes[462..510].iter().all(|b| *b == 0),
        "not a protective GPT partition table"
    );
    let h = &bytes[512..1024];
    let backup = u64le(h, 32)?;
    ensure!(
        &h[..8] == b"EFI PART"
            && u32le(h, 8)? == 0x00010000
            && u32le(h, 12)? == 92
            && u32le(h, 20)? == 0
            && u64le(h, 24)? == 1
            && (67..sectors).contains(&backup)
            && u64le(h, 72)? == 2
            && u32le(h, 80)? == 128
            && u32le(h, 84)? == 128,
        "unsupported GPT header or geometry"
    );
    let mut header = h[..92].to_vec();
    header[16..20].fill(0);
    ensure!(
        crc32(&header) == u32le(h, 16)? && crc32(&bytes[1024..]) == u32le(h, 88)?,
        "GPT checksum mismatch"
    );
    let first = u64le(h, 40)?;
    let last = u64le(h, 48)?;
    // A factory image can be written to a larger medium before DATA grows.
    // Its usable range must still exclude its original backup table/header.
    ensure!(
        first >= 34 && first <= last && last < backup - 32,
        "invalid GPT usable range"
    );
    let mut partitions: Vec<Partition> = Vec::new();
    for (i, entry) in bytes[1024..].as_chunks::<128>().0.iter().enumerate() {
        if entry[..16].iter().all(|b| *b == 0) {
            continue;
        }
        let start = u64le(entry, 32)?;
        let end = u64le(entry, 40)?;
        ensure!(
            start >= first && start <= end && end <= last && entry[16..32].iter().any(|b| *b != 0),
            "invalid GPT partition range or UUID"
        );
        let id = &entry[16..32];
        let uuid = format!(
            "{:08x}-{:04x}-{:04x}-{}-{}",
            u32le(id, 0)?,
            u16::from_le_bytes(id[4..6].try_into()?),
            u16::from_le_bytes(id[6..8].try_into()?),
            hex::encode(&id[8..10]),
            hex::encode(&id[10..16])
        );
        ensure!(
            partitions
                .iter()
                .all(|p| p.uuid != uuid && (end < p.start || start >= p.start + p.sectors)),
            "duplicate UUID or overlapping GPT partitions"
        );
        partitions.push(Partition {
            number: i as u32 + 1,
            uuid,
            start,
            sectors: end - start + 1,
        });
    }
    Ok(partitions)
}

pub fn find_partition(uuid: &str) -> Result<String> {
    ensure!(valid_uuid(uuid), "invalid GPT PARTUUID");
    let wanted = uuid.to_ascii_lowercase();
    let mut matches = Vec::new();
    for (count, entry) in fs::read_dir("/sys/class/block")?.enumerate() {
        ensure!(count < 1024, "excessive block device count");
        let path = entry?.path();
        if path.join("partition").exists() || !path.join("device").exists() {
            continue;
        }
        if fs::read_to_string(path.join("queue/logical_block_size"))?.trim() != "512" {
            continue;
        }
        let disk_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("invalid disk name")?;
        let device = format!("/dev/{disk_name}");
        let mut disk = File::from(rustix::fs::open(
            &device,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?);
        let meta = disk.metadata()?;
        ensure!(
            meta.file_type().is_block_device()
                && fs::read_to_string(path.join("dev"))?.trim()
                    == format!(
                        "{}:{}",
                        rustix::fs::major(meta.rdev()),
                        rustix::fs::minor(meta.rdev())
                    ),
            "GPT disk node identity mismatch"
        );
        let sectors: u64 = fs::read_to_string(path.join("size"))?.trim().parse()?;
        let mut bytes = vec![0; 34 * 512];
        if disk.read_exact(&mut bytes).is_err() {
            continue;
        }
        let Ok(partitions) = parse(&bytes, sectors) else {
            continue;
        };
        for p in partitions.into_iter().filter(|p| p.uuid == wanted) {
            for child in fs::read_dir(&path)? {
                let child = child?.path();
                if !child.join("partition").exists() {
                    continue;
                }
                if fs::read_to_string(child.join("partition"))?
                    .trim()
                    .parse::<u32>()?
                    != p.number
                {
                    continue;
                }
                ensure!(
                    fs::read_to_string(child.join("start"))?
                        .trim()
                        .parse::<u64>()?
                        == p.start
                        && fs::read_to_string(child.join("size"))?
                            .trim()
                            .parse::<u64>()?
                            == p.sectors,
                    "kernel partition disagrees with GPT"
                );
                let name = child
                    .file_name()
                    .and_then(|s| s.to_str())
                    .context("invalid partition name")?;
                let device = format!("/dev/{name}");
                let meta = fs::symlink_metadata(&device)?;
                ensure!(
                    meta.file_type().is_block_device()
                        && fs::read_to_string(child.join("dev"))?.trim()
                            == format!(
                                "{}:{}",
                                rustix::fs::major(meta.rdev()),
                                rustix::fs::minor(meta.rdev())
                            ),
                    "GPT partition node identity mismatch"
                );
                matches.push(device);
            }
        }
    }
    ensure!(matches.len() == 1, "GPT PARTUUID not found uniquely");
    Ok(matches.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<u8> {
        let mut b = vec![0; 34 * 512];
        b[510..512].copy_from_slice(&[0x55, 0xaa]);
        b[450] = 0xee;
        b[454..458].copy_from_slice(&1_u32.to_le_bytes());
        b[512..520].copy_from_slice(b"EFI PART");
        for (at, v) in [(8, 0x10000_u32), (12, 92), (80, 128), (84, 128)] {
            b[512 + at..516 + at].copy_from_slice(&v.to_le_bytes());
        }
        for (at, v) in [(24, 1_u64), (32, 999), (40, 34), (48, 966), (72, 2)] {
            b[512 + at..520 + at].copy_from_slice(&v.to_le_bytes());
        }
        b[1024] = 1;
        b[1040..1056].fill(0xab);
        b[1056..1064].copy_from_slice(&34_u64.to_le_bytes());
        b[1064..1072].copy_from_slice(&99_u64.to_le_bytes());
        checksums(&mut b);
        b
    }
    fn checksums(b: &mut [u8]) {
        let entries_crc = crc32(&b[1024..]);
        b[600..604].copy_from_slice(&entries_crc.to_le_bytes());
        b[528..532].fill(0);
        let crc = crc32(&b[512..604]);
        b[528..532].copy_from_slice(&crc.to_le_bytes());
    }
    #[test]
    fn gpt_only_geometry_crc_and_uuid_are_binding() {
        let good = fixture();
        let partitions = parse(&good, 1000).unwrap();
        assert_eq!(
            partitions[0],
            Partition {
                number: 1,
                uuid: "abababab-abab-abab-abab-abababababab".into(),
                start: 34,
                sectors: 66
            }
        );
        for at in [450, 510, 512, 528, 1056, 600] {
            let mut b = good.clone();
            b[at] ^= 1;
            assert!(parse(&b, 1000).is_err());
        }
        assert!(parse(&good[..good.len() - 1], 1000).is_err());
        assert!(parse(&good, 999).is_err());
        let mut duplicate = good.clone();
        duplicate.copy_within(1024..1152, 1152);
        checksums(&mut duplicate);
        assert!(parse(&duplicate, 1000).is_err());
        let mut mbr = vec![0; good.len()];
        mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
        mbr[450] = 0x83;
        assert!(parse(&mbr, 1000).is_err());
    }

    #[test]
    fn factory_gpt_on_larger_media_preserves_partition_identity() {
        let factory = fixture();
        let original = parse(&factory, 1000).unwrap();
        for sectors in [1001, 2000, 8_388_608] {
            assert_eq!(parse(&factory, sectors).unwrap(), original);
        }
    }

    #[test]
    fn larger_media_does_not_extend_the_declared_gpt_range() {
        let good = fixture();
        for backup in [0_u64, 1, 33, 66, 2000, u64::MAX] {
            let mut b = good.clone();
            b[544..552].copy_from_slice(&backup.to_le_bytes());
            checksums(&mut b);
            assert!(parse(&b, 2000).is_err(), "backup LBA {backup}");
        }
        for last in [967_u64, 998, 999, 1500, u64::MAX] {
            let mut b = good.clone();
            b[560..568].copy_from_slice(&last.to_le_bytes());
            checksums(&mut b);
            assert!(parse(&b, 2000).is_err(), "last usable LBA {last}");
        }
        let mut outside = good.clone();
        outside[1064..1072].copy_from_slice(&967_u64.to_le_bytes());
        checksums(&mut outside);
        assert!(parse(&outside, 2000).is_err());
    }

    #[test]
    fn larger_media_still_requires_crc_unique_uuid_and_nonoverlap() {
        let good = fixture();
        for at in [450, 510, 512, 528, 1056, 600] {
            let mut b = good.clone();
            b[at] ^= 1;
            assert!(parse(&b, 2000).is_err());
        }
        for unique_uuid in [false, true] {
            let mut b = good.clone();
            b.copy_within(1024..1152, 1152);
            if unique_uuid {
                b[1168] ^= 1;
            } else {
                b[1184..1192].copy_from_slice(&100_u64.to_le_bytes());
                b[1192..1200].copy_from_slice(&150_u64.to_le_bytes());
            }
            checksums(&mut b);
            assert!(parse(&b, 2000).is_err());
        }
    }
}

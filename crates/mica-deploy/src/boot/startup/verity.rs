//! Signed no-superblock geometry translated to the kernel's one verity target.
use crate::components::VerityImage;
use anyhow::{Context, Result, ensure};
use lifecycle_sys::DmTarget;
use linux_keyutils::{KeyRing, KeyRingIdentifier};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::Path,
};

pub fn table(major: u32, minor: u32, description: &str, image: &VerityImage) -> Result<DmTarget> {
    let g = &image.verity;
    let digest = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    ensure!(
        major == 7
            && !description.is_empty()
            && description.len() <= 128
            && description.bytes().all(|b| b.is_ascii_graphic()),
        "invalid verity provider or signature description"
    );
    ensure!(
        g.version == 1
            && g.algorithm == "sha256"
            && g.data_block_size == 4096
            && g.hash_block_size == 4096
            && g.data_blocks > 0
            && g.data_blocks.checked_mul(4096) == Some(g.hash_offset)
            && g.hash_offset < image.image.bytes
            && g.hash_offset.is_multiple_of(4096)
            && digest(&g.salt)
            && digest(&image.root_hash),
        "invalid signed no-superblock geometry"
    );
    Ok(DmTarget {
        sector: 0,
        length: g
            .data_blocks
            .checked_mul(8)
            .context("verity sector overflow")?,
        kind: "verity".into(),
        parameters: format!(
            "1 {major}:{minor} {major}:{minor} 4096 4096 {} {} sha256 {} {} 3 panic_on_corruption root_hash_sig_key_desc {description}",
            g.data_blocks,
            g.hash_offset / 4096,
            image.root_hash,
            g.salt
        ),
    })
}

/// Compare the full table, including target, geometry, root, signature and panic.
/// Read-only enforcement is independently required by lifecycle_sys::dm_status.
pub fn verify_table(expected: &DmTarget, targets: &[DmTarget]) -> Result<()> {
    ensure!(
        targets == [expected.clone()],
        "kernel verity table differs from authenticated read-only request"
    );
    Ok(())
}

fn control() -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(
            (rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK)
                .bits() as i32,
        )
        .open("/dev/mapper/control")?;
    let meta = file.metadata()?;
    ensure!(
        meta.file_type().is_char_device()
            && fs::read_to_string("/sys/class/misc/device-mapper/dev")?.trim()
                == format!(
                    "{}:{}",
                    rustix::fs::major(meta.rdev()),
                    rustix::fs::minor(meta.rdev())
                ),
        "device-mapper control descriptor identity mismatch"
    );
    Ok(file)
}

/// Called in a bounded PID1-owned worker. The process keyring disappears when
/// the worker exits; the kernel authenticates the PKCS#7 before table load returns.
pub fn open(loop_device: &str, name: &str, signature: &Path, image: &VerityImage) -> Result<()> {
    ensure!(
        ["mica-root", "mica-support"].contains(&name),
        "invalid startup mapping name"
    );
    let meta = fs::symlink_metadata(loop_device)?;
    ensure!(
        meta.file_type().is_block_device(),
        "verity provider is not a block device"
    );
    let signature_file = File::from(rustix::fs::open(
        signature,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    ensure!(
        signature_file.metadata()?.is_file(),
        "signature is not a regular file"
    );
    let mut bytes = Vec::new();
    signature_file.take(65537).read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= 65536,
        "invalid bounded root signature"
    );
    image.signature.verify(&bytes)?;
    // No-superblock cryptsetup has no UUID and uses cryptsetup:<mapping name>.
    // Both data/hash providers use the verified dev_t, never a parsed filename.
    let description = format!("cryptsetup:{name}");
    let expected = table(
        rustix::fs::major(meta.rdev()),
        rustix::fs::minor(meta.rdev()),
        &description,
        image,
    )?;
    let ring = KeyRing::from_special_id(KeyRingIdentifier::Process, true)
        .context("create signature keyring")?;
    let key = ring
        .add_key(&description, &bytes)
        .context("insert root hash signature key")?;
    let fd = control()?;
    let uuid = format!(
        "MICA-VERITY-{}-{name}",
        fs::read_to_string("/proc/sys/kernel/random/uuid")?.trim()
    );
    let created = lifecycle_sys::dm_create(&fd, name, &uuid).context("create verity device")?;
    let activated = (|| -> Result<()> {
        lifecycle_sys::dm_load_verity(&fd, &created, &expected)
            .context("authenticated verity table load refused")?;
        let status =
            lifecycle_sys::dm_resume(&fd, &created).context("resume read-only verity device")?;
        verify_table(&expected, &lifecycle_sys::dm_table(&fd, &status)?)?;
        ensure!(
            lifecycle_sys::dm_status(&fd, created.device)? == status,
            "verity identity changed during readback"
        );
        Ok(())
    })();
    // The target retained the authenticated signature; no userspace key remains.
    let revoked = key.revoke().context("revoke loaded signature key");
    let finalized = activated.and(revoked).and_then(|()| {
        // devtmpfs provides dm-N. Creating the conventional alias does not rely on
        // udev and refuses an existing filesystem object rather than overwriting it.
        let node = format!("/dev/dm-{}", rustix::fs::minor(created.device));
        let actual = fs::symlink_metadata(&node)?;
        ensure!(
            actual.file_type().is_block_device() && actual.rdev() == created.device,
            "new DM node identity mismatch"
        );
        std::os::unix::fs::symlink(node, format!("/dev/mapper/{name}"))?;
        Ok(())
    });
    if finalized.is_err() {
        lifecycle_sys::dm_discard_created(&fd, &created)
            .context("rollback of newly created verity mapping failed")?;
    }
    finalized
}

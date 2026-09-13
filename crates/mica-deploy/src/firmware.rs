//! Independent signed firmware maintenance manifests. No deployment can carry one.
use crate::components::{Artifact, authenticate_payload, component_id};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Firmware {
    pub schema: String,
    pub id: String,
    pub board: String,
    pub arch: String,
    pub generation: u64,
    pub version: String,
    pub artifact: Artifact,
    pub target: Target,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "format",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Target {
    Efi { partition: u8, path: String },
    RockchipLoader { disk_offset: u64, max_bytes: u64 },
    AmlogicBoot0 { payload_offset: u64, max_bytes: u64 },
}

pub fn parse_firmware(payload: &[u8]) -> Result<Firmware> {
    ensure!(payload.len() <= 4096, "firmware manifest exceeds limit");
    let firmware: Firmware = serde_json::from_slice(payload)?;
    let value = serde_json::to_value(&firmware)?;
    ensure!(
        serde_json::to_vec(&value)? == payload,
        "noncanonical firmware manifest"
    );
    ensure!(
        firmware.schema == "mos/firmware/v1",
        "unsupported firmware schema"
    );
    let arch = match firmware.board.as_str() {
        "x64" => "amd64",
        "virt-arm64" | "cx3576" | "s905x5m" => "arm64",
        _ => anyhow::bail!("unsupported firmware board"),
    };
    ensure!(firmware.arch == arch, "firmware architecture mismatch");
    ensure!(
        (1..=9_007_199_254_740_991).contains(&firmware.generation),
        "invalid firmware generation"
    );
    ensure!(
        !firmware.version.is_empty()
            && firmware.version.len() <= 128
            && firmware.version.as_bytes()[0].is_ascii_alphanumeric()
            && firmware
                .version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b)),
        "invalid firmware version"
    );
    ensure!(
        component_id(&value)? == firmware.id,
        "firmware identity mismatch"
    );
    ensure!(
        firmware.artifact.sha256.len() == 64
            && firmware
                .artifact
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid firmware digest"
    );
    let limit = match &firmware.target {
        Target::AmlogicBoot0 {
            payload_offset,
            max_bytes,
        } => {
            ensure!(
                firmware.board == "s905x5m" && *payload_offset == 512 && *max_bytes == 4193792,
                "invalid Amlogic boot0 payload"
            );
            4193792
        }
        Target::RockchipLoader {
            disk_offset,
            max_bytes,
        } => {
            ensure!(
                firmware.board == "cx3576" && *disk_offset == 32768 && *max_bytes == 16744448,
                "invalid loader write range"
            );
            16744448
        }
        Target::Efi { partition, path } => {
            let name = match firmware.board.as_str() {
                "x64" => "BOOTX64.EFI",
                "virt-arm64" => "BOOTAA64.EFI",
                _ => anyhow::bail!("invalid EFI board"),
            };
            ensure!(
                *partition == 1 && path == &format!("EFI/BOOT/{name}"),
                "invalid EFI destination"
            );
            4 * 1048576
        }
    };
    ensure!(
        (1..=limit).contains(&firmware.artifact.bytes),
        "invalid firmware length"
    );
    Ok(firmware)
}

pub fn authenticate_firmware(bytes: &[u8], keys: &[[u8; 32]]) -> Result<Firmware> {
    parse_firmware(&authenticate_payload(bytes, keys, 4096)?)
}

/// Read back the authenticated destination without touching loader or counters.
pub fn verify_installed(manifest: &Firmware, boot: &crate::deployments::BootBackend) -> Result<()> {
    use crate::deployments::{BootBackend, read_bounded};
    use std::{fs::File, io::Read};
    let bytes = match (&manifest.target, boot) {
        (Target::Efi { path, .. }, BootBackend::Uefi { esp }) => {
            read_bounded(&esp.join(path), manifest.artifact.bytes)?
        }
        (
            Target::RockchipLoader { .. },
            BootBackend::Fit {
                firmware,
                layout: crate::fit_env::FitLayout::Cx3576,
            },
        ) => {
            // FIRMWARE partition 1 starts at the manifest's absolute disk offset.
            let mut bytes = Vec::new();
            File::open(firmware)?
                .take(manifest.artifact.bytes)
                .read_to_end(&mut bytes)?;
            bytes
        }
        (
            Target::AmlogicBoot0 { .. },
            BootBackend::Fit {
                layout: crate::fit_env::FitLayout::S905x5m,
                ..
            },
        ) => {
            return verify_boot0_payload(manifest, &amlogic_boot0_device()?);
        }
        _ => anyhow::bail!("firmware target differs from the boot backend"),
    };
    manifest.artifact.verify(&bytes)?;
    Ok(())
}

/// Compare only the signed payload; the vendor creates the preceding boot header.
pub fn verify_boot0_payload(manifest: &Firmware, device: &std::path::Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let Target::AmlogicBoot0 { payload_offset, .. } = manifest.target else {
        anyhow::bail!("expected Amlogic boot0 firmware");
    };
    let mut file = std::fs::File::open(device)?;
    file.seek(SeekFrom::Start(payload_offset))?;
    let mut bytes = Vec::new();
    file.take(manifest.artifact.bytes).read_to_end(&mut bytes)?;
    manifest.artifact.verify(&bytes)?;
    Ok(())
}

fn amlogic_boot0_device() -> Result<std::path::PathBuf> {
    use std::{fs, os::unix::fs::FileTypeExt, path::Path};
    let mut found = Vec::new();
    for entry in fs::read_dir("/sys/class/block")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name
            .strip_prefix("mmcblk")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
            || !fs::read_to_string(entry.path().join("device/type"))
                .is_ok_and(|kind| kind.trim() == "MMC")
        {
            continue;
        }
        let device = Path::new("/dev").join(format!("{name}boot0"));
        if device
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_block_device())
        {
            found.push(device);
        }
    }
    ensure!(found.len() == 1, "eMMC boot0 absent or ambiguous");
    Ok(found.remove(0))
}

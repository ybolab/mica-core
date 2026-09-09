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
        "virt-arm64" | "cx3576" => "arm64",
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
        (Target::RockchipLoader { .. }, BootBackend::Fit { firmware }) => {
            // FIRMWARE partition 1 starts at the manifest's absolute disk offset.
            let mut bytes = Vec::new();
            File::open(firmware)?
                .take(manifest.artifact.bytes)
                .read_to_end(&mut bytes)?;
            bytes
        }
        _ => anyhow::bail!("firmware target differs from the boot backend"),
    };
    manifest.artifact.verify(&bytes)?;
    Ok(())
}

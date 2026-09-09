//! Bounded boot selection and the single signed dm-verity invocation.

use crate::components::VerityImage;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Component, Path},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootKind {
    Uefi,
    UbootFit,
}

impl BootKind {
    pub fn for_board(board: &str) -> anyhow::Result<Self> {
        match board {
            "x64" | "virt-arm64" => Ok(Self::Uefi),
            "cx3576" => Ok(Self::UbootFit),
            _ => anyhow::bail!("unsupported boot backend board"),
        }
    }
}

pub fn fit_selected(property: &[u8]) -> anyhow::Result<String> {
    anyhow::ensure!(
        property.len() == 65 && property[64] == 0,
        "invalid FIT selection property"
    );
    let id = std::str::from_utf8(&property[..64])?;
    crate::deployments::valid_id(id)?;
    Ok(id.to_owned())
}

/// Seed identity on physical DATA before starting systemd. Corrupt or redirected
/// state requires recovery; it must never mint a replacement identity silently.
pub fn persistent_machine_id(
    state: &Path,
    generate: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    use std::os::unix::fs::DirBuilderExt;
    match state.symlink_metadata() {
        Ok(metadata) => anyhow::ensure!(metadata.is_dir(), "identity state is not a directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(state)?;
            fs::File::open(
                state
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("state has no parent"))?,
            )?
            .sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    let path = state.join("machine-id");
    let valid = |id: &str| {
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && id.bytes().any(|b| b != b'0')
    };
    match path.symlink_metadata() {
        Ok(_) => {
            let bytes = crate::deployments::read_bounded(&path, 33)?;
            let id = std::str::from_utf8(&bytes)?
                .strip_suffix('\n')
                .unwrap_or(std::str::from_utf8(&bytes)?);
            anyhow::ensure!(valid(id), "invalid persistent machine identity");
            Ok(id.to_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let id = generate()?;
            anyhow::ensure!(valid(&id), "invalid generated machine identity");
            crate::deployments::atomic_write(&path, format!("{id}\n").as_bytes())?;
            Ok(id)
        }
        Err(error) => Err(error.into()),
    }
}

/// Preserve the signed, build-time ELF closure across deletion of the initramfs.
pub fn copy_exitrd(source: &Path, destination: &Path, manifest: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        manifest.len() <= 8192 && manifest.lines().count() <= 128,
        "excessive exitrd manifest"
    );
    let mut bytes = 0_u64;
    for name in manifest.lines() {
        let path = Path::new(name);
        anyhow::ensure!(
            !name.is_empty()
                && path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "invalid exitrd path"
        );
        let metadata = fs::symlink_metadata(source.join(path))?;
        anyhow::ensure!(metadata.is_file(), "exitrd member is not a file");
        bytes = bytes
            .checked_add(metadata.len())
            .ok_or_else(|| anyhow::anyhow!("exitrd size overflow"))?;
        anyhow::ensure!(bytes <= 32 * 1024 * 1024, "excessive exitrd size");
    }
    for name in manifest.lines() {
        let target = destination.join(name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source.join(name), target)?;
    }
    for name in ["dev", "proc", "sys", "run", "oldroot", "etc"] {
        fs::create_dir_all(destination.join(name))?;
    }
    fs::write(destination.join("etc/initrd-release"), b"ID=mos-exitrd\n")?;
    Ok(())
}

#[derive(Debug)]
pub struct BootError(&'static str);
impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for BootError {}

pub fn selected_entry(entry: &str) -> Result<String, BootError> {
    let stem = entry
        .strip_prefix("mos-")
        .and_then(|s| s.strip_suffix(".conf"))
        .ok_or(BootError("invalid deployment entry"))?;
    let (id, counter) = stem
        .split_once('+')
        .map_or((stem, None), |(id, count)| (id, Some(count)));
    if id.len() != 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(BootError("invalid deployment ID"));
    }
    if let Some(counter) = counter
        && !["3", "2-1", "1-2", "0-3"].contains(&counter)
    {
        return Err(BootError("invalid deployment trial counter"));
    }
    Ok(id.to_owned())
}

pub fn utf16_variable(bytes: &[u8]) -> Result<String, BootError> {
    if bytes.len() < 6 || bytes.len() > 512 || !bytes.len().is_multiple_of(2) {
        return Err(BootError("invalid EFI variable length"));
    }
    let mut words: Vec<u16> = bytes[4..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    if words.pop() != Some(0) || words.contains(&0) {
        return Err(BootError("invalid EFI variable termination"));
    }
    String::from_utf16(&words).map_err(|_| BootError("invalid EFI UTF-16"))
}

pub fn verity_args(
    loop_device: &str,
    name: &str,
    signature: &str,
    image: &VerityImage,
) -> Vec<String> {
    vec![
        "open".into(),
        loop_device.into(),
        name.into(),
        loop_device.into(),
        image.root_hash.clone(),
        "--no-superblock".into(),
        "--format".into(),
        "1".into(),
        "--hash".into(),
        "sha256".into(),
        "--data-block-size".into(),
        "4096".into(),
        "--hash-block-size".into(),
        "4096".into(),
        "--data-blocks".into(),
        image.verity.data_blocks.to_string(),
        "--hash-offset".into(),
        image.verity.hash_offset.to_string(),
        "--salt".into(),
        image.verity.salt.clone(),
        "--root-hash-signature".into(),
        signature.into(),
        "--panic-on-corruption".into(),
    ]
}

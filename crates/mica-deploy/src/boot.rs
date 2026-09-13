//! Bounded boot selection and the single signed dm-verity invocation.

pub mod shutdown;
pub mod startup;

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Seek, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
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
            "cx3576" | "s905x5m" => Ok(Self::UbootFit),
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

/// Copy the signed build-time closure into a fresh, empty exitrd filesystem.
/// Validate every member before writing; input and destination must be private
/// to PID 1 throughout copying, as they are before the systemd handoff.
pub fn copy_exitrd(source: &Path, destination: &Path, manifest: &str) -> anyhow::Result<()> {
    materialize_exitrd(source, Some(destination), manifest).map(|_| ())
}

/// Capacity is a limit, not allocation/RSS: round each retained member and
/// directory to a 64 KiB page, plus 1 MiB for bounded runtime record/scratch.
pub fn exitrd_tmpfs_bytes(source: &Path, manifest: &str) -> anyhow::Result<u64> {
    materialize_exitrd(source, None, manifest)
}

fn materialize_exitrd(
    source: &Path,
    destination: Option<&Path>,
    manifest: &str,
) -> anyhow::Result<u64> {
    use anyhow::{Context, ensure};
    use rustix::fs::{CWD, Dir, Mode, OFlags, ResolveFlags, mkdirat, openat2};
    const RUNTIME_DIRS: [&str; 7] = ["dev", "proc", "sys", "run", "oldroot", "etc", "backing"];
    const RELEASE: &str = "etc/initrd-release";

    anyhow::ensure!(
        !manifest.is_empty() && manifest.len() <= 8192 && manifest.lines().count() <= 128,
        "invalid or excessive exitrd manifest"
    );
    ensure!(!manifest.contains(['\0', '\r']), "invalid exitrd path");
    let mut names = BTreeSet::new();
    for name in manifest.lines() {
        ensure!(!name.is_empty(), "empty exitrd manifest member");
        ensure!(
            name.split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
            "invalid exitrd path"
        );
        ensure!(names.insert(name), "duplicate exitrd manifest member");
        ensure!(
            !RUNTIME_DIRS.contains(&name) && name != RELEASE && name != "storage.json",
            "exitrd reserved path"
        );
    }
    ensure!(names.contains("shutdown"), "missing exitrd shutdown");
    let mut directories: BTreeSet<&Path> = RUNTIME_DIRS.iter().map(Path::new).collect();
    for &name in &names {
        for parent in Path::new(name)
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            ensure!(
                !names.contains(parent.to_str().context("invalid exitrd path")?),
                "exitrd path conflict"
            );
            ensure!(parent != Path::new(RELEASE), "exitrd reserved path");
            directories.insert(parent);
        }
    }
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let source = fs::File::from(
        openat2(
            CWD,
            source,
            directory_flags,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )
        .context("invalid exitrd source")?,
    );
    let destination = destination
        .map(|path| -> anyhow::Result<fs::File> {
            let directory = fs::File::from(
                openat2(
                    CWD,
                    path,
                    directory_flags,
                    Mode::empty(),
                    ResolveFlags::NO_SYMLINKS,
                )
                .context("invalid exitrd destination")?,
            );
            for entry in Dir::read_from(&directory)? {
                let entry = entry?;
                ensure!(
                    entry.file_name() == c"." || entry.file_name() == c"..",
                    "exitrd destination is not empty"
                );
            }
            Ok(directory)
        })
        .transpose()?;
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS;
    let mut members = Vec::new();
    let mut bytes = 0_u64;
    let mut capacity = (directories.len() as u64 + 4) * 65536 + 1024 * 1024;
    for &name in &names {
        // NONBLOCK prevents a substituted FIFO from blocking before fstat.
        let file = fs::File::from(
            openat2(
                &source,
                name,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
                resolve,
            )
            .with_context(|| format!("invalid exitrd member: {name}"))?,
        );
        let metadata = file.metadata()?;
        ensure!(
            metadata.uid() == 0 && metadata.gid() == 0,
            "invalid exitrd member owner"
        );
        ensure!(metadata.is_file(), "exitrd member is not a file");
        let mode = metadata.permissions().mode();
        ensure!(
            mode & 0o7022 == 0 && mode & 0o400 != 0,
            "invalid exitrd member permissions"
        );
        ensure!(
            name != "shutdown" || mode & 0o100 != 0,
            "exitrd shutdown is not executable"
        );
        bytes = bytes
            .checked_add(metadata.len())
            .ok_or_else(|| anyhow::anyhow!("exitrd size overflow"))?;
        ensure!(bytes <= 32 * 1024 * 1024, "excessive exitrd size");
        capacity += metadata.len().div_ceil(65536) * 65536;
        members.push((name, file, metadata));
    }
    let machine = if cfg!(target_arch = "x86_64") {
        62
    } else {
        183
    };
    ensure!(
        names.len() == 1,
        "exitrd requires a single shutdown executable"
    );
    for (_, file, _) in &mut members {
        ensure!(
            exitrd_elf(file, machine)?.is_empty(),
            "exitrd shutdown must be static"
        );
    }
    let Some(destination) = destination else {
        return Ok(capacity);
    };
    for directory in directories {
        let parent = directory
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = openat2(
            &destination,
            parent,
            directory_flags,
            Mode::empty(),
            resolve,
        )?;
        mkdirat(
            &parent,
            directory.file_name().context("invalid exitrd directory")?,
            Mode::from_raw_mode(0o755),
        )?;
    }
    let create = |name: &str| -> anyhow::Result<fs::File> {
        Ok(fs::File::from(openat2(
            &destination,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
            resolve,
        )?))
    };
    for (name, file, metadata) in members {
        let mut target = create(name)?;
        ensure!(
            std::io::copy(&mut file.take(metadata.len() + 1), &mut target)? == metadata.len(),
            "exitrd member changed while copying"
        );
        target.set_permissions(metadata.permissions())?;
    }
    let mut release = create(RELEASE)?;
    release.write_all(b"ID=mica-exitrd\n")?;
    release.set_permissions(fs::Permissions::from_mode(0o644))?;
    Ok(capacity)
}

fn exitrd_elf(file: &mut fs::File, machine: u16) -> anyhow::Result<Vec<String>> {
    use anyhow::{Context, ensure};
    use std::io::SeekFrom;
    let mut header = [0_u8; 64];
    file.rewind()?;
    file.read_exact(&mut header)?;
    let word = |bytes: &[u8]| -> anyhow::Result<u64> { Ok(u64::from_le_bytes(bytes.try_into()?)) };
    ensure!(
        &header[..7] == b"\x7fELF\x02\x01\x01"
            && u16::from_le_bytes([header[18], header[19]]) == machine,
        "invalid exitrd ELF architecture"
    );
    ensure!(
        [2, 3].contains(&u16::from_le_bytes([header[16], header[17]]))
            && header[20..24] == [1, 0, 0, 0],
        "invalid exitrd ELF type"
    );
    let count = u16::from_le_bytes([header[56], header[57]]) as u64;
    ensure!(
        (1..=128).contains(&count) && u16::from_le_bytes([header[54], header[55]]) == 56,
        "invalid exitrd ELF program headers"
    );
    let mut loads = Vec::new();
    let mut dynamic = None;
    let mut needed = Vec::new();
    for n in 0..count {
        let offset = word(&header[32..40])?
            .checked_add(n * 56)
            .context("ELF offset overflow")?;
        file.seek(SeekFrom::Start(offset))?;
        let mut ph = [0; 56];
        file.read_exact(&mut ph)?;
        let kind = u32::from_le_bytes(ph[..4].try_into()?);
        let offset = word(&ph[8..16])?;
        let size = word(&ph[32..40])?;
        ensure!(
            offset
                .checked_add(size)
                .is_some_and(|end| end <= file.metadata().map_or(0, |m| m.len())),
            "exitrd ELF segment outside file"
        );
        match kind {
            1 => loads.push((word(&ph[16..24])?, offset, size)),
            2 => {
                ensure!(
                    dynamic.is_none() && size <= 65536,
                    "excessive exitrd ELF dynamic table"
                );
                dynamic = Some((offset, size));
            }
            3 => {
                ensure!(size > 1 && size <= 4096, "invalid exitrd interpreter");
                file.seek(SeekFrom::Start(offset))?;
                let mut value = vec![0; size as usize];
                file.read_exact(&mut value)?;
                ensure!(
                    value.pop() == Some(0) && !value.contains(&0),
                    "invalid exitrd interpreter"
                );
                let path = String::from_utf8(value)?;
                ensure!(path.starts_with('/'), "invalid exitrd interpreter");
                needed.push(path);
            }
            _ => {}
        }
    }
    ensure!(!loads.is_empty(), "exitrd ELF has no load segment");
    if let Some((offset, size)) = dynamic {
        ensure!(
            size >= 16 && size % 16 == 0,
            "invalid exitrd dynamic table length"
        );
        let mut strings = None;
        let mut length = None;
        let mut indices = Vec::new();
        let mut terminated = false;
        file.seek(SeekFrom::Start(offset))?;
        for _ in 0..size / 16 {
            let mut entry = [0; 16];
            file.read_exact(&mut entry)?;
            let value = word(&entry[8..])?;
            match word(&entry[..8])? {
                0 => {
                    terminated = true;
                    break;
                }
                1 => indices.push(value),
                5 => strings = Some(value),
                10 => length = Some(value),
                15 | 29 => anyhow::bail!("exitrd ELF search path is forbidden"),
                _ => {}
            }
        }
        ensure!(terminated, "unterminated exitrd dynamic table");
        if !indices.is_empty() {
            let strings = strings.context("missing exitrd ELF strings")?;
            let length = length.context("missing exitrd ELF string length")?;
            ensure!(
                length <= 65536 && indices.len() <= 128,
                "excessive exitrd ELF dependencies"
            );
            let (base, offset, size) = loads
                .iter()
                .find(|(base, _, size)| {
                    strings >= *base
                        && strings - *base <= *size
                        && length <= *size - (strings - *base)
                })
                .context("exitrd strings outside load segment")?;
            let _ = size;
            file.seek(SeekFrom::Start(offset + strings - base))?;
            let mut data = vec![0; length as usize];
            file.read_exact(&mut data)?;
            for index in indices {
                let tail = data
                    .get(usize::try_from(index)?..)
                    .context("invalid exitrd string index")?;
                let end = tail
                    .iter()
                    .position(|b| *b == 0)
                    .context("unterminated exitrd dependency")?;
                let name = std::str::from_utf8(&tail[..end])?;
                ensure!(
                    !name.is_empty()
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
                    "invalid exitrd dependency name"
                );
                needed.push(name.to_owned());
            }
        }
    }
    file.rewind()?;
    Ok(needed)
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
        .strip_prefix("mica-")
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

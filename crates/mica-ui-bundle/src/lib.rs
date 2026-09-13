//! Shared `.mica-ui.zip` validation, deterministic packing, and safe extraction.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

pub const MAX_COMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_EXPANDED_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 4096;
pub const MAX_DEPTH: usize = 32;
pub const MAX_PATH_BYTES: usize = 240;
pub const MAX_EXPANSION_RATIO: u64 = 100;
pub const MANIFEST_NAME: &str = "mica-ui.json";
pub const INDEX_NAME: &str = "index.html";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageManifest {
    pub schema_version: u32,
    pub name: String,
    pub version: String,
    pub immutable_dir: String,
    pub api_versions: Vec<String>,
}

impl PackageManifest {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != 1 {
            bail!(
                "unsupported manifest schemaVersion {}; expected 1",
                self.schema_version
            );
        }
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
        {
            bail!("manifest name must contain 1..128 bytes");
        }
        if self.version.trim().is_empty()
            || self.version.len() > 128
            || self.version.chars().any(char::is_control)
        {
            bail!("manifest version must contain 1..128 bytes");
        }
        let immutable = validate_name(&self.immutable_dir, false)?;
        if immutable.components().count() != 1 {
            bail!("manifest immutableDir must name one root directory");
        }
        if self.api_versions.is_empty()
            || self.api_versions.iter().any(|version| {
                version.is_empty()
                    || version.len() > 32
                    || !version
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
        {
            bail!("manifest apiVersions must contain short alphanumeric version names");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageInfo {
    pub manifest: PackageManifest,
    pub entries: usize,
    pub compressed_bytes: u64,
    pub expanded_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone)]
struct EntryPlan {
    index: usize,
    path: PathBuf,
    is_dir: bool,
    declared_size: u64,
}

struct Inspection {
    info: PackageInfo,
    plan: Vec<EntryPlan>,
}

pub fn inspect(path: &Path) -> anyhow::Result<PackageInfo> {
    validate_archive_file_size(path)?;
    let archive_bytes = fs::metadata(path)?.len();
    let digest = archive_sha256(path)?;
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut info = inspect_reader(file)?.info;
    info.compressed_bytes = archive_bytes;
    info.sha256 = digest;
    Ok(info)
}

pub fn extract(path: &Path, destination: &Path) -> anyhow::Result<PackageInfo> {
    validate_archive_file_size(path)?;
    let archive_bytes = fs::metadata(path)?.len();
    let digest = archive_sha256(path)?;
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut archive = ZipArchive::new(file).context("read ZIP directory")?;
    let mut inspection = inspect_archive(&mut archive)?;
    inspection.info.compressed_bytes = archive_bytes;
    inspection.info.sha256 = digest;
    fs::create_dir(destination)
        .with_context(|| format!("create private extraction root {}", destination.display()))?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod private extraction root {}", destination.display()))?;

    let result = (|| -> anyhow::Result<()> {
        let mut expanded = 0_u64;
        for planned in &inspection.plan {
            let output = destination.join(&planned.path);
            if planned.is_dir {
                create_dirs_exclusive(destination, &planned.path)?;
                continue;
            }
            if let Some(parent) = planned.path.parent() {
                create_dirs_exclusive(destination, parent)?;
            }
            let mut source = archive
                .by_index(planned.index)
                .with_context(|| format!("reopen ZIP entry {}", planned.path.display()))?;
            let mut target = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&output)
                .with_context(|| format!("create {}", output.display()))?;
            let copied = copy_bounded(&mut source, &mut target, MAX_FILE_BYTES)?;
            if copied != planned.declared_size {
                bail!(
                    "{} expanded to {copied} bytes, not its declared {} bytes",
                    planned.path.display(),
                    planned.declared_size
                );
            }
            expanded = expanded
                .checked_add(copied)
                .context("expanded byte count overflow")?;
            if expanded > MAX_EXPANDED_BYTES {
                bail!("archive expands beyond {MAX_EXPANDED_BYTES} bytes");
            }
            target
                .sync_all()
                .with_context(|| format!("fsync {}", output.display()))?;
        }
        Ok(())
    })();
    if let Err(err) = result {
        let _ = fs::remove_dir_all(destination);
        return Err(err);
    }
    File::open(destination)?.sync_all()?;
    Ok(inspection.info)
}

pub fn pack(source: &Path, output: &Path) -> anyhow::Result<PackageInfo> {
    pack_impl(source, output, None)
}

pub fn pack_with_manifest(
    source: &Path,
    output: &Path,
    manifest: &PackageManifest,
) -> anyhow::Result<PackageInfo> {
    manifest.validate()?;
    pack_impl(source, output, Some(manifest))
}

fn pack_impl(
    source: &Path,
    output: &Path,
    manifest_override: Option<&PackageManifest>,
) -> anyhow::Result<PackageInfo> {
    let source = source
        .canonicalize()
        .with_context(|| format!("resolve {}", source.display()))?;
    if !source.is_dir() {
        bail!("{} is not a directory", source.display());
    }
    if manifest_override.is_none() {
        read_manifest(&source)?;
    } else if source.join(MANIFEST_NAME).exists() {
        bail!("source already contains {MANIFEST_NAME}; remove it when passing manifest options");
    }
    if !source.join(INDEX_NAME).is_file() {
        bail!("package root has no regular {INDEX_NAME}");
    }
    let mut entries = Vec::new();
    collect_source(&source, Path::new(""), &mut entries)?;
    if manifest_override.is_some() {
        entries.push((PathBuf::from(MANIFEST_NAME), false));
        entries.sort_by(|left, right| left.0.cmp(&right.0));
    }
    if entries.len() > MAX_ENTRIES {
        bail!("package contains more than {MAX_ENTRIES} entries");
    }

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)
        .with_context(|| format!("create {}", output.display()))?;
    let result = (|| -> anyhow::Result<PackageInfo> {
        let mut writer = ZipWriter::new(file);
        let base = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(DateTime::default());
        for (rel, is_dir) in &entries {
            let name = slash_name(rel, *is_dir)?;
            if *is_dir {
                writer.add_directory(name, base.unix_permissions(0o755))?;
            } else {
                writer.start_file(name, base.unix_permissions(0o644))?;
                if manifest_override.is_some() && rel == Path::new(MANIFEST_NAME) {
                    let manifest = manifest_override.context("missing generated manifest")?;
                    writer.write_all(&serde_json::to_vec(manifest)?)?;
                } else {
                    let mut input = File::open(source.join(rel))?;
                    std::io::copy(&mut input, &mut writer)?;
                }
            }
        }
        let file = writer.finish().context("finish ZIP")?;
        file.sync_all().context("fsync ZIP")?;
        inspect(output)
    })();
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}

fn validate_archive_file_size(path: &Path) -> anyhow::Result<()> {
    let size = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if size > MAX_COMPRESSED_BYTES {
        bail!("archive file is larger than {MAX_COMPRESSED_BYTES} bytes");
    }
    Ok(())
}

fn archive_sha256(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest).context("hash archive")?;
    Ok(hex::encode(digest.finalize()))
}

fn inspect_reader<R: Read + Seek>(reader: R) -> anyhow::Result<Inspection> {
    let mut archive = ZipArchive::new(reader).context("read ZIP directory")?;
    inspect_archive(&mut archive)
}

fn inspect_archive<R: Read + Seek>(archive: &mut ZipArchive<R>) -> anyhow::Result<Inspection> {
    if archive.len() > MAX_ENTRIES {
        bail!("archive contains more than {MAX_ENTRIES} entries");
    }
    let mut seen = BTreeSet::new();
    let mut plan = Vec::with_capacity(archive.len());
    let mut compressed = 0_u64;
    let mut expanded = 0_u64;
    for index in 0..archive.len() {
        let file = archive
            .by_index(index)
            .with_context(|| format!("read ZIP entry {index}"))?;
        if file.encrypted() {
            bail!("encrypted ZIP entries are not supported");
        }
        let raw = std::str::from_utf8(file.name_raw()).context("ZIP entry name is not UTF-8")?;
        let path = validate_name(raw, file.is_dir())?;
        if !seen.insert(path.clone()) {
            bail!("duplicate ZIP entry {}", path.display());
        }
        validate_kind(&file, &path)?;
        if !file.is_dir() && file.size() > MAX_FILE_BYTES {
            bail!("{} is larger than {MAX_FILE_BYTES} bytes", path.display());
        }
        compressed = compressed
            .checked_add(file.compressed_size())
            .context("compressed byte count overflow")?;
        expanded = expanded
            .checked_add(file.size())
            .context("expanded byte count overflow")?;
        if compressed > MAX_COMPRESSED_BYTES || expanded > MAX_EXPANDED_BYTES {
            bail!("archive exceeds its compressed or expanded byte limit");
        }
        plan.push(EntryPlan {
            index,
            path,
            is_dir: file.is_dir(),
            declared_size: file.size(),
        });
    }
    if expanded > 0
        && (compressed == 0 || expanded > compressed.saturating_mul(MAX_EXPANSION_RATIO))
    {
        bail!("archive expands by more than {MAX_EXPANSION_RATIO}:1");
    }
    if !seen.contains(Path::new(INDEX_NAME)) || !seen.contains(Path::new(MANIFEST_NAME)) {
        bail!("package root must contain {INDEX_NAME} and {MANIFEST_NAME}");
    }

    let manifest = {
        let mut file = archive
            .by_name(MANIFEST_NAME)
            .context("open package manifest")?;
        let declared_size = file.size();
        let mut bytes = Vec::with_capacity(usize::try_from(declared_size).unwrap_or(0));
        file.by_ref()
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != declared_size || bytes.len() as u64 > MAX_FILE_BYTES {
            bail!("package manifest expanded beyond its declared or permitted size");
        }
        serde_json::from_slice::<PackageManifest>(&bytes).context("parse package manifest")?
    };
    manifest.validate()?;
    Ok(Inspection {
        info: PackageInfo {
            manifest,
            entries: plan.len(),
            compressed_bytes: compressed,
            expanded_bytes: expanded,
            sha256: String::new(),
        },
        plan,
    })
}

fn validate_name(raw: &str, directory: bool) -> anyhow::Result<PathBuf> {
    if raw.is_empty()
        || raw.len() > MAX_PATH_BYTES
        || raw.contains('\\')
        || raw.contains('%')
        || raw.starts_with('/')
        || raw.chars().any(char::is_control)
    {
        bail!("unsafe ZIP entry name {raw:?}");
    }
    let name = if directory {
        raw.strip_suffix('/').unwrap_or(raw)
    } else {
        raw
    };
    if name.is_empty()
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("unsafe ZIP entry name {raw:?}");
    }
    let path = PathBuf::from(name);
    if path.components().count() > MAX_DEPTH
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("unsafe ZIP entry name {raw:?}");
    }
    Ok(path)
}

fn validate_kind<R: Read>(file: &zip::read::ZipFile<'_, R>, path: &Path) -> anyhow::Result<()> {
    if file.is_symlink() {
        bail!("{} is a symbolic link", path.display());
    }
    if let Some(mode) = file.unix_mode() {
        let kind = mode & 0o170000;
        let expected = if file.is_dir() { 0o040000 } else { 0o100000 };
        if kind != 0 && kind != expected {
            bail!("{} is not a regular file or directory", path.display());
        }
    }
    Ok(())
}

fn copy_bounded(input: &mut impl Read, output: &mut impl Write, max: u64) -> anyhow::Result<u64> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > max {
            bail!("file expands beyond {max} bytes");
        }
        output.write_all(&buffer[..count])?;
    }
    Ok(total)
}

fn create_dirs_exclusive(root: &Path, rel: &Path) -> anyhow::Result<()> {
    let mut current = root.to_path_buf();
    for part in rel.components() {
        let Component::Normal(part) = part else {
            bail!("unsafe extraction path");
        };
        current.push(part);
        match fs::create_dir(&current) {
            Ok(()) => fs::set_permissions(&current, fs::Permissions::from_mode(0o700))?,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && current.is_dir() => {}
            Err(err) => return Err(err).with_context(|| format!("create {}", current.display())),
        }
    }
    Ok(())
}

fn read_manifest(root: &Path) -> anyhow::Result<PackageManifest> {
    let bytes = fs::read(root.join(MANIFEST_NAME)).context("read package manifest")?;
    let manifest =
        serde_json::from_slice::<PackageManifest>(&bytes).context("parse package manifest")?;
    manifest.validate()?;
    Ok(manifest)
}

fn collect_source(root: &Path, rel: &Path, out: &mut Vec<(PathBuf, bool)>) -> anyhow::Result<()> {
    let mut children = fs::read_dir(root.join(rel))?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(fs::DirEntry::file_name);
    for child in children {
        let child_rel = rel.join(child.file_name());
        validate_name(
            child_rel.to_str().context("source path is not UTF-8")?,
            false,
        )?;
        let meta = fs::symlink_metadata(child.path())?;
        if meta.file_type().is_symlink() || (!meta.is_dir() && !meta.is_file()) {
            bail!(
                "{} is not a regular file or directory",
                child.path().display()
            );
        }
        if meta.is_file() && meta.len() > MAX_FILE_BYTES {
            bail!(
                "{} is larger than {MAX_FILE_BYTES} bytes",
                child.path().display()
            );
        }
        out.push((child_rel.clone(), meta.is_dir()));
        if meta.is_dir() {
            collect_source(root, &child_rel, out)?;
        }
    }
    Ok(())
}

fn slash_name(path: &Path, directory: bool) -> anyhow::Result<String> {
    let mut name = path
        .to_str()
        .context("source path is not UTF-8")?
        .replace(std::path::MAIN_SEPARATOR, "/");
    if directory {
        name.push('/');
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) {
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join(INDEX_NAME), "<!doctype html>").unwrap();
        fs::write(root.join("assets/app.js"), "console.log(1)").unwrap();
        fs::write(root.join(MANIFEST_NAME), r#"{"schemaVersion":1,"name":"demo","version":"1.0.0","immutableDir":"assets","apiVersions":["v1"]}"#).unwrap();
    }

    #[test]
    fn pack_is_deterministic_and_extracts() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fixture(&source);
        let first = temp.path().join("one.mica-ui.zip");
        let second = temp.path().join("two.mica-ui.zip");
        let info = pack(&source, &first).unwrap();
        pack(&source, &second).unwrap();
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
        assert_eq!(info.manifest.schema_version, 1);
        assert_eq!(info.sha256.len(), 64);
        let destination = temp.path().join("expanded");
        extract(&first, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("assets/app.js")).unwrap(),
            "console.log(1)"
        );
    }

    #[test]
    fn pack_can_generate_the_manifest_from_cli_fields() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("assets")).unwrap();
        fs::write(source.join(INDEX_NAME), "<!doctype html>").unwrap();
        fs::write(source.join("assets/app.js"), "console.log(1)").unwrap();
        let output = temp.path().join("generated.mica-ui.zip");
        let manifest = PackageManifest {
            schema_version: 1,
            name: "demo".to_string(),
            version: "2.0.0".to_string(),
            immutable_dir: "assets".to_string(),
            api_versions: vec!["v1".to_string()],
        };
        let info = pack_with_manifest(&source, &output, &manifest).unwrap();
        assert_eq!(info.manifest, manifest);
        assert_eq!(info.sha256.len(), 64);
    }

    #[test]
    fn traversal_is_rejected_without_creating_a_destination() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("bad.zip");
        let mut zip = ZipWriter::new(File::create(&archive).unwrap());
        zip.start_file("../index.html", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"x").unwrap();
        zip.finish().unwrap();
        let destination = temp.path().join("expanded");
        assert!(extract(&archive, &destination).is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn residual_escape_names_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("escaped.zip");
        let mut zip = ZipWriter::new(File::create(&archive).unwrap());
        zip.start_file("assets/%2f.js", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"x").unwrap();
        zip.finish().unwrap();
        assert!(inspect(&archive).is_err());
    }

    #[test]
    fn whole_archive_file_limit_is_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("oversized.zip");
        let file = File::create(&archive).unwrap();
        file.set_len(MAX_COMPRESSED_BYTES + 1).unwrap();
        assert!(inspect(&archive).is_err());
    }
}

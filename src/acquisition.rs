//! Bounded online acquisition and streaming offline component archives.
use crate::{
    catalog::{self, CatalogCheckpoint, CatalogRequest, SelectedRelease, VerifiedCatalog},
    components::{Artifact, Deployment, authenticate_deployment, component_id},
    deployments::{
        DeploymentStore, atomic_write, directory, read_bounded, sync_directory, verify_file,
    },
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub struct Acquisition<'a> {
    pub root: PathBuf,
    pub store: &'a DeploymentStore,
    pub keys: &'a [[u8; 32]],
    pub board: &'a str,
    pub arch: &'a str,
    pub max_bytes: u64,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyDeployment {
    pub id: String,
    pub path: PathBuf,
    pub objects: PathBuf,
    pub version: String,
    pub generation: u64,
}

impl Acquisition<'_> {
    pub fn objects(&self) -> PathBuf {
        self.root.join("verified/objects")
    }

    fn prepare(&self) -> Result<()> {
        ensure!(
            self.max_bytes > 0 && self.max_bytes <= 8 * 1024 * 1024 * 1024,
            "invalid workspace budget"
        );
        for path in [
            self.root.join("downloads"),
            self.objects(),
            self.root.join("staging"),
        ] {
            directory(&path)?;
        }
        Ok(())
    }

    fn files(&self) -> Result<Vec<(PathBuf, u64)>> {
        let mut stack = vec![(self.root.clone(), 0)];
        let mut files = Vec::new();
        let mut entries = 0;
        while let Some((path, depth)) = stack.pop() {
            ensure!(depth <= 3, "workspace directory depth exceeded");
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                entries += 1;
                ensure!(entries <= 4096, "workspace entry bound exceeded");
                let meta = entry.path().symlink_metadata()?;
                if meta.is_dir() {
                    stack.push((entry.path(), depth + 1));
                } else {
                    ensure!(meta.is_file(), "unexpected workspace file type");
                    files.push((entry.path(), meta.len()));
                }
            }
        }
        Ok(files)
    }

    fn reserve(&self, bytes: u64) -> Result<()> {
        let used = self
            .files()?
            .into_iter()
            .try_fold(0_u64, |sum, (_, bytes)| {
                sum.checked_add(bytes).context("workspace size overflow")
            })?;
        ensure!(
            used.checked_add(bytes)
                .is_some_and(|total| total <= self.max_bytes),
            "workspace budget exhausted"
        );
        let stat = rustix::fs::statvfs(&self.root)?;
        ensure!(
            stat.f_bavail.saturating_mul(stat.f_frsize) >= bytes.saturating_add(128 * 1024 * 1024)
                && (stat.f_files == 0 || stat.f_favail >= 2048 + 32),
            "DATA reserve unavailable"
        );
        Ok(())
    }

    /// Remove only the disposable acquisition workspace under the storage lock.
    pub fn discard(&self) -> Result<usize> {
        self.prepare()?;
        let files = self.files()?;
        for (path, _) in &files {
            fs::remove_file(path)?;
            sync_directory(path.parent().context("missing workspace parent")?)?;
        }
        Ok(files.len())
    }

    pub fn probe(&self) -> Result<serde_json::Value> {
        self.prepare()?;
        self.reserve(4096)?;
        let probe = self.root.join("staging/probe");
        atomic_write(&probe, b"workspace probe\n")?;
        fs::remove_file(&probe)?;
        sync_directory(probe.parent().context("missing probe parent")?)?;
        let stat = rustix::fs::statvfs(&self.root)?;
        Ok(serde_json::json!({
            "status":"ready", "root":self.root, "maxBytes":self.max_bytes,
            "freeBytes":stat.f_bavail.saturating_mul(stat.f_frsize),
            "freeInodes":stat.f_favail,
        }))
    }

    fn missing(&self, deployment: &Deployment) -> Result<BTreeMap<String, u64>> {
        catalog::artifacts(deployment)?;
        let mut missing = BTreeMap::new();
        for (path, artifact) in self.store.object_paths(deployment) {
            if path.symlink_metadata().is_ok() {
                verify_file(&path, artifact)?;
            } else {
                missing.insert(artifact.sha256.clone(), artifact.bytes);
            }
        }
        for (sha, bytes) in missing.clone() {
            let path = self.objects().join(&sha);
            if path.symlink_metadata().is_ok() {
                verify_file(
                    &path,
                    &Artifact {
                        sha256: sha.clone(),
                        bytes,
                    },
                )?;
                missing.remove(&sha);
            }
        }
        Ok(missing)
    }

    fn validate(&self, deployment: &Deployment) -> Result<()> {
        ensure!(
            deployment.board == self.board && deployment.arch == self.arch,
            "deployment targets another device"
        );
        let state = self.store.effective_state()?;
        ensure!(state.candidate.is_none(), "another deployment is pending");
        ensure!(
            deployment.generation > state.highest_generation,
            "deployment generation is not newer"
        );
        Ok(())
    }

    fn ready(&self, envelope: &[u8], deployment: Deployment) -> Result<ReadyDeployment> {
        ensure!(
            self.missing(&deployment)?.is_empty(),
            "deployment objects are incomplete"
        );
        let id = component_id(&serde_json::to_value(&deployment)?)?;
        let path = self.root.join(format!("verified/{id}.json"));
        atomic_write(&path, envelope)?;
        Ok(ReadyDeployment {
            id,
            path,
            objects: self.objects(),
            version: deployment.version,
            generation: deployment.generation,
        })
    }

    pub fn check(&self, source: &str, channel: &str, now: i64) -> Result<VerifiedCatalog> {
        catalog::source_url(source)?;
        self.prepare()?;
        self.reserve(catalog::MAX_CATALOG_ENVELOPE)?;
        let path = self.root.join("staging/catalog.partial");
        download(source, &path, catalog::MAX_CATALOG_ENVELOPE, false)?;
        let checkpoint_path = self.store.meta.join("catalog.json");
        let previous: Option<CatalogCheckpoint> = if checkpoint_path.symlink_metadata().is_ok() {
            Some(serde_json::from_slice(&read_bounded(
                &checkpoint_path,
                4096,
            )?)?)
        } else {
            None
        };
        let result = catalog::verify_catalog(
            &read_bounded(&path, catalog::MAX_CATALOG_ENVELOPE)?,
            self.keys,
            &CatalogRequest {
                source,
                board: self.board,
                arch: self.arch,
                channel,
                now,
                checkpoint: previous.as_ref(),
                highest_generation: self.store.effective_state()?.highest_generation,
            },
        )?;
        atomic_write(&checkpoint_path, &serde_json::to_vec(&result.checkpoint)?)?;
        fs::remove_file(&path)?;
        sync_directory(path.parent().context("missing catalog parent")?)?;
        Ok(result)
    }

    fn reserve_missing(&self, missing: &BTreeMap<String, u64>) -> Result<()> {
        let mut needed = 0_u64;
        for (sha, bytes) in missing {
            let partial = self.root.join(format!("downloads/{sha}.partial"));
            let held = if partial.symlink_metadata().is_ok() {
                let meta = partial.symlink_metadata()?;
                ensure!(
                    meta.is_file() && meta.len() <= *bytes,
                    "invalid partial object"
                );
                meta.len()
            } else {
                0
            };
            needed = needed
                .checked_add(bytes - held)
                .context("object size overflow")?;
        }
        self.reserve(needed + 24576)
    }

    pub fn fetch(&self, selected: SelectedRelease) -> Result<ReadyDeployment> {
        self.validate(&selected.deployment)?;
        self.prepare()?;
        let missing = self.missing(&selected.deployment)?;
        self.reserve_missing(&missing)?;
        for (sha, bytes) in missing {
            let object = selected
                .objects
                .iter()
                .find(|object| object.sha256 == sha && object.bytes == bytes)
                .context("missing acquisition URL")?;
            let partial = self.root.join(format!("downloads/{sha}.partial"));
            if !partial.try_exists()? || partial.metadata()?.len() < bytes {
                download(&object.url, &partial, bytes, true)?;
            }
            self.promote(&partial, &Artifact { sha256: sha, bytes })?;
        }
        self.ready(selected.envelope.as_bytes(), selected.deployment)
    }

    fn promote(&self, partial: &Path, artifact: &Artifact) -> Result<()> {
        if let Err(error) = verify_file(partial, artifact) {
            fs::remove_file(partial)?;
            sync_directory(partial.parent().context("missing object parent")?)?;
            return Err(error);
        }
        File::open(partial)?.sync_all()?;
        fs::rename(partial, self.objects().join(&artifact.sha256))?;
        sync_directory(&self.objects())?;
        sync_directory(partial.parent().context("missing object parent")?)
    }

    /// MOSUPD01: descriptor length (u32 BE), signed descriptor, object count
    /// (u32 BE), then digest (64 ASCII), size (u64 BE), and exact object bytes.
    /// There are no filenames, directory entries, links, compression or padding.
    pub fn import(&self, input: &mut impl Read) -> Result<ReadyDeployment> {
        let mut magic = [0; 8];
        input.read_exact(&mut magic)?;
        ensure!(&magic == b"MOSUPD01", "invalid component archive");
        let mut length = [0; 4];
        input.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        ensure!(
            length > 0 && length <= 24576,
            "archive descriptor exceeds bound"
        );
        let mut envelope = vec![0; length];
        input.read_exact(&mut envelope)?;
        let deployment = authenticate_deployment(&envelope, self.keys)?;
        self.validate(&deployment)?;
        let mut required = catalog::artifacts(&deployment)?;
        let mut count = [0; 4];
        input.read_exact(&mut count)?;
        ensure!(
            u32::from_be_bytes(count) as usize == required.len(),
            "archive object count mismatch"
        );
        self.prepare()?;
        let missing = self.missing(&deployment)?;
        self.reserve_missing(&missing)?;
        for _ in 0..required.len() {
            let mut sha = [0; 64];
            input.read_exact(&mut sha)?;
            let sha = String::from_utf8(sha.to_vec())?;
            let mut size = [0; 8];
            input.read_exact(&mut size)?;
            let size = u64::from_be_bytes(size);
            ensure!(
                required.remove(&sha) == Some(size),
                "unlisted, duplicate or wrong-sized archive object"
            );
            let partial = self.root.join(format!("downloads/{sha}.partial"));
            let mut output = if missing.contains_key(&sha) {
                if partial.symlink_metadata().is_ok() {
                    ensure!(
                        partial.symlink_metadata()?.is_file(),
                        "invalid partial object"
                    );
                    fs::remove_file(&partial)?;
                }
                Some(
                    OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&partial)?,
                )
            } else {
                None
            };
            let mut remaining = size;
            let mut buffer = [0; 65536];
            let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
            while remaining > 0 {
                let count = remaining.min(buffer.len() as u64) as usize;
                input.read_exact(&mut buffer[..count])?;
                hash.update(&buffer[..count]);
                if let Some(output) = &mut output {
                    output.write_all(&buffer[..count])?;
                }
                remaining -= count as u64;
            }
            ensure!(
                hex::encode(hash.finish()) == sha,
                "archive object digest mismatch"
            );
            if let Some(output) = output {
                output.sync_all()?;
                drop(output);
                self.promote(
                    &partial,
                    &Artifact {
                        sha256: sha,
                        bytes: size,
                    },
                )?;
            }
        }
        ensure!(input.read(&mut [0; 1])? == 0, "trailing archive bytes");
        self.ready(&envelope, deployment)
    }
}

fn download(url: &str, path: &Path, limit: u64, resume: bool) -> Result<()> {
    if path.symlink_metadata().is_ok() {
        ensure!(
            path.symlink_metadata()?.is_file(),
            "invalid download target"
        );
    }
    let mut command = Command::new("/usr/bin/curl");
    command
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .args([
            "--disable",
            "--silent",
            "--show-error",
            "--fail",
            "--proto",
            "=http,https",
            "--connect-timeout",
            "15",
            "--max-time",
            "1800",
            "--speed-limit",
            "1024",
            "--speed-time",
            "30",
            "--max-filesize",
        ])
        .arg(limit.to_string())
        .args(["--output"])
        .arg(path)
        .args(["--write-out", "%{http_code}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if resume {
        command.args(["--continue-at", "-"]);
    }
    let output = command.args(["--url", url]).output()?;
    ensure!(
        output.status.success(),
        "component transfer interrupted: {}",
        output.status
    );
    ensure!(
        output.stdout == b"200" || (resume && output.stdout == b"206"),
        "unexpected transfer status"
    );
    ensure!(
        path.metadata()?.len() <= limit,
        "transfer exceeded byte bound"
    );
    Ok(())
}

//! Durable file-deployment state and native UEFI/FIT boot records.
use crate::boot::selected_entry;
use crate::fit_env::{Environment, FitLayout, Record};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// Shared durable metadata cannot be read or committed. Selecting another
/// deployment cannot repair DATA, so boot health must enter recovery.
#[derive(Debug)]
pub struct SharedDataFailure;
impl std::fmt::Display for SharedDataFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("shared DATA metadata unavailable; recovery required")
    }
}
impl std::error::Error for SharedDataFailure {}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BootReceipt {
    pub backend: crate::boot::BootKind,
    pub deployment_id: String,
    pub entry: String,
    pub kernel_id: String,
    pub rootfs_id: String,
    pub content_verified: bool,
    pub secure_boot: bool,
    pub boot_verified: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct State {
    pub highest_generation: u64,
    pub current: Option<String>,
    pub fallback: Option<String>,
    pub candidate: Option<String>,
    pub failed: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub file: String,
    pub generation: u64,
    pub tries_left: Option<u8>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStatus {
    #[serde(flatten)]
    pub entry: Entry,
    pub version: String,
    pub kernel_id: String,
    pub kernel_release: String,
    pub rootfs_id: String,
}

pub enum BootBackend {
    Uefi {
        esp: PathBuf,
    },
    Fit {
        firmware: PathBuf,
        layout: FitLayout,
    },
}

pub struct DeploymentStore {
    pub system: PathBuf,
    pub boot: BootBackend,
    pub meta: PathBuf,
}

/// Resolve partition 1 on the disk containing the authenticated SYSTEM UUID.
pub fn boot_partition(system: &Path, kind: crate::boot::BootKind, board: &str) -> Result<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    ensure!(
        fs::read_to_string(system.join("partition"))?.trim() == "2",
        "invalid SYSTEM partition"
    );
    let parent = system.parent().context("SYSTEM has no physical disk")?;
    let mut partitions = Vec::new();
    for child in fs::read_dir(parent)? {
        let child = child?.path();
        if fs::read_to_string(child.join("partition")).is_ok_and(|number| number.trim() == "1") {
            partitions.push(child);
        }
    }
    ensure!(
        partitions.len() == 1,
        "boot partition is absent or ambiguous"
    );
    let physical = &partitions[0];
    if kind == crate::boot::BootKind::UbootFit {
        ensure!(
            fs::read_to_string(physical.join("start"))?.trim() == "64"
                && fs::read_to_string(physical.join("size"))?.trim()
                    == FitLayout::for_board(board)?.sectors().to_string(),
            "FIRMWARE geometry differs from the compiled layout"
        );
    }
    let device = Path::new("/dev").join(physical.file_name().context("missing boot device")?);
    ensure!(
        device.symlink_metadata()?.file_type().is_block_device(),
        "boot partition is not a block device"
    );
    Ok(device)
}

pub fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    ensure!(
        meta.is_file() && meta.len() <= limit,
        "invalid file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "file exceeded read limit");
    Ok(bytes)
}

pub fn valid_id(id: &str) -> Result<()> {
    ensure!(
        selected_entry(&format!("mica-{id}.conf"))? == id,
        "invalid ID"
    );
    Ok(())
}

pub fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    if path.symlink_metadata().is_ok() {
        ensure!(
            path.symlink_metadata()?.is_file(),
            "destination is not a regular file"
        );
    }
    let pending = path.with_extension("pending");
    if pending.symlink_metadata().is_ok() {
        ensure!(
            pending.symlink_metadata()?.is_file(),
            "pending file is not regular"
        );
        fs::remove_file(&pending)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&pending, path)?;
    sync_directory(parent)
}

struct Collection {
    files: Vec<PathBuf>,
    directories: Vec<PathBuf>,
}

impl Collection {
    fn reclaimed_bytes(&self, destination: &Path) -> Result<u64> {
        let device = destination.metadata()?.dev();
        self.files.iter().try_fold(0_u64, |bytes, file| {
            let metadata = file.symlink_metadata()?;
            // Hard-linked objects may still occupy blocks after unlinking.
            Ok(bytes
                + if metadata.dev() == device && metadata.nlink() == 1 {
                    metadata.blocks() * 512
                } else {
                    0
                })
        })
    }

    fn apply(self) -> Result<usize> {
        let count = self.files.len();
        for file in self.files {
            fs::remove_file(&file)?;
            sync_directory(file.parent().context("missing collection parent")?)?;
        }
        for directory in self.directories {
            fs::remove_dir(&directory)?;
            sync_directory(directory.parent().context("missing collection parent")?)?;
        }
        Ok(count)
    }
}

impl DeploymentStore {
    pub fn new(system: PathBuf, boot: BootBackend, meta: PathBuf) -> Self {
        Self { system, boot, meta }
    }

    /// Hold this across installation, confirmation, reset or garbage collection.
    pub fn lock(&self) -> Result<File> {
        let file = (|| -> Result<File> {
            ensure!(
                self.meta.symlink_metadata()?.is_dir(),
                "metadata namespace is not a directory"
            );
            let path = self.meta.join("transaction.lock");
            if path.symlink_metadata().is_ok() {
                ensure!(path.symlink_metadata()?.is_file(), "invalid lock file");
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            Ok(file)
        })()
        .context(SharedDataFailure)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .context("another storage transaction is active")?;
        Ok(file)
    }

    pub fn state(&self) -> Result<State> {
        (|| -> Result<State> {
            let path = self.meta.join("deployments.json");
            if !path.try_exists()? && path.symlink_metadata().is_err() {
                return Ok(State::default());
            }
            let state: State = serde_json::from_slice(&read_bounded(&path, 24576)?)?;
            ensure!(
                state.failed.len() <= 128,
                "failed deployment set exceeds limit"
            );
            for id in [&state.current, &state.fallback, &state.candidate]
                .into_iter()
                .flatten()
                .chain(state.failed.iter())
            {
                valid_id(id)?;
            }
            Ok(state)
        })()
        .context(SharedDataFailure)
    }

    pub fn save_state(&self, state: &State) -> Result<()> {
        atomic_write(
            &self.meta.join("deployments.json"),
            &serde_json::to_vec(state)?,
        )
        .context(SharedDataFailure)
    }

    /// Native boot entries commit activation before DATA's diagnostic state.
    /// Reconcile that crash window without refilling any boot attempts.
    pub fn effective_state(&self) -> Result<State> {
        let mut state = self.state()?;
        let entries = self.entries()?;
        state.highest_generation = state.highest_generation.max(
            entries
                .iter()
                .map(|entry| entry.generation)
                .max()
                .unwrap_or(0),
        );
        if let Some(confirmed) = entries
            .iter()
            .find(|entry| entry.tries_left.is_none() && !state.failed.contains(&entry.id))
            && state.current.as_deref() != Some(confirmed.id.as_str())
        {
            let usable = |id: &String| {
                entries.iter().any(|entry| {
                    &entry.id == id
                        && entry.id != confirmed.id
                        && entry.tries_left != Some(0)
                        && !state.failed.contains(id)
                })
            };
            state.fallback = state
                .current
                .clone()
                .filter(usable)
                .or_else(|| state.fallback.clone().filter(usable));
            state.current = Some(confirmed.id.clone());
        }
        // Retirement commits in the native records before DATA. An absent
        // fallback must never protect objects after that commit.
        state.fallback = state
            .fallback
            .filter(|id| entries.iter().any(|e| &e.id == id));
        if let Some(current) = &state.current {
            let generation = entries
                .iter()
                .find(|entry| &entry.id == current)
                .context("confirmed deployment entry is missing")?
                .generation;
            let candidates: Vec<_> = entries
                .iter()
                .filter(|entry| {
                    entry.generation > generation
                        && entry.tries_left.is_some_and(|tries| tries > 0)
                        && !state.failed.contains(&entry.id)
                        && state.fallback.as_deref() != Some(entry.id.as_str())
                })
                .collect();
            ensure!(candidates.len() <= 1, "ambiguous pending deployments");
            state.candidate = candidates.first().map(|entry| entry.id.clone());
        }
        Ok(state)
    }

    pub fn entries(&self) -> Result<Vec<Entry>> {
        let esp = match &self.boot {
            BootBackend::Uefi { esp } => esp,
            BootBackend::Fit { firmware, layout } => {
                return Ok(Environment::load(firmware, *layout)?
                    .records
                    .into_iter()
                    .map(|r| Entry {
                        file: format!("fit:{}", r.id),
                        id: r.id,
                        generation: r.generation,
                        tries_left: r.tries_left,
                    })
                    .collect());
            }
        };
        let directory = esp.join("loader/entries");
        ensure!(
            directory.symlink_metadata()?.is_dir(),
            "invalid entries directory"
        );
        let mut entries: Vec<Entry> = Vec::new();
        for item in fs::read_dir(directory)? {
            let item = item?;
            let file = item
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("invalid entry name"))?;
            if !file.starts_with("mica-") || !file.ends_with(".conf") {
                continue;
            }
            let id = selected_entry(&file)?;
            ensure!(
                !entries.iter().any(|entry| entry.id == id),
                "ambiguous boot entries for {id}"
            );
            let content = String::from_utf8(read_bounded(&item.path(), 4096)?)?;
            let versions = content
                .lines()
                .filter_map(|line| line.strip_prefix("version "))
                .collect::<Vec<_>>();
            ensure!(versions.len() == 1, "entry has no unique version");
            let generation: u64 = versions[0].parse()?;
            ensure!(
                generation > 0 && generation.to_string() == versions[0],
                "invalid entry generation"
            );
            let tries_left = file
                .split_once('+')
                .map(|(_, count)| count.as_bytes()[0] - b'0');
            entries.push(Entry {
                id,
                file,
                generation,
                tries_left,
            });
            ensure!(entries.len() <= 2, "too many deployment entries");
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.generation));
        Ok(entries)
    }

    fn validate_receipt(&self, receipt: &BootReceipt) -> Result<()> {
        ensure!(
            receipt.content_verified,
            "running content is not authenticated"
        );
        valid_id(&receipt.deployment_id)?;
        match &self.boot {
            BootBackend::Uefi { .. } => {
                ensure!(
                    receipt.backend == crate::boot::BootKind::Uefi
                        && receipt.boot_verified == receipt.secure_boot,
                    "boot receipt backend mismatch"
                );
                ensure!(
                    selected_entry(&receipt.entry)? == receipt.deployment_id,
                    "boot receipt identity mismatch"
                );
            }
            BootBackend::Fit { firmware, layout } => {
                ensure!(
                    receipt.backend == crate::boot::BootKind::UbootFit
                        && !receipt.secure_boot
                        && receipt.boot_verified,
                    "boot receipt backend mismatch"
                );
                ensure!(
                    receipt.entry == format!("fit:{}", receipt.deployment_id),
                    "boot receipt identity mismatch"
                );
                let env = Environment::load(firmware, *layout)?;
                ensure!(
                    env.records
                        .iter()
                        .any(|r| r.id == receipt.deployment_id && r.kernel_id == receipt.kernel_id),
                    "boot receipt kernel mismatch"
                );
            }
        }
        Ok(())
    }

    fn set_tries(&self, entry: &Entry, tries_left: Option<u8>) -> Result<()> {
        match &self.boot {
            BootBackend::Uefi { esp } => {
                let suffix = match tries_left {
                    None => ".conf",
                    Some(0) => "+0-3.conf",
                    _ => anyhow::bail!("attempts cannot be refilled"),
                };
                let directory = esp.join("loader/entries");
                fs::rename(
                    directory.join(&entry.file),
                    directory.join(format!("mica-{}{suffix}", entry.id)),
                )?;
                sync_directory(&directory)
            }
            BootBackend::Fit { firmware, layout } => {
                ensure!(
                    tries_left.is_none() || tries_left == Some(0),
                    "attempts cannot be refilled"
                );
                let mut env = Environment::load(firmware, *layout)?;
                let record = env
                    .records
                    .iter_mut()
                    .find(|r| r.id == entry.id && r.generation == entry.generation)
                    .context("boot record is missing")?;
                record.tries_left = tries_left;
                env.save(firmware)
            }
        }
    }

    /// Early PID 1, or a caller holding the DATA transaction lock, may retire a
    /// failed confirmed boot. Native trial counters remain firmware-owned.
    pub fn retire_failed_confirmed(&self, id: &str) -> Result<bool> {
        valid_id(id)?;
        let entries = self.entries()?;
        let entry = entries
            .iter()
            .find(|entry| entry.id == id)
            .context("selected boot record is missing")?;
        if entry.tries_left.is_some() {
            return Ok(false);
        }
        ensure!(
            entries
                .iter()
                .any(|entry| entry.id != id && entry.tries_left != Some(0)),
            "no usable fallback remains; recovery required"
        );
        self.set_tries(entry, Some(0))?;
        Ok(true)
    }

    pub fn fail_boot(&self, receipt: &BootReceipt) -> Result<bool> {
        self.validate_receipt(receipt)?;
        self.retire_failed_confirmed(&receipt.deployment_id)
    }

    fn retain_entries(&self, entries: &[Entry], keep: &[Option<&str>]) -> Result<()> {
        match &self.boot {
            BootBackend::Uefi { esp } => {
                for entry in entries {
                    if !keep.contains(&Some(entry.id.as_str())) {
                        fs::remove_file(esp.join("loader/entries").join(&entry.file))?;
                    }
                }
                sync_directory(&esp.join("loader/entries"))
            }
            BootBackend::Fit { firmware, layout } => {
                let mut env = Environment::load(firmware, *layout)?;
                env.records.retain(|r| keep.contains(&Some(r.id.as_str())));
                env.save(firmware)?;
                // Both redundant copies must forget retired records before
                // collection can remove their objects, including after retry.
                Environment::load(firmware, *layout)?.save(firmware)
            }
        }
    }

    pub fn describe(&self, keys: &[[u8; 32]]) -> Result<Vec<DeploymentStatus>> {
        use crate::components::{authenticate_deployment, component_id};
        self.entries()?
            .into_iter()
            .map(|entry| {
                let deployment = authenticate_deployment(
                    &read_bounded(
                        &self.system.join(format!("deployments/{}.json", entry.id)),
                        24576,
                    )?,
                    keys,
                )?;
                ensure!(
                    component_id(&serde_json::to_value(&deployment)?)? == entry.id
                        && deployment.generation == entry.generation,
                    "deployment status identity mismatch"
                );
                if let BootBackend::Fit { firmware, layout } = &self.boot {
                    ensure!(
                        Environment::load(firmware, *layout)?
                            .records
                            .iter()
                            .any(|r| r.id == entry.id && r.kernel_id == deployment.kernel.id),
                        "deployment boot record kernel mismatch"
                    );
                }
                Ok(DeploymentStatus {
                    entry,
                    version: deployment.version,
                    kernel_id: deployment.kernel.id,
                    kernel_release: deployment.kernel.release,
                    rootfs_id: deployment.rootfs.id,
                })
            })
            .collect()
    }

    pub fn confirm(&self, receipt: &BootReceipt) -> Result<State> {
        self.validate_receipt(receipt)?;
        let mut state = self.effective_state()?;
        let entries = self.entries()?;
        for entry in &entries {
            if entry.id != receipt.deployment_id
                && entry.tries_left == Some(0)
                && !state.failed.contains(&entry.id)
            {
                if state.failed.len() == 128 {
                    state.failed.remove(0);
                }
                state.failed.push(entry.id.clone());
            }
        }
        if state
            .candidate
            .as_ref()
            .is_some_and(|id| state.failed.contains(id))
        {
            state.candidate = None;
        }
        let current = entries
            .iter()
            .find(|e| e.id == receipt.deployment_id)
            .context("running deployment entry is missing")?;
        ensure!(current.tries_left != Some(3), "entry was never launched");
        ensure!(
            !state.failed.contains(&current.id),
            "deployment is already rejected"
        );
        let fallback = if state.current.as_deref() == Some(&current.id) {
            state.fallback.clone().filter(|id| {
                entries
                    .iter()
                    .any(|e| &e.id == id && e.id != current.id && e.tries_left != Some(0))
            })
        } else {
            state
                .current
                .clone()
                .filter(|id| {
                    entries
                        .iter()
                        .any(|e| &e.id == id && e.tries_left != Some(0))
                })
                .or_else(|| {
                    entries
                        .iter()
                        .find(|e| e.id != current.id && e.tries_left != Some(0))
                        .map(|e| e.id.clone())
                })
        };
        // The uncounted entry is the durable confirmation fact. If DATA recording
        // is interrupted, the next confirmation reconciles it without refilling.
        if current.tries_left.is_some() {
            self.set_tries(current, None)?;
        }
        state.current = Some(current.id.clone());
        state.fallback = fallback;
        if state.candidate.as_deref() == Some(&current.id) {
            state.candidate = None;
        }
        self.save_state(&state)?;
        self.retain_entries(
            &entries,
            &[
                state.current.as_deref(),
                state.fallback.as_deref(),
                state.candidate.as_deref(),
            ],
        )?;
        Ok(state)
    }

    /// Collect unreachable immutable objects after validating every retained
    /// descriptor. The caller holds the transaction lock and writable mounts.
    pub fn collect(&self, receipt: &BootReceipt, keys: &[[u8; 32]]) -> Result<usize> {
        let state = self.effective_state()?;
        let mut retained = BTreeSet::from([receipt.deployment_id.clone()]);
        for id in [state.current, state.fallback, state.candidate]
            .into_iter()
            .flatten()
        {
            retained.insert(id);
        }
        retained.extend(self.entries()?.into_iter().map(|entry| entry.id));
        let collection = self.collection(receipt, keys, retained, None)?;
        let entries = self.entries()?;
        self.retain_entries(
            &entries,
            &entries
                .iter()
                .map(|e| Some(e.id.as_str()))
                .collect::<Vec<_>>(),
        )?;
        collection.apply()
    }

    fn collection(
        &self,
        receipt: &BootReceipt,
        keys: &[[u8; 32]],
        retained: BTreeSet<String>,
        incoming: Option<&crate::components::Deployment>,
    ) -> Result<Collection> {
        use crate::components::{authenticate_deployment, component_id};
        self.validate_receipt(receipt)?;
        let mut roots = BTreeSet::new();
        let mut kernels = BTreeSet::new();
        let mut target = None;
        for id in &retained {
            valid_id(id)?;
            let path = self.system.join(format!("deployments/{id}.json"));
            let d = authenticate_deployment(&read_bounded(&path, 24576)?, keys)?;
            ensure!(
                component_id(&serde_json::to_value(&d)?)? == *id,
                "retained descriptor identity mismatch"
            );
            let board = (d.board, d.arch);
            if let Some(expected) = &target {
                ensure!(expected == &board, "retained deployment target mismatch");
            } else {
                target = Some(board);
            }
            if *id == receipt.deployment_id {
                ensure!(
                    d.kernel.id == receipt.kernel_id && d.rootfs.id == receipt.rootfs_id,
                    "running component identity mismatch"
                );
            }
            roots.insert(d.rootfs.id);
            kernels.insert(d.kernel.id);
        }
        if let Some(deployment) = incoming {
            ensure!(
                target.as_ref() == Some(&(deployment.board.clone(), deployment.arch.clone())),
                "incoming deployment target mismatch"
            );
            // A new deployment can reuse components belonging only to old B.
            // Keep those verified objects while retiring B's descriptor.
            roots.insert(deployment.rootfs.id.clone());
            kernels.insert(deployment.kernel.id.clone());
        }
        let mut files = Vec::new();
        let mut directories = Vec::new();
        for (parent, keep) in [
            (self.system.join("roots"), &roots),
            (self.system.join("kernels"), &kernels),
        ] {
            ensure!(
                parent.symlink_metadata()?.is_dir(),
                "invalid object namespace"
            );
            for entry in fs::read_dir(&parent)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid object name"))?;
                valid_id(&name)?;
                ensure!(entry.file_type()?.is_dir(), "object directory is not real");
                let referenced = keep.contains(&name);
                for child in fs::read_dir(entry.path())? {
                    let child = child?;
                    ensure!(child.file_type()?.is_file(), "unexpected object child");
                    let path = child.path();
                    if !referenced
                        || matches!(
                            path.extension().and_then(|s| s.to_str()),
                            Some("partial" | "pending")
                        )
                    {
                        files.push(path);
                    }
                    ensure!(files.len() <= 4096, "collection exceeds file limit");
                }
                if !referenced {
                    directories.push(entry.path());
                }
                ensure!(directories.len() <= 512, "collection exceeds object limit");
            }
        }
        let mut artifacts = vec![(
            self.system.join("deployments"),
            ".json",
            ".pending",
            &retained,
        )];
        if let BootBackend::Uefi { esp } = &self.boot {
            artifacts.push((esp.join("EFI/mica/kernels"), ".efi", ".partial", &kernels));
        }
        for (parent, suffix, temporary_suffix, keep) in artifacts {
            ensure!(
                parent.symlink_metadata()?.is_dir(),
                "invalid artifact namespace"
            );
            for entry in fs::read_dir(&parent)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid artifact name"))?;
                let temporary = name.ends_with(temporary_suffix);
                let id = name
                    .strip_suffix(if temporary { temporary_suffix } else { suffix })
                    .context("unexpected artifact name")?;
                valid_id(id)?;
                ensure!(entry.file_type()?.is_file(), "artifact is not regular");
                if temporary || !keep.contains(id) {
                    files.push(entry.path());
                }
                ensure!(files.len() <= 4096, "collection exceeds file limit");
            }
        }
        if let BootBackend::Uefi { esp } = &self.boot {
            for entry in fs::read_dir(esp.join("loader/entries"))? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_str().context("invalid entry name")?;
                if let Some(stem) = name.strip_suffix(".pending") {
                    selected_entry(&format!("{stem}.conf"))?;
                    ensure!(entry.file_type()?.is_file(), "pending entry is not regular");
                    files.push(entry.path());
                    ensure!(files.len() <= 4096, "collection exceeds file limit");
                }
            }
        }
        let pending = self.meta.join("deployments.pending");
        if pending.symlink_metadata().is_ok() {
            ensure!(
                pending.symlink_metadata()?.is_file(),
                "pending state is not regular"
            );
            if incoming.is_none() {
                files.push(pending);
            }
        }
        Ok(Collection { files, directories })
    }

    pub fn rollback(&self, receipt: &BootReceipt) -> Result<State> {
        self.validate_receipt(receipt)?;
        let state = self.effective_state()?;
        ensure!(state.candidate.is_none(), "candidate deployment is pending");
        ensure!(
            state.current.as_ref() == Some(&receipt.deployment_id)
                && !state.failed.contains(&receipt.deployment_id),
            "running deployment is not confirmed"
        );
        let fallback = state.fallback.as_ref().context("no retained fallback")?;
        ensure!(
            fallback != &receipt.deployment_id
                && !state.failed.contains(fallback)
                && self
                    .entries()?
                    .iter()
                    .any(|entry| &entry.id == fallback && entry.tries_left != Some(0)),
            "no usable retained fallback"
        );
        self.reject(&receipt.deployment_id)
    }

    pub fn reject(&self, id: &str) -> Result<State> {
        valid_id(id)?;
        let mut state = self.effective_state()?;
        let entries = self.entries()?;
        let entry = entries
            .iter()
            .find(|e| e.id == id)
            .context("deployment entry is missing")?;
        ensure!(
            entries
                .iter()
                .any(|e| e.id != id && e.tries_left != Some(0)),
            "cannot reject the last usable deployment"
        );
        ensure!(
            state.fallback.as_deref() != Some(id),
            "cannot reject retained fallback"
        );
        if entry.tries_left != Some(0) {
            self.set_tries(entry, Some(0))?;
        }
        if !state.failed.iter().any(|failed| failed == id) {
            if state.failed.len() == 128 {
                state.failed.remove(0);
            }
            state.failed.push(id.to_owned());
        }
        if state.candidate.as_deref() == Some(id) {
            state.candidate = None;
        }
        self.save_state(&state)?;
        Ok(state)
    }
}

pub fn verify_file(path: &Path, artifact: &crate::components::Artifact) -> Result<()> {
    let meta = path.symlink_metadata()?;
    ensure!(
        meta.is_file() && meta.len() == artifact.bytes,
        "object length or type mismatch: {}",
        path.display()
    );
    let mut file = File::open(path)?;
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0_u8; 65536];
    let mut bytes = 0;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
        ensure!(bytes <= artifact.bytes, "object grew during verification");
        digest.update(&buffer[..read]);
    }
    ensure!(
        bytes == artifact.bytes && hex::encode(digest.finish()) == artifact.sha256,
        "object digest mismatch: {}",
        path.display()
    );
    Ok(())
}

pub(crate) fn directory(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty() && *parent != path)
    {
        directory(parent)?;
    }
    if path.symlink_metadata().is_ok() {
        ensure!(
            path.symlink_metadata()?.is_dir(),
            "invalid object directory"
        );
    } else {
        fs::create_dir(path)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

fn publish_object(
    source: &Path,
    target: &Path,
    artifact: &crate::components::Artifact,
) -> Result<()> {
    if target.symlink_metadata().is_ok() {
        return verify_file(target, artifact);
    }
    let parent = target.parent().context("object has no parent")?;
    directory(parent)?;
    let pending = target.with_extension("partial");
    if pending.symlink_metadata().is_ok() {
        ensure!(
            pending.symlink_metadata()?.is_file(),
            "invalid partial object"
        );
        fs::remove_file(&pending)?;
    }
    let mut input = File::open(source)?.take(artifact.bytes + 1);
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&pending)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    verify_file(&pending, artifact)?;
    fs::rename(&pending, target)?;
    sync_directory(parent)
}

impl DeploymentStore {
    pub fn object_paths<'a>(
        &self,
        deployment: &'a crate::components::Deployment,
    ) -> [(PathBuf, &'a crate::components::Artifact); 5] {
        [
            (
                self.system
                    .join(format!("roots/{}/rootfs.img", deployment.rootfs.id)),
                &deployment.rootfs.content.image,
            ),
            (
                self.system.join(format!(
                    "roots/{}/rootfs.roothash.p7s",
                    deployment.rootfs.id
                )),
                &deployment.rootfs.content.signature,
            ),
            (
                self.system
                    .join(format!("kernels/{}/support.img", deployment.kernel.id)),
                &deployment.kernel.support.image,
            ),
            (
                self.system.join(format!(
                    "kernels/{}/support.roothash.p7s",
                    deployment.kernel.id
                )),
                &deployment.kernel.support.signature,
            ),
            (
                match &self.boot {
                    BootBackend::Uefi { esp } => {
                        esp.join(format!("EFI/mica/kernels/{}.efi", deployment.kernel.id))
                    }
                    BootBackend::Fit { .. } => self
                        .system
                        .join(format!("kernels/{}/boot.itb", deployment.kernel.id)),
                },
                &deployment.kernel.boot.artifact,
            ),
        ]
    }

    /// Caller owns the DATA transaction lock and writable SYSTEM/ESP mounts.
    /// Object sources are addressed only by authenticated SHA-256 digests.
    pub fn install(
        &self,
        envelope: &[u8],
        keys: &[[u8; 32]],
        board: &str,
        arch: &str,
        objects: &Path,
        receipt: &BootReceipt,
    ) -> Result<State> {
        use crate::components::{authenticate_deployment, component_id};
        let deployment = authenticate_deployment(envelope, keys)?;
        ensure!(
            deployment.board == board && deployment.arch == arch,
            "deployment targets another device"
        );
        ensure!(
            deployment.kernel.boot.format
                == match self.boot {
                    BootBackend::Uefi { .. } => "uki",
                    BootBackend::Fit { .. } => "fit",
                },
            "deployment boot format differs from this device"
        );
        let id = component_id(&serde_json::to_value(&deployment)?)?;
        let mut state = self.effective_state()?;
        let entries = self.entries()?;
        ensure!(
            !entries.iter().any(|e| e.id == id) && !state.failed.contains(&id),
            "deployment is installed or rejected"
        );
        ensure!(state.candidate.is_none(), "another deployment is pending");
        self.validate_receipt(receipt)?;
        ensure!(
            state.current.as_deref() == Some(receipt.deployment_id.as_str())
                && entries
                    .iter()
                    .any(|e| e.id == receipt.deployment_id && e.tries_left.is_none())
                && !state.failed.contains(&receipt.deployment_id),
            "confirm the running deployment before installation"
        );
        ensure!(
            deployment.generation > state.highest_generation
                && entries.iter().all(|e| deployment.generation > e.generation),
            "deployment generation is not newer"
        );
        ensure!(entries.len() <= 2, "retained deployment set is full");
        let files = self.object_paths(&deployment);
        // Validate every missing input and reserve both destination filesystems
        // before exposing any candidate. Reused objects require no source copy.
        let mut system_bytes = 0_u64;
        let mut esp_bytes = 0_u64;
        for (target, artifact) in &files {
            if target.symlink_metadata().is_ok() {
                verify_file(target, artifact)?;
                continue;
            }
            verify_file(&objects.join(&artifact.sha256), artifact)?;
            if target.starts_with(&self.system) {
                system_bytes += artifact.bytes;
            } else {
                esp_bytes += artifact.bytes;
            }
        }
        let collection = self.collection(
            receipt,
            keys,
            BTreeSet::from([receipt.deployment_id.clone()]),
            Some(&deployment),
        )?;
        let mut destinations = vec![(&self.system, system_bytes, 128 * 1024 * 1024)];
        if let BootBackend::Uefi { esp } = &self.boot {
            destinations.push((esp, esp_bytes, 64 * 1024 * 1024));
        }
        for &(path, bytes, reserve) in &destinations {
            directory(path)?;
            let space = rustix::fs::statvfs(path)?;
            ensure!(
                space.f_bavail * space.f_frsize + collection.reclaimed_bytes(path)?
                    >= bytes + reserve,
                "insufficient destination space: {}",
                path.display()
            );
            ensure!(
                space.f_files == 0 || space.f_favail >= 32,
                "insufficient destination inodes"
            );
        }
        // No new object is written until old B is durably unbootable. Native
        // records are authoritative if DATA reconciliation is interrupted.
        self.retain_entries(&entries, &[Some(&receipt.deployment_id)])?;
        state.fallback = None;
        state.candidate = None;
        self.save_state(&state)?;
        collection.apply()?;
        for &(path, bytes, reserve) in &destinations {
            let space = rustix::fs::statvfs(path)?;
            ensure!(
                space.f_bavail * space.f_frsize >= bytes + reserve,
                "insufficient reclaimed destination space: {}",
                path.display()
            );
        }
        for (target, artifact) in &files {
            publish_object(&objects.join(&artifact.sha256), target, artifact)?;
        }
        for (path, hash) in [
            (
                self.system
                    .join(format!("roots/{}/rootfs.roothash", deployment.rootfs.id)),
                &deployment.rootfs.content.root_hash,
            ),
            (
                self.system
                    .join(format!("kernels/{}/support.roothash", deployment.kernel.id)),
                &deployment.kernel.support.root_hash,
            ),
        ] {
            if path.try_exists()? {
                ensure!(
                    read_bounded(&path, 64)? == hash.as_bytes(),
                    "existing root hash differs"
                );
            } else {
                atomic_write(&path, hash.as_bytes())?;
            }
        }
        directory(&self.system.join("deployments"))?;
        let descriptor = self.system.join(format!("deployments/{id}.json"));
        if descriptor.try_exists()? {
            ensure!(
                read_bounded(&descriptor, 24576)? == envelope,
                "deployment ID was reused"
            );
        } else {
            atomic_write(&descriptor, envelope)?;
        }
        sync_directory(&self.system)?;
        // The entry is the activation commit. All referenced bytes are durable.
        match &self.boot {
            BootBackend::Uefi { esp } => {
                sync_directory(&esp.join("EFI/mica/kernels"))?;
                let entry = format!(
                    "title MICA {}\nversion {}\nsort-key mica\nefi /EFI/mica/kernels/{}.efi\n",
                    deployment.version, deployment.generation, deployment.kernel.id
                );
                atomic_write(
                    &esp.join(format!("loader/entries/mica-{id}+3.conf")),
                    entry.as_bytes(),
                )?;
            }
            BootBackend::Fit { firmware, layout } => {
                let mut env = Environment::load(firmware, *layout)?;
                env.records.insert(
                    0,
                    Record {
                        id: id.clone(),
                        kernel_id: deployment.kernel.id,
                        generation: deployment.generation,
                        tries_left: Some(3),
                    },
                );
                env.save(firmware)?;
            }
        }
        state.highest_generation = deployment.generation;
        state.candidate = Some(id);
        self.save_state(&state)?;
        Ok(state)
    }
}

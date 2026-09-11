//! One bounded storage-release policy for exitrd and partial startup refusal.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Reboot,
    Poweroff,
    Halt,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reboot => "reboot",
            Self::Poweroff => "poweroff",
            Self::Halt => "halt",
        }
    }
    pub fn parse(args: &[String]) -> Result<Self> {
        Ok(Request::parse(args)?.action)
    }
}

pub struct Request {
    pub action: Action,
    pub timeout_ms: Option<u64>,
}
impl Request {
    pub fn parse(args: &[String]) -> Result<Self> {
        ensure!(
            !args.is_empty()
                && args.len() <= 16
                && args.iter().map(String::len).sum::<usize>() <= 1024,
            "invalid shutdown arguments"
        );
        let action = match args[0].as_str() {
            "reboot" => Action::Reboot,
            "poweroff" => Action::Poweroff,
            "halt" => Action::Halt,
            _ => bail!("unsupported shutdown action"),
        };
        let mut seen = BTreeSet::new();
        let mut timeout_ms = None;
        let mut iter = args[1..].iter();
        while let Some(arg) = iter.next() {
            let (key, value) = if let Some(pair) = arg.split_once('=') {
                pair
            } else if ["--log-color", "--log-location", "--log-time"].contains(&arg.as_str()) {
                (arg.as_str(), "true")
            } else {
                (
                    arg.as_str(),
                    iter.next()
                        .context("missing shutdown metadata value")?
                        .as_str(),
                )
            };
            ensure!(seen.insert(key), "duplicate shutdown metadata");
            let level = |s: &str| {
                [
                    "emerg", "alert", "crit", "err", "warning", "notice", "info", "debug",
                ]
                .contains(&s)
                    || s.parse::<u8>().is_ok_and(|n| n <= 7)
            };
            let target = |s: &str| {
                [
                    "console",
                    "console-prefixed",
                    "kmsg",
                    "syslog",
                    "syslog-or-kmsg",
                    "journal",
                    "journal-or-kmsg",
                    "auto",
                    "null",
                ]
                .contains(&s)
            };
            let valid = match key {
                "--log-level" => {
                    value.split(',').count() <= 8
                        && value.split(',').all(|part| {
                            part.split_once(':').map_or_else(
                                || level(part),
                                |(name, value)| target(name) && level(value),
                            )
                        })
                }
                "--log-target" => target(value),
                "--log-color" | "--log-location" | "--log-time" => {
                    ["0", "1", "yes", "no", "true", "false"].contains(&value)
                }
                "--exit-code" => value.parse::<u8>().is_ok(),
                "--timeout" => {
                    let (number, multiplier) = if let Some(n) = value.strip_suffix("us") {
                        (n, 1)
                    } else if let Some(n) = value.strip_suffix("ms") {
                        (n, 1000)
                    } else {
                        (value.strip_suffix('s').unwrap_or(value), 1_000_000)
                    };
                    let micros = number
                        .parse::<u64>()?
                        .checked_mul(multiplier)
                        .context("shutdown metadata timeout overflow")?;
                    ensure!(
                        (1000..=3_600_000_000).contains(&micros),
                        "invalid shutdown metadata timeout"
                    );
                    timeout_ms = Some(micros / 1000);
                    true
                }
                _ => false,
            };
            ensure!(valid, "unsupported shutdown metadata");
        }
        Ok(Self { action, timeout_ms })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub deadline_ms: u64,
    pub cleanup_deadline_ms: u64,
}
impl Budget {
    pub fn new(now: u64, watchdog_seconds: u64) -> Result<Self> {
        ensure!(
            (20..=86400).contains(&watchdog_seconds),
            "inadequate watchdog timeout"
        );
        let duration = (watchdog_seconds - 10).min(60) * 1000;
        let deadline_ms = now.checked_add(duration).context("deadline overflow")?;
        Ok(Self {
            deadline_ms,
            cleanup_deadline_ms: deadline_ms - 2000,
        })
    }
    pub fn operation_deadline(self, now: u64) -> Result<u64> {
        ensure!(now < self.cleanup_deadline_ms, "cleanup deadline exhausted");
        Ok(now.saturating_add(5000).min(self.cleanup_deadline_ms))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Device {
    pub major: u32,
    pub minor: u32,
}
impl Device {
    fn parse(value: &str) -> Result<Self> {
        let (major, minor) = value.split_once(':').context("invalid device identity")?;
        Ok(Self {
            major: major.parse()?,
            minor: minor.parse()?,
        })
    }
    pub fn from_raw(value: u64) -> Self {
        Self {
            major: rustix::fs::major(value),
            minor: rustix::fs::minor(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub id: u64,
    pub parent: u64,
    pub device: Device,
    pub root: String,
    pub path: String,
    pub kind: String,
    pub propagation: Vec<String>,
}

fn unescape(value: &str) -> Result<String> {
    let mut bytes = Vec::new();
    let mut input = value.as_bytes();
    while !input.is_empty() {
        if input[0] == b'\\' {
            ensure!(input.len() >= 4, "invalid mount escape");
            bytes.push(match &input[1..4] {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => bail!("invalid mount escape"),
            });
            input = &input[4..];
        } else {
            ensure!(input[0] != 0, "invalid mount path");
            bytes.push(input[0]);
            input = &input[1..];
        }
    }
    let value = String::from_utf8(bytes)?;
    ensure!(
        value.starts_with('/') && value.len() <= 4096,
        "invalid mount path"
    );
    Ok(value)
}

pub fn parse_mountinfo(text: &str) -> Result<Vec<Mount>> {
    ensure!(
        !text.is_empty() && text.len() <= 1024 * 1024 && text.lines().count() <= 4096,
        "invalid mountinfo size"
    );
    let mut mounts = Vec::new();
    let mut parents = BTreeMap::new();
    for line in text.lines() {
        let (left, right) = line.split_once(" - ").context("malformed mountinfo")?;
        let left: Vec<_> = left.split(' ').collect();
        let right: Vec<_> = right.split(' ').collect();
        ensure!(
            left.len() >= 6 && right.len() == 3 && left.iter().chain(&right).all(|x| !x.is_empty()),
            "malformed mountinfo fields"
        );
        let id = left[0].parse::<u64>()?;
        let parent = left[1].parse::<u64>()?;
        ensure!(
            id != 0 && (id != parent || left[4] == "/") && parents.insert(id, parent).is_none(),
            "invalid mount ID"
        );
        mounts.push(Mount {
            id,
            parent,
            device: Device::parse(left[2])?,
            root: unescape(left[3])?,
            path: unescape(left[4])?,
            kind: right[0].into(),
            propagation: left[6..].iter().map(|s| (*s).into()).collect(),
        });
    }
    ensure!(
        mounts.iter().any(|m| m.path == "/"),
        "mountinfo has no root"
    );
    for mount in &mounts {
        let mut current = mount.id;
        let mut visited = BTreeSet::new();
        while let Some(&parent) = parents.get(&current) {
            ensure!(visited.insert(current), "cyclic mountinfo");
            if current == parent {
                break;
            }
            current = parent;
        }
    }
    Ok(mounts)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopIdentity {
    pub device: Device,
    pub generation: u64,
    pub backing: Device,
    pub inode: u64,
    pub offset: u64,
    pub size_limit: u64,
    pub flags: u32,
}
impl LoopIdentity {
    fn same_association(&self, other: &Self) -> bool {
        self.device == other.device
            && self.generation == other.generation
            && self.backing == other.backing
            && self.inode == other.inode
            && self.offset == other.offset
            && self.size_limit == other.size_limit
            && self.flags & !4 == other.flags & !4
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    pub device: Device,
    pub generation: u64,
    pub name: String,
    pub uuid: String,
    pub table: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub device: Device,
    pub generation: u64,
    pub name: String,
    pub holders: BTreeSet<Device>,
    pub slaves: BTreeSet<Device>,
    pub association: Option<LoopIdentity>,
    pub mapping: Option<Mapping>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ownership {
    pub deployment: String,
    pub backings: BTreeSet<Device>,
    pub backing_generations: Vec<(Device, u64)>,
    pub loops: Vec<LoopIdentity>,
    pub mappings: Vec<Mapping>,
    pub mounts: Vec<Mount>,
    pub allow_extra_loops: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub mounts: Vec<Mount>,
    pub blocks: Vec<Block>,
    pub processes: Vec<u32>,
    pub swaps: Vec<String>,
    pub dirty_kib: u64,
}

fn memory_kind(kind: &str) -> bool {
    [
        "tmpfs",
        "ramfs",
        "rootfs",
        "devtmpfs",
        "proc",
        "sysfs",
        "devpts",
        "cgroup2",
        "efivarfs",
        "securityfs",
        "debugfs",
        "tracefs",
        "bpf",
        "pstore",
        "configfs",
        "mqueue",
        "hugetlbfs",
    ]
    .contains(&kind)
}
fn protected(mount: &Mount) -> bool {
    memory_kind(&mount.kind)
        && (mount.path == "/"
            || mount.path == "/run"
            || ["/dev", "/proc", "/sys"]
                .iter()
                .any(|p| mount.path == *p || mount.path.starts_with(&format!("{p}/"))))
}

impl Snapshot {
    fn validate(&self, owner: &Ownership) -> Result<()> {
        ensure!(
            self.mounts.len() <= 4096 && self.blocks.len() <= 4096 && self.processes.len() <= 4096,
            "excessive live graph"
        );
        ensure!(self.swaps.is_empty(), "outstanding swap users");
        ensure!(
            self.mounts
                .iter()
                .all(|m| m.propagation.iter().all(|p| p == "unbindable")),
            "mount propagation is not private"
        );
        ensure!(
            self.blocks.iter().all(|b| b
                .association
                .as_ref()
                .is_none_or(|l| owner.backings.contains(&l.backing)
                    || owner.loops.iter().any(|known| known.same_association(l)))),
            "unknown active loop backing"
        );
        ensure!(
            self.blocks.iter().all(|b| b
                .mapping
                .as_ref()
                .is_none_or(|m| owner.mappings.contains(m))
                && (b.slaves.is_empty() || b.mapping.is_some())),
            "unknown active block layer"
        );
        let root = self
            .mounts
            .iter()
            .find(|m| m.path == "/")
            .context("missing exitrd root")?;
        ensure!(
            memory_kind(&root.kind),
            "PID1 root still uses persistent storage"
        );
        for (device, generation) in &owner.backing_generations {
            if let Some(block) = self.blocks.iter().find(|b| b.device == *device) {
                ensure!(
                    block.generation == *generation,
                    "backing block device was reused"
                );
            } else {
                ensure!(
                    !self.mounts.iter().any(|m| m.device == *device)
                        && !self
                            .blocks
                            .iter()
                            .any(|b| b.association.as_ref().is_some_and(|l| l.backing == *device)),
                    "backing device disappeared while still in use"
                );
            }
        }
        for expected in &owner.loops {
            if let Some(current) = self
                .blocks
                .iter()
                .find(|b| b.device == expected.device)
                .and_then(|b| b.association.as_ref())
            {
                ensure!(
                    expected.same_association(current),
                    "reused or changed loop association"
                );
            }
        }
        if !owner.allow_extra_loops {
            ensure!(
                self.blocks
                    .iter()
                    .filter_map(|b| b.association.as_ref())
                    .all(|l| !owner.backings.contains(&l.backing)
                        || owner.loops.iter().any(|known| known.same_association(l))),
                "unknown partial-startup loop ownership"
            );
        }
        for expected in &owner.mappings {
            if let Some(block) = self.blocks.iter().find(|b| b.device == expected.device) {
                ensure!(
                    block.mapping.as_ref() == Some(expected),
                    "reused or changed MOS mapping"
                );
            }
        }
        let owned = self.owned_mounts(owner);
        ensure!(
            self.mounts
                .iter()
                .all(|m| protected(m) || owned.contains(&m.id)),
            "unknown mount ownership"
        );
        Ok(())
    }
    fn owned_mounts(&self, owner: &Ownership) -> BTreeSet<u64> {
        let devices: BTreeSet<_> = owner
            .backings
            .iter()
            .copied()
            .chain(owner.mappings.iter().map(|m| m.device))
            .collect();
        let mut ids: BTreeSet<_> =
            self.mounts
                .iter()
                .filter(|m| {
                    devices.contains(&m.device)
                        || owner.mounts.iter().any(|old| {
                            old.id == m.id && old.device == m.device && old.root == m.root
                        })
                })
                .map(|m| m.id)
                .collect();
        for _ in 0..self.mounts.len() {
            let old = ids.len();
            for mount in &self.mounts {
                if ids.contains(&mount.parent) {
                    ids.insert(mount.id);
                }
            }
            if ids.len() == old {
                break;
            }
        }
        ids
    }
    fn owned_loops<'a>(&'a self, owner: &Ownership) -> Vec<&'a LoopIdentity> {
        self.blocks
            .iter()
            .filter_map(|b| b.association.as_ref())
            .filter(|l| {
                owner.backings.contains(&l.backing)
                    || owner.loops.iter().any(|old| old.same_association(l))
            })
            .collect()
    }
    fn empty(&self, owner: &Ownership) -> bool {
        self.processes.is_empty()
            && self.swaps.is_empty()
            && self.dirty_kib == 0
            && self.mounts.iter().all(protected)
            && self.owned_loops(owner).is_empty()
            && !self.blocks.iter().any(|b| {
                b.mapping
                    .as_ref()
                    .is_some_and(|m| owner.mappings.contains(m))
            })
            && !self.blocks.iter().any(|b| {
                (owner.backings.contains(&b.device)
                    || owner.loops.iter().any(|l| l.device == b.device))
                    && !b.holders.is_empty()
            })
    }
    fn candidates(&self, owner: &Ownership) -> Vec<Operation> {
        let mut result = Vec::new();
        let loops = self.owned_loops(owner);
        for mount in &self.mounts {
            if protected(mount) || self.mounts.iter().any(|m| m.parent == mount.id) {
                continue;
            }
            if loops.iter().any(|l| l.backing == mount.device) {
                if !mount.path.starts_with("/backing/") {
                    result.push(Operation::MoveBacking(mount.clone()));
                }
            } else {
                result.push(Operation::Unmount(mount.clone()));
            }
        }
        for block in &self.blocks {
            if !block.holders.is_empty() || self.mounts.iter().any(|m| m.device == block.device) {
                continue;
            }
            if let Some(mapping) = &block.mapping
                && owner.mappings.contains(mapping)
            {
                result.push(Operation::RemoveMapping(mapping.clone()));
            }
            if let Some(association) = &block.association
                && loops.contains(&association)
            {
                result.push(Operation::DetachLoop(association.clone()));
            }
        }
        result
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Operation {
    Scan,
    Private,
    Quiesce,
    Sync,
    SyncMount(Mount),
    Unmount(Mount),
    MoveBacking(Mount),
    RemoveMapping(Mapping),
    DetachLoop(LoopIdentity),
    Retire {
        id: String,
        backend: super::BootKind,
        board: String,
        system: String,
        system_device: String,
        boot_device: String,
    },
}

/// Deterministic policy boundary. Production supplies only fixed typed workers.
pub trait LifecycleIo {
    fn now_ms(&self) -> u64;
    fn scan(&mut self, deadline_ms: u64) -> Result<Snapshot>;
    fn execute(&mut self, operation: &Operation, deadline_ms: u64) -> Result<()>;
    fn event(&mut self, stage: &str, detail: &str);
    fn terminal(&mut self, _action: Action, _released: Released) -> Result<()> {
        bail!("terminal action unavailable")
    }
}

fn remaining(state: &Snapshot, owner: &Ownership) -> String {
    let mounts: Vec<_> = state.mounts.iter().filter(|m| !protected(m)).collect();
    format!(
        "mounts={} mappings={} loops={} backings={} users={} swaps={} dirtyKiB={} mountIds={:?} blockIds={:?}",
        mounts.len(),
        state.blocks.iter().filter(|b| b.mapping.is_some()).count(),
        state
            .blocks
            .iter()
            .filter(|b| b.association.is_some())
            .count(),
        owner
            .backings
            .iter()
            .filter(|dev| mounts.iter().any(|m| m.device == **dev))
            .count(),
        state.processes.len(),
        state.swaps.len(),
        state.dirty_kib,
        mounts.iter().take(8).map(|m| m.id).collect::<Vec<_>>(),
        state
            .blocks
            .iter()
            .filter(|b| b.association.is_some() || b.mapping.is_some() || !b.holders.is_empty())
            .take(8)
            .map(|b| b.device)
            .collect::<Vec<_>>()
    )
}

pub struct Released {
    _private: (),
}

/// No requested action can be authorized by a tool status or a shell marker.
pub fn release(io: &mut impl LifecycleIo, budget: Budget, owner: &Ownership) -> Result<Released> {
    ensure!(
        owner.mappings.len() <= 2 && owner.loops.len() <= 128 && owner.mounts.len() <= 4096,
        "excessive ownership record"
    );
    ensure!(
        owner.backing_generations.len() == owner.backings.len()
            && owner.backings.len() <= 3
            && owner
                .backing_generations
                .iter()
                .all(|(dev, generation)| *generation > 0 && owner.backings.contains(dev))
            && owner
                .backing_generations
                .iter()
                .map(|(dev, _)| *dev)
                .collect::<BTreeSet<_>>()
                == owner.backings,
        "invalid backing ownership generations"
    );
    let mut names = BTreeSet::new();
    ensure!(
        owner.mappings.iter().all(
            |m| ["mos-root", "mos-support"].contains(&m.name.as_str()) && names.insert(&m.name)
        ),
        "invalid owned mapping name"
    );
    io.execute(&Operation::Private, budget.operation_deadline(io.now_ms())?)?;
    io.execute(&Operation::Quiesce, budget.operation_deadline(io.now_ms())?)?;
    let initial = io.scan(budget.operation_deadline(io.now_ms())?)?;
    initial
        .validate(owner)
        .with_context(|| remaining(&initial, owner))?;
    ensure!(
        initial.processes.is_empty(),
        "userspace holders remain after quiesce"
    );
    io.event("quiesced", "users=0");
    let mut previous = None;
    for pass in 0..12 {
        let mut tried = BTreeSet::new();
        for _ in 0..512 {
            let state = io.scan(budget.operation_deadline(io.now_ms())?)?;
            state
                .validate(owner)
                .with_context(|| remaining(&state, owner))?;
            if let Some(operation) = previous.take() {
                let observed = match &operation {
                    Operation::Unmount(m) => !state.mounts.iter().any(|current| current.id == m.id),
                    Operation::MoveBacking(m) => state.mounts.iter().any(|current| {
                        current.id == m.id
                            && current.device == m.device
                            && current.path == format!("/backing/{}", m.id)
                    }),
                    Operation::RemoveMapping(m) => {
                        !state.blocks.iter().any(|b| b.device == m.device)
                    }
                    Operation::DetachLoop(l) => !state.blocks.iter().any(|b| {
                        b.device == l.device && (b.association.is_some() || !b.holders.is_empty())
                    }),
                    _ => false,
                };
                if observed {
                    io.event("release-observed", &serde_json::to_string(&operation)?);
                }
            }
            ensure!(
                state.processes.is_empty(),
                "new userspace holder during shutdown"
            );
            if state.empty(owner) {
                io.event(
                    "empty-observation",
                    "mounts=0 mappings=0 loops=0 backings=0",
                );
                io.execute(&Operation::Sync, budget.operation_deadline(io.now_ms())?)?;
                let final_state = io.scan(budget.operation_deadline(io.now_ms())?)?;
                final_state
                    .validate(owner)
                    .with_context(|| remaining(&final_state, owner))?;
                ensure!(final_state.empty(owner), "storage changed after sync");
                io.event(
                    "storage-released",
                    "observations=2 mounts=0 mappings=0 loops=0 backings=0",
                );
                return Ok(Released { _private: () });
            }
            let mut next = None;
            for operation in state.candidates(owner) {
                let key = serde_json::to_string(&operation)?;
                if tried.insert(key) {
                    next = Some(operation);
                    break;
                }
            }
            let Some(operation) = next else {
                io.event("remaining", &remaining(&state, owner));
                if state.dirty_kib > 0 {
                    io.execute(&Operation::Sync, budget.operation_deadline(io.now_ms())?)?;
                }
                break;
            };
            if let Operation::Unmount(mount) = &operation {
                io.execute(
                    &Operation::SyncMount(mount.clone()),
                    budget.operation_deadline(io.now_ms())?,
                )?;
            }
            match io.execute(&operation, budget.operation_deadline(io.now_ms())?) {
                Ok(()) => io.event("operation-returned", &serde_json::to_string(&operation)?),
                Err(error) => io.event("operation-refused", &format!("pass={pass} {error:#}")),
            }
            previous = Some(operation);
            // Always re-observe, including after refusal: tools can mutate then fail.
        }
    }
    bail!("storage-not-released after twelve passes")
}

/// A returned terminal operation is always a failure, including status zero.
pub fn finish(
    io: &mut impl LifecycleIo,
    budget: Budget,
    owner: &Ownership,
    action: Action,
) -> Result<()> {
    let released = release(io, budget, owner)?;
    io.terminal(action, released)?;
    bail!("terminal action returned")
}

/// Bounded best-effort console transport. A failed transition record prevents
/// terminal authorization; failure diagnostics never wait on a full console.
pub fn diagnostic(message: &str) -> Result<()> {
    write_diagnostic(rustix::stdio::stderr(), message)
}
fn write_diagnostic(fd: impl std::os::fd::AsFd, message: &str) -> Result<()> {
    let fd = fd.as_fd();
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    let mut line = message.as_bytes()[..message.len().min(1023)].to_vec();
    line.push(b'\n');
    ensure!(
        rustix::io::write(fd, &line)? == line.len(),
        "incomplete lifecycle console write"
    );
    Ok(())
}

/// The only watchdog owner. All files are close-on-exec; children cannot feed it.
pub struct Supervisor {
    started: Instant,
    watchdog: Option<File>,
    timeout: u64,
    last_kick: u64,
    armed: bool,
    poisoned: bool,
    budget: Option<Budget>,
}
impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}
impl Supervisor {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            watchdog: None,
            timeout: 0,
            last_kick: 0,
            armed: false,
            poisoned: false,
            budget: None,
        }
    }
    pub fn now_ms(&self) -> u64 {
        self.started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
    pub fn arm(&mut self) -> Result<()> {
        ensure!(self.watchdog.is_none(), "watchdog already owned");
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(
                rustix::fs::OFlags::CLOEXEC.bits() as i32
                    | rustix::fs::OFlags::NONBLOCK.bits() as i32
                    | rustix::fs::OFlags::NOFOLLOW.bits() as i32,
            )
            .open("/dev/watchdog0")
            .context("required watchdog unavailable")?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.file_type().is_char_device(),
            "watchdog is not a character device"
        );
        let device = Device::parse(read_text("/sys/class/watchdog/watchdog0/dev", 64)?.trim())?;
        ensure!(
            Device::from_raw(metadata.rdev()) == device,
            "watchdog device identity mismatch"
        );
        self.watchdog = Some(file);
        let fd = self.watchdog.as_ref().context("watchdog missing")?;
        lifecycle_sys::watchdog_support(fd)?;
        self.timeout = lifecycle_sys::watchdog_timeout(fd)?;
        Budget::new(self.now_ms(), self.timeout)?;
        ensure!(
            read_text("/sys/class/watchdog/watchdog0/nowayout", 64)?.trim() == "1",
            "watchdog NOWAYOUT is not enforced"
        );
        lifecycle_sys::watchdog_keepalive(fd)?;
        self.armed = true;
        self.last_kick = self.now_ms();
        Ok(())
    }
    fn kick(&mut self) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        let now = self.now_ms();
        if self.budget.is_some_and(|b| now >= b.deadline_ms) {
            bail!("watchdog feeding deadline exhausted");
        }
        if now.saturating_sub(self.last_kick) >= 1000 {
            lifecycle_sys::watchdog_keepalive(self.watchdog.as_ref().context("watchdog missing")?)?;
            self.last_kick = now;
        }
        Ok(())
    }
    pub fn begin_shutdown(&mut self) -> Result<Budget> {
        ensure!(self.armed, "shutdown watchdog was not verified armed");
        if let Some(budget) = self.budget {
            return Ok(budget);
        }
        let actual =
            lifecycle_sys::watchdog_timeout(self.watchdog.as_ref().context("watchdog missing")?)?;
        self.timeout = actual;
        let budget = Budget::new(self.now_ms(), actual)?;
        self.budget = Some(budget);
        Ok(budget)
    }
    pub fn limit_shutdown(&mut self, limit_ms: Option<u64>) -> Result<Budget> {
        let mut budget = self.begin_shutdown()?;
        if let Some(limit) = limit_ms {
            ensure!(limit >= 3000, "inadequate systemd shutdown timeout");
            budget.deadline_ms = budget.deadline_ms.min(self.now_ms().saturating_add(limit));
            budget.cleanup_deadline_ms = budget.cleanup_deadline_ms.min(budget.deadline_ms - 2000);
            self.budget = Some(budget);
        }
        Ok(budget)
    }
    pub fn observe(&mut self, executable: &'static str) -> Result<Snapshot> {
        let deadline = if let Some(b) = self.budget {
            b.operation_deadline(self.now_ms())?
        } else {
            self.now_ms().saturating_add(5000)
        };
        SystemIo {
            supervisor: self,
            executable,
        }
        .scan(deadline)
    }
    pub fn prepare_process(&self) -> Result<()> {
        use std::os::fd::AsRawFd;
        std::env::set_current_dir("/")?;
        let console = OpenOptions::new()
            .read(true)
            .write(true)
            .open(devfs()?.join("console"))?;
        ensure!(
            console.metadata()?.file_type().is_char_device(),
            "console is not a character device"
        );
        rustix::stdio::dup2_stdin(&console)?;
        rustix::stdio::dup2_stdout(&console)?;
        rustix::stdio::dup2_stderr(&console)?;
        drop(console);
        let proc = procfs()?;
        let root_device = fs::metadata("/")?.dev();
        ensure!(
            fs::metadata(proc.join("self/exe"))?.dev() == root_device,
            "executable pins persistent storage"
        );
        for line in read_text(proc.join("self/maps"), 1024 * 1024)?.lines() {
            let parts: Vec<_> = line.split_whitespace().collect();
            ensure!(parts.len() >= 5, "invalid executable mappings");
            let (major, minor) = parts[3].split_once(':').context("invalid mapped device")?;
            let device = Device {
                major: u32::from_str_radix(major, 16)?,
                minor: u32::from_str_radix(minor, 16)?,
            };
            ensure!(
                parts[4] == "0" || device == Device::from_raw(root_device),
                "library or mapping pins persistent storage"
            );
        }
        let names = list(&proc.join("self/fd"), 1024)?;
        for path in names {
            let number = path
                .file_name()
                .and_then(|s| s.to_str())
                .context("invalid descriptor")?
                .parse::<i32>()?;
            if number <= 2
                || self
                    .watchdog
                    .as_ref()
                    .is_some_and(|fd| fd.as_raw_fd() == number)
            {
                continue;
            }
            ensure!(!path.exists(), "unexpected inherited descriptor {number}");
        }
        Ok(())
    }
    pub fn run_startup(&mut self, command: Command) -> Result<String> {
        let program = command
            .get_program()
            .to_str()
            .context("invalid startup command")?;
        ensure!(
            [
                "/bin/busybox",
                "/sbin/blkid",
                "/sbin/veritysetup",
                "/sbin/dmsetup"
            ]
            .contains(&program),
            "unapproved startup executable"
        );
        let deadline = if let Some(budget) = self.budget {
            budget.operation_deadline(self.now_ms())?
        } else {
            self.now_ms().saturating_add(30000)
        };
        let output = self.run(command, deadline, 16384)?;
        Ok(String::from_utf8(output)?.trim().into())
    }
    fn run(&mut self, mut command: Command, deadline: u64, limit: usize) -> Result<Vec<u8>> {
        ensure!(!self.poisoned, "unreaped or failed supervised work remains");
        ensure!(
            self.now_ms().saturating_add(1000) < deadline,
            "insufficient child deadline margin"
        );
        let mut child = command
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .context("start supervised child")?;
        let pid = rustix::process::Pid::from_raw(child.id() as i32).context("invalid child PID")?;
        let mut stdout = child.stdout.take().context("missing child stdout")?;
        let mut stderr = child.stderr.take().context("missing child stderr")?;
        let setup = rustix::fs::fcntl_setfl(&stdout, rustix::fs::OFlags::NONBLOCK)
            .and_then(|()| rustix::fs::fcntl_setfl(&stderr, rustix::fs::OFlags::NONBLOCK));
        let readable = setup.is_ok();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let mut failure = setup.err().map(anyhow::Error::from);
        let mut status = None;
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        let mut term_at = None;
        let mut killed = false;
        loop {
            if let Err(error) = self.kick() {
                self.poisoned = true;
                failure.get_or_insert(error);
            }
            if readable {
                for (pipe, data, cap, eof) in [
                    (
                        &mut stdout as &mut dyn Read,
                        &mut output,
                        limit,
                        &mut stdout_eof,
                    ),
                    (
                        &mut stderr as &mut dyn Read,
                        &mut errors,
                        16384,
                        &mut stderr_eof,
                    ),
                ] {
                    if !*eof {
                        match drain(pipe, data, cap) {
                            Ok(done) => *eof = done,
                            Err(error) => {
                                failure.get_or_insert(error);
                            }
                        }
                    }
                }
            }
            if status.is_none() {
                match child.try_wait() {
                    Ok(value) => status = value,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        failure.get_or_insert(error.into());
                    }
                }
            }
            let now = self.now_ms();
            if status.is_some() && stdout_eof && stderr_eof {
                self.reap_adopted(deadline)?;
                if let Some(error) = failure {
                    return Err(error);
                }
                ensure!(
                    status.is_some_and(|s| s.success()),
                    "supervised child failed: {status:?}: {}",
                    String::from_utf8_lossy(&errors)
                );
                return Ok(output);
            }
            if failure.is_some() || now >= deadline.saturating_sub(1000) {
                failure.get_or_insert_with(|| anyhow::anyhow!("supervised operation timed out"));
                let start = *term_at.get_or_insert(now);
                if !killed {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::TERM);
                }
                if now >= start.saturating_add(250) || now >= deadline.saturating_sub(500) {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                    killed = true;
                }
                if status.is_some() && (killed || !readable) {
                    self.reap_adopted(deadline)?;
                    return Err(failure.context("missing supervision failure")?);
                }
            }
            if now >= deadline {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                self.poisoned = true;
                bail!("unreaped supervised child at absolute deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn reap_adopted(&mut self, deadline: u64) -> Result<()> {
        if std::process::id() != 1 {
            return Ok(());
        }
        for _ in 0..4096 {
            ensure!(self.now_ms() < deadline, "reap deadline exhausted");
            self.kick()?;
            match rustix::process::waitpid(None, rustix::process::WaitOptions::NOHANG) {
                Ok(Some(_)) => {}
                Ok(None) | Err(rustix::io::Errno::CHILD) => return Ok(()),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => {
                    self.poisoned = true;
                    return Err(error.into());
                }
            }
        }
        self.poisoned = true;
        bail!("excessive adopted children")
    }
    pub fn failure(mut self, error: &anyhow::Error) -> ! {
        let _ = diagnostic(&format!(
            "MOS_SHUTDOWN stage=storage-not-released error={:?}",
            format!("{error:#}")
        ));
        if let Some(budget) = self.budget {
            let until = self.now_ms().saturating_add(2000).min(budget.deadline_ms);
            while self.now_ms() < until {
                if self.kick().is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = diagnostic(&format!(
            "MOS_SHUTDOWN stage=failed watchdogArmed={}",
            self.armed
        ));
        // PID 1 must stay alive without feeding. Only a verified armed watchdog
        // can provide the emergency reset; this is never graceful completion.
        loop {
            thread::park();
        }
    }
    pub fn terminal(&mut self, action: Action, _released: Released) -> Result<()> {
        ensure!(!self.poisoned, "unreaped work prevents terminal action");
        let budget = self.budget.context("missing lifecycle deadline")?;
        ensure!(
            self.now_ms() < budget.cleanup_deadline_ms,
            "terminal deadline exhausted"
        );
        diagnostic(&format!(
            "MOS_SHUTDOWN stage=action-requested action={}",
            action.as_str()
        ))?;
        let command = match action {
            Action::Reboot => rustix::system::RebootCommand::Restart,
            Action::Poweroff => rustix::system::RebootCommand::PowerOff,
            Action::Halt => rustix::system::RebootCommand::Halt,
        };
        let result = rustix::system::reboot(command);
        bail!("terminal action returned: {result:?}")
    }
}

fn drain(pipe: &mut dyn Read, data: &mut Vec<u8>, limit: usize) -> Result<bool> {
    let mut buffer = [0; 4096];
    // Bound work per tick even if a writer continuously fills the pipe.
    for _ in 0..16 {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                ensure!(data.len() + n <= limit, "excessive supervised output");
                data.extend_from_slice(&buffer[..n]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

fn read_text(path: impl AsRef<Path>, limit: u64) -> Result<String> {
    let mut value = String::new();
    File::open(path.as_ref())
        .with_context(|| format!("open {}", path.as_ref().display()))?
        .take(limit + 1)
        .read_to_string(&mut value)?;
    ensure!(value.len() as u64 <= limit, "excessive kernel state");
    Ok(value)
}

pub struct SystemIo<'a> {
    pub supervisor: &'a mut Supervisor,
    pub executable: &'static str,
}
impl SystemIo<'_> {
    fn worker(&mut self, op: &Operation, deadline: u64) -> Result<Vec<u8>> {
        let argument = serde_json::to_string(op)?;
        ensure!(argument.len() <= 16384, "excessive worker request");
        let mut command = Command::new(self.executable);
        command.args(["--lifecycle-worker", &argument]);
        self.supervisor.run(command, deadline, 1024 * 1024)
    }
}
impl LifecycleIo for SystemIo<'_> {
    fn now_ms(&self) -> u64 {
        self.supervisor.now_ms()
    }
    fn scan(&mut self, deadline: u64) -> Result<Snapshot> {
        Ok(serde_json::from_slice(
            &self.worker(&Operation::Scan, deadline)?,
        )?)
    }
    fn execute(&mut self, op: &Operation, deadline: u64) -> Result<()> {
        self.worker(op, deadline)?;
        Ok(())
    }
    fn event(&mut self, stage: &str, detail: &str) {
        if diagnostic(&format!("MOS_SHUTDOWN stage={stage} detail={detail:?}")).is_err() {
            self.supervisor.poisoned = true;
        }
    }
    fn terminal(&mut self, action: Action, released: Released) -> Result<()> {
        self.supervisor.terminal(action, released)
    }
}

fn api(name: &str, magic: i64) -> Result<PathBuf> {
    for base in [Path::new("/"), Path::new("/newroot")] {
        let path = base.join(name);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir())
            && rustix::fs::statfs(&path).is_ok_and(|s| s.f_type == magic)
        {
            return Ok(path);
        }
    }
    bail!("required {name} filesystem unavailable")
}
fn procfs() -> Result<PathBuf> {
    api("proc", 0x9fa0)
}
fn sysfs() -> Result<PathBuf> {
    api("sys", 0x62656572)
}
fn devfs() -> Result<PathBuf> {
    api("dev", 0x01021994)
}

fn dm_control() -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(
            (rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOFOLLOW)
                .bits() as i32,
        )
        .open(devfs()?.join("mapper/control"))?;
    let expected =
        Device::parse(read_text(sysfs()?.join("class/misc/device-mapper/dev"), 64)?.trim())?;
    ensure!(
        file.metadata()?.file_type().is_char_device()
            && Device::from_raw(file.metadata()?.rdev()) == expected,
        "device-mapper control descriptor identity mismatch"
    );
    Ok(file)
}

fn checked_verity_table(
    status: &lifecycle_sys::DmStatus,
    targets: &[lifecycle_sys::DmTarget],
    device: Device,
    name: &str,
    uuid: &str,
    slaves: &BTreeSet<Device>,
    sectors: u64,
) -> Result<String> {
    ensure!(
        Device::from_raw(status.device) == device
            && status.name == name
            && status.uuid == uuid
            && !uuid.is_empty()
            && status.targets == 1
            && targets.len() == 1,
        "DM device/name/UUID/target identity mismatch"
    );
    let target = &targets[0];
    ensure!(
        target.kind == "verity" && target.sector == 0 && target.length == sectors && sectors > 0,
        "DM table does not completely cover the verified device"
    );
    let parameters: Vec<_> = target.parameters.split_whitespace().collect();
    ensure!(
        parameters.len() >= 10 && parameters[0] == "1",
        "unsupported MOS verity table"
    );
    let providers = BTreeSet::from([Device::parse(parameters[1])?, Device::parse(parameters[2])?]);
    ensure!(
        providers == *slaves && providers.len() == 1 && providers.iter().all(|d| d.major == 7),
        "DM table providers differ from live MOS loop dependencies"
    );
    Ok(format!(
        "{} {} {} {}",
        target.sector,
        target.length,
        target.kind,
        parameters.join(" ")
    ))
}

fn read_mos_table(
    control: &File,
    device: Device,
    path: &Path,
    name: &str,
    uuid: &str,
    slaves: &BTreeSet<Device>,
) -> Result<(String, lifecycle_sys::DmStatus)> {
    let status =
        lifecycle_sys::dm_status(control, rustix::fs::makedev(device.major, device.minor))?;
    let targets = lifecycle_sys::dm_table(control, &status)?;
    let sectors = read_text(path.join("size"), 64)?.trim().parse::<u64>()?;
    let table = checked_verity_table(&status, &targets, device, name, uuid, slaves, sectors)?;
    ensure!(
        lifecycle_sys::dm_status(control, status.device)? == status,
        "DM identity changed while reading its table"
    );
    Ok((table, status))
}
fn list(path: &Path, limit: usize) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        ensure!(paths.len() < limit, "excessive kernel directory");
        paths.push(entry?.path());
    }
    paths.sort();
    Ok(paths)
}
fn links(path: &Path) -> Result<BTreeSet<Device>> {
    list(path, 4096)?
        .into_iter()
        .map(|p| Device::parse(read_text(p.join("dev"), 64)?.trim()))
        .collect()
}
fn slave_links(path: &Path) -> Result<BTreeSet<Device>> {
    if path.join("partition").exists() {
        ensure!(
            read_text(path.join("partition"), 64)?
                .trim()
                .parse::<u32>()?
                > 0,
            "invalid partition identity"
        );
        // Linux creates holders for partitions, but only whole disks have a
        // slaves directory. Absence elsewhere remains an observation error.
        return Ok(BTreeSet::new());
    }
    links(&path.join("slaves"))
}
fn disk_generation(path: &Path) -> Result<u64> {
    let node = fs::canonicalize(path)?;
    let disk = if node.join("partition").exists() {
        node.parent().context("partition has no disk")?
    } else {
        node.as_path()
    };
    let generation = read_text(disk.join("diskseq"), 64)?.trim().parse()?;
    ensure!(generation > 0, "invalid block generation");
    Ok(generation)
}
fn inspect_loop_at(path: &Path, device: Device, generation: u64) -> Result<Option<LoopIdentity>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOFOLLOW)
                .bits() as i32,
        )
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.file_type().is_block_device() && Device::from_raw(metadata.rdev()) == device,
        "loop descriptor identity mismatch"
    );
    let state = lifecycle_sys::loop_status(&file)?;
    // Drop the inspection FD before returning an observation of association state.
    drop(file);
    state
        .map(|s| {
            ensure!(
                s.number == device.minor && device.major == 7,
                "loop number mismatch"
            );
            Ok(LoopIdentity {
                device,
                generation,
                backing: Device::from_raw(s.backing_device),
                inode: s.inode,
                offset: s.offset,
                size_limit: s.size_limit,
                flags: s.flags,
            })
        })
        .transpose()
}
fn process_ids(proc: &Path) -> Result<Vec<u32>> {
    let own = std::process::id();
    let mut users = Vec::new();
    for path in list(proc, 16384)? {
        let Some(pid) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == 1 || pid == own {
            continue;
        }
        let stat = match read_text(path.join("stat"), 8192) {
            Ok(stat) => stat,
            Err(_) if !path.exists() => continue,
            Err(error) => return Err(error),
        };
        let (_, rest) = stat.rsplit_once(") ").context("malformed process stat")?;
        let fields: Vec<_> = rest.split_whitespace().collect();
        ensure!(fields.len() > 19, "short process stat");
        let flags = fields[6].parse::<u64>()?;
        if flags & 0x0020_0000 == 0 && fields[0] != "Z" {
            users.push(pid);
        }
    }
    ensure!(users.len() <= 4096, "excessive userspace holders");
    Ok(users)
}
fn snapshot() -> Result<Snapshot> {
    let proc = procfs()?;
    let sys = sysfs()?;
    let dev = devfs()?;
    let mounts = parse_mountinfo(&read_text(proc.join("self/mountinfo"), 1024 * 1024)?)?;
    let mut blocks = Vec::new();
    for path in list(&sys.join("class/block"), 4096)? {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("invalid block name")?
            .to_owned();
        let device = Device::parse(read_text(path.join("dev"), 64)?.trim())?;
        let mut generation = disk_generation(&path)?;
        let mut association = None;
        if device.major == 7 {
            let mut stable = false;
            for _ in 0..3 {
                let observed = inspect_loop_at(&dev.join(&name), device, generation)?;
                let after = disk_generation(&path)?;
                if generation == after {
                    association = observed;
                    stable = true;
                    break;
                }
                generation = after;
            }
            ensure!(stable, "loop changed during descriptor inspection");
        }
        let holders = links(&path.join("holders"))?;
        let slaves = slave_links(&path)?;
        let mapping = if path.join("dm").is_dir() {
            let dm_name = read_text(path.join("dm/name"), 256)?.trim().to_owned();
            let uuid = read_text(path.join("dm/uuid"), 256)?.trim().to_owned();
            // Only MOS tables need content identity. Foreign holders remain
            // visible but are never passed to a removal command.
            let table = if ["mos-root", "mos-support"].contains(&dm_name.as_str()) {
                let control = dm_control()?;
                let (table, _) = read_mos_table(&control, device, &path, &dm_name, &uuid, &slaves)?;
                drop(control);
                ensure!(
                    disk_generation(&path)? == generation
                        && read_text(path.join("dm/name"), 256)?.trim() == dm_name
                        && read_text(path.join("dm/uuid"), 256)?.trim() == uuid,
                    "DM generation or identity changed during inspection"
                );
                table
            } else {
                String::new()
            };
            Some(Mapping {
                device,
                generation,
                name: dm_name,
                uuid,
                table,
            })
        } else {
            None
        };
        blocks.push(Block {
            device,
            generation,
            name,
            holders,
            slaves,
            association,
            mapping,
        });
    }
    let swap_text = read_text(proc.join("swaps"), 65536)?;
    ensure!(
        swap_text
            .lines()
            .next()
            .is_some_and(|s| s.starts_with("Filename")),
        "invalid swap state"
    );
    let swaps = swap_text.lines().skip(1).map(str::to_owned).collect();
    let memory = read_text(proc.join("meminfo"), 16384)?;
    let mut dirty = 0_u64;
    let mut fields = 0;
    for line in memory.lines() {
        if ["Dirty:", "Writeback:", "NFS_Unstable:"]
            .iter()
            .any(|p| line.starts_with(p))
        {
            let value = line
                .split_whitespace()
                .nth(1)
                .context("invalid dirty state")?
                .parse::<u64>()?;
            dirty = dirty.checked_add(value).context("dirty count overflow")?;
            fields += 1;
        }
    }
    ensure!(fields == 3, "missing dirty state");
    Ok(Snapshot {
        mounts,
        blocks,
        processes: process_ids(&proc)?,
        swaps,
        dirty_kib: dirty,
    })
}
fn current_mount(expected: &Mount) -> Result<()> {
    let mounts = parse_mountinfo(&read_text(procfs()?.join("self/mountinfo"), 1024 * 1024)?)?;
    ensure!(
        mounts.iter().any(|m| m == expected),
        "mount identity changed before operation"
    );
    let stat = rustix::fs::statx(
        rustix::fs::CWD,
        &expected.path,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW | rustix::fs::AtFlags::NO_AUTOMOUNT,
        rustix::fs::StatxFlags::MNT_ID,
    )?;
    ensure!(
        stat.stx_mask & rustix::fs::StatxFlags::MNT_ID.bits() != 0
            && stat.stx_mnt_id == expected.id,
        "mount is hidden or was replaced"
    );
    Ok(())
}
fn quiesce() -> Result<()> {
    let proc = procfs()?;
    let mut handles = Vec::new();
    for pid in process_ids(&proc)? {
        let id = rustix::process::Pid::from_raw(pid as i32).context("invalid process id")?;
        let before = read_text(proc.join(format!("{pid}/stat")), 8192)?;
        let handle = match rustix::process::pidfd_open(id, rustix::process::PidfdFlags::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::SRCH) => continue,
            Err(error) => return Err(error.into()),
        };
        let after = read_text(proc.join(format!("{pid}/stat")), 8192)?;
        let birth = |s: &str| {
            s.rsplit_once(") ")
                .and_then(|(_, tail)| tail.split_whitespace().nth(19))
                .map(str::to_owned)
        };
        ensure!(
            birth(&before).is_some() && birth(&before) == birth(&after),
            "process reused during quiesce"
        );
        let _ = rustix::process::pidfd_send_signal(&handle, rustix::process::Signal::TERM);
        handles.push(handle);
    }
    if !handles.is_empty() {
        thread::sleep(Duration::from_millis(250));
    }
    for handle in handles {
        match rustix::process::pidfd_send_signal(&handle, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Fixed child protocol, reachable only as a direct child of lifecycle PID 1.
/// It carries typed operations and no command, environment or path-root override.
pub fn worker(args: &[String]) -> Option<Result<()>> {
    if args.first().map(String::as_str) != Some("--lifecycle-worker") {
        return None;
    }
    Some((|| {
        ensure!(
            rustix::process::getppid() == rustix::process::Pid::from_raw(1),
            "lifecycle worker requires PID1 parent"
        );
        ensure!(
            args.len() == 2 && args[1].len() <= 16384,
            "invalid worker input"
        );
        let op: Operation = serde_json::from_str(&args[1])?;
        perform(&op)
    })())
}

fn perform(op: &Operation) -> Result<()> {
    match op {
        Operation::Scan => {
            println!("{}", serde_json::to_string(&snapshot()?)?);
        }
        Operation::Private => {
            // Privatize before restoring moved APIs: moving beneath a shared
            // parent is rejected by mount(2). Inherited stdin also works while
            // /dev/null temporarily lives below /newroot/dev.
            rustix::mount::mount_change(
                "/",
                rustix::mount::MountPropagationFlags::PRIVATE
                    | rustix::mount::MountPropagationFlags::REC,
            )?;
            // Startup can fail between API mount moves. Restore them before
            // traversing/removing the old root; validate the real fs types.
            for (name, magic) in [("dev", 0x01021994), ("proc", 0x9fa0), ("sys", 0x62656572)] {
                let source = api(name, magic)?;
                let target = Path::new("/").join(name);
                if source != target {
                    rustix::mount::mount_move(&source, &target)?;
                }
            }
        }
        Operation::Quiesce => quiesce()?,
        Operation::Sync => rustix::fs::sync(),
        Operation::SyncMount(mount) => {
            current_mount(mount)?;
            let fd = rustix::fs::open(
                &mount.path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )?;
            let identity = rustix::fs::statx(
                &fd,
                "",
                rustix::fs::AtFlags::EMPTY_PATH,
                rustix::fs::StatxFlags::MNT_ID,
            )?;
            ensure!(
                identity.stx_mask & rustix::fs::StatxFlags::MNT_ID.bits() != 0
                    && identity.stx_mnt_id == mount.id,
                "sync mount descriptor changed"
            );
            // sync() cannot report writeback errors. Per-filesystem syncfs
            // reports EIO/ENOSPC before ordinary unmount releases this mount.
            rustix::fs::syncfs(&fd)?;
            drop(fd);
        }
        Operation::Unmount(mount) => {
            current_mount(mount)?;
            rustix::mount::unmount(&mount.path, rustix::mount::UnmountFlags::empty())?;
        }
        Operation::MoveBacking(mount) => {
            current_mount(mount)?;
            let target = format!("/backing/{}", mount.id);
            let root = rustix::fs::open(
                "/",
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
                rustix::fs::Mode::empty(),
            )?;
            match rustix::fs::mkdirat(&root, "backing", rustix::fs::Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
            let directory = rustix::fs::openat2(
                &root,
                "backing",
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
                rustix::fs::Mode::empty(),
                rustix::fs::ResolveFlags::BENEATH | rustix::fs::ResolveFlags::NO_SYMLINKS,
            )?;
            match rustix::fs::mkdirat(
                &directory,
                mount.id.to_string(),
                rustix::fs::Mode::from_raw_mode(0o700),
            ) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
            let target_fd = rustix::fs::openat2(
                &directory,
                mount.id.to_string(),
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
                rustix::fs::Mode::empty(),
                rustix::fs::ResolveFlags::BENEATH
                    | rustix::fs::ResolveFlags::NO_SYMLINKS
                    | rustix::fs::ResolveFlags::NO_XDEV,
            )?;
            ensure!(
                rustix::fs::fstat(&target_fd)?.st_dev == rustix::fs::fstat(&root)?.st_dev
                    && fs::read_dir(&target)?.next().is_none(),
                "backing destination is not empty exitrd storage"
            );
            drop(target_fd);
            drop(directory);
            drop(root);
            rustix::mount::mount_move(&mount.path, &target)?;
            let moved =
                parse_mountinfo(&read_text(procfs()?.join("self/mountinfo"), 1024 * 1024)?)?;
            ensure!(
                moved.iter().any(|m| m.id == mount.id
                    && m.device == mount.device
                    && m.root == mount.root
                    && m.path == target),
                "moved mount identity mismatch"
            );
        }
        Operation::RemoveMapping(expected) => {
            ensure!(
                ["mos-root", "mos-support"].contains(&expected.name.as_str()),
                "foreign mapping removal refused"
            );
            let state = snapshot()?;
            let block = state
                .blocks
                .iter()
                .find(|b| b.device == expected.device)
                .context("mapping disappeared before removal")?;
            ensure!(
                block.mapping.as_ref() == Some(expected)
                    && block.holders.is_empty()
                    && !state.mounts.iter().any(|m| m.device == expected.device),
                "mapping identity or users changed"
            );
            let path = sysfs()?.join("class/block").join(&block.name);
            let control = dm_control()?;
            let (table, status) = read_mos_table(
                &control,
                expected.device,
                &path,
                &expected.name,
                &expected.uuid,
                &block.slaves,
            )?;
            ensure!(
                table == expected.table && disk_generation(&path)? == expected.generation,
                "DM table/generation changed before removal"
            );
            lifecycle_sys::dm_remove(&control, &status)?;
            drop(control);
            let after = snapshot()?;
            ensure!(
                !after.blocks.iter().any(|b| b.device == expected.device
                    || b.mapping
                        .as_ref()
                        .is_some_and(|m| m.name == expected.name || m.uuid == expected.uuid)),
                "DM removal did not release the observed device"
            );
        }
        Operation::DetachLoop(expected) => {
            let state = snapshot()?;
            let block = state
                .blocks
                .iter()
                .find(|b| b.device == expected.device)
                .context("loop missing before detach")?;
            ensure!(
                block.holders.is_empty()
                    && !state.mounts.iter().any(|m| m.device == expected.device),
                "loop still has users"
            );
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(
                    (rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NONBLOCK
                        | rustix::fs::OFlags::NOFOLLOW)
                        .bits() as i32,
                )
                .open(devfs()?.join(&block.name))?;
            ensure!(
                file.metadata()?.file_type().is_block_device()
                    && Device::from_raw(file.metadata()?.rdev()) == expected.device,
                "loop descriptor changed"
            );
            let current =
                lifecycle_sys::loop_status(&file)?.context("loop association disappeared")?;
            ensure!(
                current.number == expected.device.minor
                    && Device::from_raw(current.backing_device) == expected.backing
                    && current.inode == expected.inode
                    && current.offset == expected.offset
                    && current.size_limit == expected.size_limit
                    && current.flags & !4 == expected.flags & !4,
                "loop association changed before detach"
            );
            ensure!(
                disk_generation(&sysfs()?.join("class/block").join(&block.name))?
                    == expected.generation,
                "loop generation changed before detach"
            );
            lifecycle_sys::clear_loop(&file)?;
            drop(file);
            // The parent obtains a new snapshot after this worker exits. This
            // call is never itself reported as association/holder release.
        }
        Operation::Retire {
            id,
            backend,
            board,
            system,
            system_device,
            boot_device,
        } => {
            ensure!(
                fs::read_link(procfs()?.join("1/exe"))? == Path::new("/init"),
                "record retirement requires startup PID1"
            );
            crate::deployments::valid_id(id)?;
            ensure!(
                super::BootKind::for_board(board)? == *backend,
                "retirement board mismatch"
            );
            let name = Path::new(system_device)
                .file_name()
                .context("SYSTEM name")?;
            let node = fs::canonicalize(sysfs()?.join("class/block").join(name))?;
            let expected = crate::deployments::boot_partition(&node, *backend, board)?;
            ensure!(
                expected == Path::new(boot_device),
                "retirement boot device mismatch"
            );
            let system_dev = Device::from_raw(fs::metadata(system_device)?.rdev());
            let state = snapshot()?;
            ensure!(
                state.mounts.iter().any(|m| m.path == *system
                    && m.device == system_dev
                    && m.kind == "ext4"
                    && m.root == "/"),
                "retirement SYSTEM mount identity mismatch"
            );
            let boot = match backend {
                super::BootKind::Uefi => {
                    fs::create_dir_all("/boot-state")?;
                    rustix::mount::mount(
                        boot_device,
                        "/boot-state",
                        "vfat",
                        rustix::mount::MountFlags::NODEV
                            | rustix::mount::MountFlags::NOSUID
                            | rustix::mount::MountFlags::NOEXEC,
                        None,
                    )?;
                    crate::deployments::BootBackend::Uefi {
                        esp: "/boot-state".into(),
                    }
                }
                super::BootKind::UbootFit => crate::deployments::BootBackend::Fit {
                    firmware: expected,
                    layout: crate::fit_env::FitLayout::for_board(board)?,
                },
            };
            let store = crate::deployments::DeploymentStore::new(
                system.into(),
                boot,
                "/unused-meta".into(),
            );
            let result = store.retire_failed_confirmed(id);
            if *backend == super::BootKind::Uefi {
                rustix::mount::mount_remount(
                    "/boot-state",
                    rustix::mount::MountFlags::RDONLY
                        | rustix::mount::MountFlags::NODEV
                        | rustix::mount::MountFlags::NOSUID
                        | rustix::mount::MountFlags::NOEXEC,
                    "",
                )?;
            }
            println!("{}", result?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_verity_table_preserves_live_device_provider_and_complete_coverage() {
        let device = Device {
            major: 253,
            minor: 0,
        };
        let provider = Device { major: 7, minor: 0 };
        let status = lifecycle_sys::DmStatus {
            device: rustix::fs::makedev(253, 0),
            name: "mos-root".into(),
            uuid: "CRYPT-VERITY-owned".into(),
            targets: 1,
            open_count: 0,
            event: 7,
        };
        let target = lifecycle_sys::DmTarget {
            sector: 0,
            length: 80,
            kind: "verity".into(),
            parameters: "1 7:0 7:0 4096 4096 10 10 sha256 abcd - 1 restart_on_corruption".into(),
        };
        let slaves = BTreeSet::from([provider]);
        assert_eq!(
            checked_verity_table(
                &status,
                std::slice::from_ref(&target),
                device,
                "mos-root",
                "CRYPT-VERITY-owned",
                &slaves,
                80
            )
            .unwrap(),
            format!("0 80 verity {}", target.parameters)
        );
        for case in 0..11 {
            let mut status = status.clone();
            let mut target = target.clone();
            let mut slaves = slaves.clone();
            match case {
                0 => status.device = rustix::fs::makedev(253, 1),
                1 => status.name = "mos-support".into(),
                2 => status.uuid = "reused".into(),
                3 => status.targets = 2,
                4 => target.sector = 1,
                5 => target.length = 79,
                6 => target.kind = "linear".into(),
                7 => target.parameters = "1 7:0".into(),
                8 => target.parameters = target.parameters.replacen("1 ", "0 ", 1),
                9 => target.parameters = target.parameters.replace("7:0", "7:1"),
                _ => {
                    slaves.insert(Device { major: 7, minor: 1 });
                }
            }
            assert!(
                checked_verity_table(
                    &status,
                    &[target],
                    device,
                    "mos-root",
                    "CRYPT-VERITY-owned",
                    &slaves,
                    80
                )
                .is_err(),
                "case={case}"
            );
        }
    }

    #[test]
    fn diagnostic_output_never_waits_for_a_full_pipe() {
        use std::io::Write;
        let (mut writer, _reader) = std::os::unix::net::UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        while writer.write(&[0_u8; 4096]).is_ok() {}
        let started = Instant::now();
        assert!(write_diagnostic(&writer, "bounded failure").is_err());
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn partition_sysfs_has_holders_but_no_slaves_directory() {
        let root = tempfile::tempdir().unwrap();
        assert!(slave_links(root.path()).is_err());
        fs::write(root.path().join("partition"), "2\n").unwrap();
        assert!(slave_links(root.path()).unwrap().is_empty());
        fs::write(root.path().join("partition"), "0\n").unwrap();
        assert!(slave_links(root.path()).is_err());
    }

    #[test]
    fn watchdog_errors_and_short_metadata_never_create_a_fresh_budget() {
        let mut supervisor = Supervisor::new();
        assert!(supervisor.begin_shutdown().is_err());
        supervisor.watchdog = Some(tempfile::tempfile().unwrap());
        supervisor.armed = true;
        assert!(supervisor.begin_shutdown().is_err());
        supervisor.last_kick = 0;
        supervisor.started = Instant::now() - Duration::from_secs(2);
        assert!(supervisor.kick().is_err());
        supervisor.budget = Some(Budget::new(0, 60).unwrap());
        assert!(supervisor.limit_shutdown(Some(2999)).is_err());
        let first = supervisor.limit_shutdown(Some(5000)).unwrap();
        let second = supervisor.limit_shutdown(Some(60000)).unwrap();
        assert_eq!(first.deadline_ms, second.deadline_ms);
        assert_eq!(first.cleanup_deadline_ms, second.cleanup_deadline_ms);
        supervisor.poisoned = true;
        assert!(
            supervisor
                .terminal(Action::Reboot, Released { _private: () })
                .is_err()
        );
    }

    #[test]
    fn supervisor_drains_large_output_without_blocking_wait() {
        let mut supervisor = Supervisor::new();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "head -c 200000 /dev/zero"]);
        assert_eq!(supervisor.run(command, 5000, 200000).unwrap().len(), 200000);
    }

    #[test]
    fn exited_parent_with_live_output_writer_is_not_completion() {
        let mut supervisor = Supervisor::new();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 10 & exit 0"]);
        assert!(supervisor.run(command, 1500, 16384).is_err());
        assert!(supervisor.now_ms() < 2000);
    }

    #[test]
    fn supervisor_bounds_output_refusal_and_uncooperative_children() {
        for script in [
            "head -c 200000 /dev/zero",
            "trap '' TERM; while :; do :; done",
        ] {
            let mut supervisor = Supervisor::new();
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script]);
            assert!(supervisor.run(command, 1500, 16384).is_err());
            assert!(supervisor.now_ms() < 2000);
        }
    }
}

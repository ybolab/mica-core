//! The authenticated initramfs PID 1. No network or shell policy is accepted.

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use mica_deploy::{
    boot::{
        BootKind, copy_exitrd, exitrd_tmpfs_bytes, fit_selected, persistent_machine_id,
        selected_entry,
        shutdown::{self, Action, Device, LifecycleIo, Operation, Ownership, Supervisor, SystemIo},
        startup::{
            self,
            native::{self, Operation as Startup},
        },
        utf16_variable,
    },
    components::{BootIdentity, VerityImage, component_id, verify_deployment},
    deployments::boot_partition,
};
use serde::Deserialize;
use std::{
    fs::{self, File},
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
    thread,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Config {
    identity: BootIdentity,
    public_keys: Vec<String>,
    system_part_uuid: String,
    data_part_uuid: String,
}

#[derive(Debug)]
struct SharedSystemFailure;
impl std::fmt::Display for SharedSystemFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("shared SYSTEM unavailable; full-image reflash recovery required")
    }
}
impl std::error::Error for SharedSystemFailure {}

struct BootAttempt {
    id: String,
    backend: BootKind,
    system_device: String,
    board: String,
}

struct BootControl {
    supervisor: Supervisor,
    storage: Ownership,
}

impl BootControl {
    fn observe(&mut self) -> Result<shutdown::Snapshot> {
        self.supervisor.observe("/sbin/mica-shutdown")
    }

    fn backing(&mut self, path: &str) -> Result<()> {
        let metadata = fs::metadata(path)?;
        ensure!(
            metadata.file_type().is_block_device(),
            "backing source is not a block device"
        );
        let device = Device::from_raw(metadata.rdev());
        let state = self.observe()?;
        let generation = state
            .blocks
            .iter()
            .find(|b| b.device == device)
            .context("backing block missing from live graph")?
            .generation;
        if let Some((_, known)) = self
            .storage
            .backing_generations
            .iter()
            .find(|(id, _)| *id == device)
        {
            ensure!(
                *known == generation,
                "backing block was reused during startup"
            );
        } else {
            self.storage.backing_generations.push((device, generation));
        }
        self.storage.backings.insert(device);
        Ok(())
    }
}

impl BootAttempt {
    fn retire_failed_confirmed(&self, control: &mut BootControl) -> Result<()> {
        let system = fs::canonicalize(
            Path::new("/sys/class/block").join(
                Path::new(&self.system_device)
                    .file_name()
                    .context("missing SYSTEM device")?,
            ),
        )?;
        let device = boot_partition(&system, self.backend, &self.board)?;
        let boot_device = device.to_str().context("invalid boot device")?.to_owned();
        control.backing(&boot_device)?;
        let state = control.observe()?;
        let expected = Device::from_raw(fs::metadata(&self.system_device)?.rdev());
        let system_root = state
            .mounts
            .iter()
            .find(|m| m.device == expected && m.kind == "ext4" && m.root == "/")
            .context("verified SYSTEM mount no longer available for retirement")?
            .path
            .clone();
        let deadline = control
            .supervisor
            .begin_shutdown()?
            .operation_deadline(control.supervisor.now_ms())?;
        SystemIo {
            supervisor: &mut control.supervisor,
            executable: "/sbin/mica-shutdown",
        }
        .execute(
            &Operation::Retire {
                id: self.id.clone(),
                backend: self.backend,
                board: self.board.clone(),
                system: system_root,
                system_device: self.system_device.clone(),
                boot_device,
            },
            deadline,
        )
    }
}

fn bounded_file(path: &str, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).with_context(|| format!("stat {path}"))?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "invalid bounded file {path}"
    );
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("open {path}"))?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "file grew beyond limit: {path}"
    );
    Ok(bytes)
}

fn run(control: &mut BootControl, operation: Startup) -> Result<String> {
    control.supervisor.run_startup(native::command(operation)?)
}

fn mount(
    control: &mut BootControl,
    source: &str,
    target: &str,
    kind: &str,
    options: &str,
) -> Result<()> {
    fs::create_dir_all(target)?;
    if kind == "ext4" {
        control.backing(source)?;
    }
    let mounted = run(
        control,
        Startup::Mount {
            source: source.into(),
            target: target.into(),
            kind: kind.into(),
            options: options.into(),
        },
    );
    if target == "/run/initramfs" {
        let live = control.observe()?;
        if let Some(mount) = live
            .mounts
            .into_iter()
            .find(|m| m.path == target && m.kind == "tmpfs" && m.root == "/")
        {
            control.storage.mounts.push(mount);
        }
    }
    mounted.with_context(|| format!("mount {source} at {target}"))?;
    Ok(())
}

fn verified_mount(
    control: &mut BootControl,
    path: &str,
    signature: &str,
    name: &str,
    target: &str,
    image: &VerityImage,
) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {path}"))?;
    ensure!(
        meta.is_file() && meta.len() == image.image.bytes,
        "image type or length mismatch: {path}"
    );
    image.signature.verify(&bounded_file(signature, 65536)?)?;
    let loop_device = run(control, Startup::Loop { image: path.into() })?;
    let live = control.observe()?;
    let loop_id = Device::from_raw(fs::metadata(&loop_device)?.rdev());
    let association = live
        .blocks
        .iter()
        .find(|b| b.device == loop_id)
        .and_then(|b| b.association.as_ref())
        .context("new loop association disappeared")?
        .clone();
    ensure!(
        association.backing == Device::from_raw(meta.dev())
            && association.inode == meta.ino()
            && association.flags & 1 == 1
            && association.offset == 0
            && association.size_limit == 0,
        "native loop identity disagrees with verified startup association"
    );
    control.storage.loops.push(association);
    ensure!(
        !live
            .blocks
            .iter()
            .any(|b| b.mapping.as_ref().is_some_and(|m| m.name == name)),
        "MICA mapping already exists before creation"
    );
    let creation = run(
        control,
        Startup::Verity {
            device: loop_device.clone(),
            name: name.into(),
            signature: signature.into(),
            image: serde_json::from_value(serde_json::to_value(image)?)?,
        },
    );
    // A worker can create the mapping and then fail. Adopt only the live
    // expected verity table and the already verified owned loop, never a name.
    let live = control.observe()?;
    if let Some(block) = live
        .blocks
        .iter()
        .find(|b| b.mapping.as_ref().is_some_and(|m| m.name == name))
    {
        let mapping = block.mapping.as_ref().context("mapping disappeared")?;
        ensure!(
            block.slaves == std::collections::BTreeSet::from([loop_id])
                && mapping.table.split_whitespace().nth(2) == Some("verity")
                && mapping
                    .table
                    .split_whitespace()
                    .any(|s| s == image.root_hash)
                && !mapping.uuid.is_empty(),
            "created mapping ownership cannot be established"
        );
        control.storage.mappings.push(mapping.clone());
    }
    creation?;
    let mapping = control
        .storage
        .mappings
        .last()
        .context("created mapping is absent")?;
    let expected = startup::verity::table(
        loop_id.major,
        loop_id.minor,
        &format!("cryptsetup:{name}"),
        image,
    )?;
    ensure!(
        mapping.table
            == format!(
                "{} {} {} {}",
                expected.sector, expected.length, expected.kind, expected.parameters
            ),
        "kernel mapping differs from signed verity table"
    );
    mount(
        control,
        &format!("/dev/mapper/{name}"),
        target,
        "squashfs",
        "ro,nodev",
    )
}

fn boot(control: &mut BootControl, attempt: &mut Option<BootAttempt>) -> Result<()> {
    let started = Instant::now();
    ensure!(std::process::id() == 1, "mica-init must run as PID 1");
    mount(control, "devtmpfs", "/dev", "devtmpfs", "nosuid,mode=0755")?;
    mount(control, "proc", "/proc", "proc", "nosuid,nodev,noexec")?;
    mount(control, "sysfs", "/sys", "sysfs", "nosuid,nodev,noexec")?;
    mount(
        control,
        "tmpfs",
        "/run",
        "tmpfs",
        "nosuid,nodev,mode=0755,size=32M",
    )?;
    ensure!(
        fs::read_to_string("/sys/module/dm_verity/parameters/require_signatures")?.trim() == "Y",
        "signature enforcement is disabled"
    );
    // Arming is synchronous and precedes access to deployment storage. The
    // signed cmdline fixes the timeout; NOWAYOUT keeps it armed across exec.
    control.supervisor.arm()?;
    eprintln!("mica-init: boot watchdog armed");
    eprintln!("mica-init: pseudo-filesystems and signature policy ready");
    let config: Config = serde_json::from_slice(&bounded_file("/etc/mica/boot.json", 4096)?)?;
    for uuid in [&config.system_part_uuid, &config.data_part_uuid] {
        ensure!(
            uuid.len() == 36 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
            "invalid storage UUID"
        );
    }
    ensure!(
        fs::read_to_string("/proc/sys/kernel/osrelease")?.trim() == config.identity.kernel_release,
        "running kernel release mismatch"
    );
    let keys = config
        .public_keys
        .iter()
        .map(|key| -> Result<[u8; 32]> {
            let raw = STANDARD.decode(key)?;
            ensure!(STANDARD.encode(&raw) == *key, "noncanonical metadata key");
            raw.try_into()
                .map_err(|_| anyhow::anyhow!("invalid metadata key length"))
        })
        .collect::<Result<Vec<_>>>()?;
    let backend = BootKind::for_board(&config.identity.board)?;
    let (selected, id, secure_boot) = match backend {
        BootKind::Uefi => {
            mount(
                control,
                "efivarfs",
                "/sys/firmware/efi/efivars",
                "efivarfs",
                "nosuid,nodev,noexec",
            )?;
            let selected = utf16_variable(&bounded_file(
                "/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f",
                512,
            )?)?;
            let id = selected_entry(&selected)?;
            let secure_boot = fs::read(
                "/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c",
            )?
            .get(4)
                == Some(&1);
            (selected, id, secure_boot)
        }
        BootKind::UbootFit => {
            let id = fit_selected(&bounded_file(
                "/sys/firmware/devicetree/base/chosen/mica,deployment-id",
                65,
            )?)?;
            (format!("fit:{id}"), id, false)
        }
    };
    eprintln!("mica-init: selected deployment {id}");
    control.storage.deployment = id.clone();
    let mut device = String::new();
    let discovery = Instant::now();
    while discovery.elapsed() < Duration::from_secs(15) {
        if let Ok(found) = run(
            control,
            Startup::Partition {
                uuid: config.system_part_uuid.clone(),
            },
        ) && !found.is_empty()
        {
            device = found;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !device.starts_with("/dev/") || device.contains(char::is_whitespace) {
        return Err(
            anyhow::anyhow!("SYSTEM partition not found uniquely").context(SharedSystemFailure)
        );
    }
    // ext4 replays a dirty journal before completing this read-only mount.
    mount(
        control,
        &device,
        "/system",
        "ext4",
        "ro,nodev,nosuid,noexec",
    )
    .context(SharedSystemFailure)?;
    *attempt = Some(BootAttempt {
        id: id.clone(),
        backend,
        system_device: device.clone(),
        board: config.identity.board.clone(),
    });
    eprintln!("mica-init: SYSTEM mounted read-only");
    let envelope = bounded_file(&format!("/system/deployments/{id}.json"), 24576)?;
    let deployment = verify_deployment(&envelope, &keys, &config.identity)?;
    let value = serde_json::to_value(&deployment)?;
    ensure!(
        component_id(&value)? == id,
        "selected deployment identity mismatch"
    );
    let paths = deployment.paths()?;
    verified_mount(
        control,
        &format!("/system/{}", paths.rootfs),
        &format!("/system/roots/{}/rootfs.roothash.p7s", deployment.rootfs.id),
        "mica-root",
        "/newroot",
        &deployment.rootfs.content,
    )?;
    verified_mount(
        control,
        &format!("/system/{}", paths.support),
        &format!(
            "/system/kernels/{}/support.roothash.p7s",
            deployment.kernel.id
        ),
        "mica-support",
        "/support",
        &deployment.kernel.support,
    )?;
    ensure!(
        fs::read_to_string("/support/kernel.release")?.trim() == config.identity.kernel_release,
        "support module release mismatch"
    );
    for (source, target) in [
        ("/support/modules", "/newroot/usr/lib/modules"),
        ("/support/firmware", "/newroot/usr/lib/firmware"),
    ] {
        ensure!(
            fs::symlink_metadata(target)?.is_dir() && fs::read_dir(target)?.next().is_none(),
            "invalid support mountpoint {target}"
        );
        run(
            control,
            Startup::Bind {
                source: source.into(),
                target: target.into(),
            },
        )?;
        run(
            control,
            Startup::Remount {
                target: target.into(),
                options: "remount,bind,ro,nodev,nosuid".into(),
            },
        )?;
    }
    (|| -> Result<()> {
        let data = run(
            control,
            Startup::Partition {
                uuid: config.data_part_uuid.clone(),
            },
        )?;
        ensure!(
            data.starts_with("/dev/") && !data.contains(char::is_whitespace),
            "DATA partition not found uniquely"
        );
        let system_node = fs::canonicalize(format!(
            "/sys/class/block/{}",
            Path::new(&device)
                .file_name()
                .context("SYSTEM device name")?
                .to_string_lossy()
        ))?;
        let data_node = fs::canonicalize(format!(
            "/sys/class/block/{}",
            Path::new(&data)
                .file_name()
                .context("DATA device name")?
                .to_string_lossy()
        ))?;
        ensure!(
            system_node.parent() == data_node.parent()
                && fs::read_to_string(system_node.join("partition"))?.trim() == "2"
                && fs::read_to_string(data_node.join("partition"))?.trim() == "3",
            "DATA is not on the authenticated system disk"
        );
        mount(
            control,
            &data,
            "/newroot/mnt/data",
            "ext4",
            "rw,noatime,prjquota",
        )?;
        persistent_machine_id(Path::new("/newroot/mnt/data/state"), || {
            Ok(fs::read_to_string("/proc/sys/kernel/random/uuid")?
                .trim()
                .replace('-', ""))
        })?;
        ensure!(
            fs::symlink_metadata("/newroot/etc/machine-id")?.is_file(),
            "machine identity mountpoint is not a file"
        );
        run(
            control,
            Startup::Bind {
                source: "/newroot/mnt/data/state/machine-id".into(),
                target: "/newroot/etc/machine-id".into(),
            },
        )?;
        run(
            control,
            Startup::Remount {
                target: "/newroot/etc/machine-id".into(),
                options: "remount,bind,ro,nodev,nosuid,noexec".into(),
            },
        )?;
        Ok(())
    })()
    .context(mica_deploy::deployments::SharedDataFailure)?;
    eprintln!("mica-init: persistent DATA identity ready before system init");
    fs::create_dir_all("/run/mica")?;
    fs::write(
        "/run/mica/boot.json",
        serde_json::to_vec(&serde_json::json!({
            "deploymentId": id, "entry": selected, "kernelId": deployment.kernel.id,
            "rootfsId": deployment.rootfs.id, "contentVerified": true, "secureBoot": secure_boot,
            "backend": backend, "bootVerified": secure_boot || backend == BootKind::UbootFit,
        }))?,
    )?;
    fs::write("/run/mica/deployment.json", &envelope)?;
    let manifest = String::from_utf8(bounded_file("/exitrd.files", 8192)?)?;
    let retained_bytes = exitrd_tmpfs_bytes(Path::new("/exitrd"), &manifest)?;
    mount(
        control,
        "tmpfs",
        "/run/initramfs",
        "tmpfs",
        &format!("nosuid,nodev,mode=0700,size={retained_bytes},nr_inodes=8192"),
    )?;
    copy_exitrd(Path::new("/exitrd"), Path::new("/run/initramfs"), &manifest)?;
    let state = control.observe()?;
    for mount in state.mounts.into_iter().filter(|m| {
        control.storage.backings.contains(&m.device)
            || control
                .storage
                .mappings
                .iter()
                .any(|dm| dm.device == m.device)
    }) {
        if !control.storage.mounts.iter().any(|old| old.id == mount.id) {
            control.storage.mounts.push(mount);
        }
    }
    let mut handoff = control.storage.clone();
    handoff.allow_extra_loops = true;
    fs::write("/run/initramfs/storage.json", serde_json::to_vec(&handoff)?)?;
    startup::validate_new_root(Path::new("/newroot"))?;
    for (source, target) in [
        ("/system", "/newroot/mnt/system"),
        ("/support", "/newroot/run/mica-support"),
    ] {
        // /run moves below; keep the support mount in that tmpfs first.
        let destination = if source == "/support" {
            "/run/mica-support"
        } else {
            target
        };
        if source == "/support" {
            fs::create_dir_all(destination)?;
        }
        ensure!(
            Path::new(destination).is_dir(),
            "missing immutable mountpoint {destination}"
        );
        run(
            control,
            Startup::Move {
                source: source.into(),
                target: destination.into(),
            },
        )?;
    }
    for dir in ["dev", "proc", "sys", "run"] {
        run(
            control,
            Startup::Move {
                source: format!("/{dir}"),
                target: format!("/newroot/{dir}"),
            },
        )?;
    }
    fs::copy("/etc/mica/boot.json", "/newroot/run/mica/boot-policy.json")?;
    eprintln!("mica-init: verified deployment {id}; support mounted before system init");
    if let Ok(status) = fs::read_to_string("/newroot/proc/self/status") {
        let peak = status.lines().find_map(|line| {
            line.strip_prefix("VmHWM:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        });
        if let Some(peak) = peak {
            eprintln!(
                "mica-init: metrics elapsedMs={} peakRssKiB={peak}",
                started.elapsed().as_millis()
            );
        }
    }
    native::switch_root()
}

pub fn main() {
    let _ = rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    );
    if let Some(result) = native::worker(&std::env::args().skip(1).collect::<Vec<_>>()) {
        if let Err(error) = result {
            eprintln!("mica-init startup worker: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if std::process::id() != 1 {
        eprintln!("mica-init must run as PID 1");
        std::process::exit(1);
    }
    let mut control = BootControl {
        supervisor: Supervisor::new(),
        storage: Ownership::default(),
    };
    let mut attempt = None;
    let error = boot(&mut control, &mut attempt)
        .err()
        .unwrap_or_else(|| anyhow::anyhow!("boot unexpectedly returned"));
    let _ = shutdown::diagnostic(&format!("mica-init: boot refused: {error:#}"));
    let mut recovery = error.is::<SharedSystemFailure>()
        || error.is::<mica_deploy::deployments::SharedDataFailure>();
    let cleanup = (|| -> Result<()> {
        let budget = control.supervisor.begin_shutdown()?;
        let deadline = budget.operation_deadline(control.supervisor.now_ms())?;
        SystemIo {
            supervisor: &mut control.supervisor,
            executable: "/sbin/mica-shutdown",
        }
        .execute(&Operation::Private, deadline)?;
        let deadline = budget.operation_deadline(control.supervisor.now_ms())?;
        SystemIo {
            supervisor: &mut control.supervisor,
            executable: "/sbin/mica-shutdown",
        }
        .execute(&Operation::Quiesce, deadline)?;
        ensure!(
            control.observe()?.processes.is_empty(),
            "startup workers remain before record retirement"
        );
        control.supervisor.prepare_process()?;
        if !recovery
            && let Err(error) = attempt
                .as_ref()
                .context("boot selection could not be established")
                .and_then(|selected| selected.retire_failed_confirmed(&mut control))
        {
            let _ = shutdown::diagnostic(&format!("mica-init: recovery required: {error:#}"));
            recovery = true;
        }
        let action = if recovery {
            Action::Poweroff
        } else {
            Action::Reboot
        };
        shutdown::diagnostic(&format!(
            "MICA_SHUTDOWN stage=entered action={} source=partial-startup",
            action.as_str()
        ))?;
        shutdown::finish(
            &mut SystemIo {
                supervisor: &mut control.supervisor,
                executable: "/sbin/mica-shutdown",
            },
            budget,
            &control.storage,
            action,
        )
    })();
    let failure = cleanup
        .err()
        .unwrap_or_else(|| anyhow::anyhow!("shutdown returned"));
    control.supervisor.failure(&failure)
}

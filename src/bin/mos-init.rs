//! The authenticated initramfs PID 1. No network or shell policy is accepted.
#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use mos_deploy::{
    boot::{
        BootKind, copy_exitrd, fit_selected, persistent_machine_id, selected_entry, utf16_variable,
        verity_args,
    },
    components::{BootIdentity, VerityImage, component_id, verify_deployment},
    deployments::{BootBackend, DeploymentStore, boot_partition},
};
use serde::Deserialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
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
}

impl BootAttempt {
    fn retire_failed_confirmed(&self) -> Result<()> {
        let system = fs::canonicalize(
            Path::new("/sys/class/block").join(
                Path::new(&self.system_device)
                    .file_name()
                    .context("missing SYSTEM device")?,
            ),
        )?;
        let device = boot_partition(&system, self.backend)?;
        let boot = match self.backend {
            BootKind::Uefi => {
                mount(
                    device.to_str().context("invalid ESP device")?,
                    "/boot-state",
                    "vfat",
                    "rw,nodev,nosuid,noexec",
                )?;
                BootBackend::Uefi {
                    esp: "/boot-state".into(),
                }
            }
            BootKind::UbootFit => BootBackend::Fit { firmware: device },
        };
        // PID 1 has not started any DATA writer. Only the native boot record
        // changes here; the fallback reconciles diagnostic state after boot.
        let store = DeploymentStore::new("/system".into(), boot, "/unused-meta".into());
        let result = store.retire_failed_confirmed(&self.id);
        if self.backend == BootKind::Uefi {
            run(
                "/bin/mount",
                &["-o", "remount,ro,nodev,nosuid,noexec", "/boot-state"],
            )?;
        }
        if result? {
            eprintln!("mos-init: retired failed confirmed deployment {}", self.id);
        }
        Ok(())
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

fn run(program: &str, args: &[&str]) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        // /dev/null does not exist until the first devtmpfs mount completes.
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("execute {program}"))?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let output = child.wait_with_output()?;
            ensure!(status.success(), "{program} failed: {status}");
            ensure!(output.stdout.len() <= 16384, "excessive command output");
            return Ok(String::from_utf8(output.stdout)?.trim().to_owned());
        }
        if start.elapsed() >= Duration::from_secs(30) {
            child.kill()?;
            child.wait()?;
            bail!("{program} timed out");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn mount(source: &str, target: &str, kind: &str, options: &str) -> Result<()> {
    fs::create_dir_all(target)?;
    run("/bin/mount", &["-t", kind, "-o", options, source, target])
        .with_context(|| format!("mount {source} at {target}"))?;
    Ok(())
}

fn verified_mount(
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
    let loop_device = run("/sbin/losetup", &["--read-only", "--find", "--show", path])?;
    ensure!(
        loop_device.starts_with("/dev/loop")
            && loop_device[9..].bytes().all(|b| b.is_ascii_digit()),
        "invalid loop device"
    );
    let args = verity_args(&loop_device, name, signature, image);
    run(
        "/sbin/veritysetup",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    let table = run("/sbin/dmsetup", &["table", name])?;
    ensure!(
        table
            .split_whitespace()
            .any(|s| s == "root_hash_sig_key_desc"),
        "kernel mapping has no verified signature"
    );
    let mut read_only = false;
    for entry in fs::read_dir("/sys/block")? {
        let path = entry?.path();
        if fs::read_to_string(path.join("dm/name")).is_ok_and(|value| value.trim() == name) {
            read_only = fs::read_to_string(path.join("ro"))?.trim() == "1";
        }
    }
    ensure!(read_only, "kernel mapping is not read-only");
    mount(
        &format!("/dev/mapper/{name}"),
        target,
        "squashfs",
        "ro,nodev",
    )
}

fn boot(attempt: &mut Option<BootAttempt>) -> Result<()> {
    let started = Instant::now();
    ensure!(std::process::id() == 1, "mos-init must run as PID 1");
    mount("devtmpfs", "/dev", "devtmpfs", "nosuid,mode=0755")?;
    mount("proc", "/proc", "proc", "nosuid,nodev,noexec")?;
    mount("sysfs", "/sys", "sysfs", "nosuid,nodev,noexec")?;
    mount("tmpfs", "/run", "tmpfs", "nosuid,nodev,mode=0755,size=32M")?;
    ensure!(
        fs::read_to_string("/sys/module/dm_verity/parameters/require_signatures")?.trim() == "Y",
        "signature enforcement is disabled"
    );
    // Arming is synchronous and precedes access to deployment storage. The
    // signed cmdline fixes the timeout; NOWAYOUT keeps it armed across exec.
    let mut watchdog = OpenOptions::new()
        .write(true)
        .open("/dev/watchdog")
        .context("required boot watchdog is unavailable")?;
    watchdog.write_all(b"1")?;
    eprintln!("mos-init: boot watchdog armed");
    eprintln!("mos-init: pseudo-filesystems and signature policy ready");
    let config: Config = serde_json::from_slice(&bounded_file("/etc/mos/boot.json", 4096)?)?;
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
                "/sys/firmware/devicetree/base/chosen/mos,deployment-id",
                65,
            )?)?;
            (format!("fit:{id}"), id, false)
        }
    };
    eprintln!("mos-init: selected deployment {id}");
    let mut device = String::new();
    let discovery = Instant::now();
    while discovery.elapsed() < Duration::from_secs(15) {
        if let Ok(found) = run(
            "/sbin/blkid",
            &[
                "-t",
                &format!("PARTUUID={}", config.system_part_uuid),
                "-o",
                "device",
            ],
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
    watchdog.write_all(b"1")?;
    // ext4 replays a dirty journal before completing this read-only mount.
    mount(&device, "/system", "ext4", "ro,nodev,nosuid,noexec").context(SharedSystemFailure)?;
    *attempt = Some(BootAttempt {
        id: id.clone(),
        backend,
        system_device: device.clone(),
    });
    eprintln!("mos-init: SYSTEM mounted read-only");
    let envelope = bounded_file(&format!("/system/deployments/{id}.json"), 24576)?;
    let deployment = verify_deployment(&envelope, &keys, &config.identity)?;
    let value = serde_json::to_value(&deployment)?;
    ensure!(
        component_id(&value)? == id,
        "selected deployment identity mismatch"
    );
    let paths = deployment.paths()?;
    verified_mount(
        &format!("/system/{}", paths.rootfs),
        &format!("/system/roots/{}/rootfs.roothash.p7s", deployment.rootfs.id),
        "mos-root",
        "/newroot",
        &deployment.rootfs.content,
    )?;
    verified_mount(
        &format!("/system/{}", paths.support),
        &format!(
            "/system/kernels/{}/support.roothash.p7s",
            deployment.kernel.id
        ),
        "mos-support",
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
        run("/bin/mount", &["--bind", source, target])?;
        run(
            "/bin/mount",
            &["-o", "remount,bind,ro,nodev,nosuid", target],
        )?;
    }
    (|| -> Result<()> {
        let data = run(
            "/sbin/blkid",
            &[
                "-t",
                &format!("PARTUUID={}", config.data_part_uuid),
                "-o",
                "device",
            ],
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
        mount(&data, "/newroot/mnt/data", "ext4", "rw,noatime,prjquota")?;
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
            "/bin/mount",
            &[
                "--bind",
                "/newroot/mnt/data/state/machine-id",
                "/newroot/etc/machine-id",
            ],
        )?;
        run(
            "/bin/mount",
            &[
                "-o",
                "remount,bind,ro,nodev,nosuid,noexec",
                "/newroot/etc/machine-id",
            ],
        )?;
        Ok(())
    })()
    .context(mos_deploy::deployments::SharedDataFailure)?;
    eprintln!("mos-init: persistent DATA identity ready before system init");
    watchdog.write_all(b"1")?;
    fs::create_dir_all("/run/mos")?;
    fs::write(
        "/run/mos/boot.json",
        serde_json::to_vec(&serde_json::json!({
            "deploymentId": id, "entry": selected, "kernelId": deployment.kernel.id,
            "rootfsId": deployment.rootfs.id, "contentVerified": true, "secureBoot": secure_boot,
            "backend": backend, "bootVerified": secure_boot || backend == BootKind::UbootFit,
        }))?,
    )?;
    fs::write("/run/mos/deployment.json", &envelope)?;
    mount(
        "tmpfs",
        "/run/initramfs",
        "tmpfs",
        "nosuid,nodev,mode=0700,size=36M",
    )?;
    copy_exitrd(
        Path::new("/exitrd"),
        Path::new("/run/initramfs"),
        std::str::from_utf8(&bounded_file("/exitrd.files", 8192)?)?,
    )?;
    for (source, target) in [
        ("/system", "/newroot/mnt/system"),
        ("/support", "/newroot/run/mos-support"),
    ] {
        // /run moves below; keep the support mount in that tmpfs first.
        let destination = if source == "/support" {
            "/run/mos-support"
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
        run("/bin/mount", &["--move", source, destination])?;
    }
    for dir in ["dev", "proc", "sys", "run"] {
        run(
            "/bin/mount",
            &["--move", &format!("/{dir}"), &format!("/newroot/{dir}")],
        )?;
    }
    fs::copy("/etc/mos/boot.json", "/newroot/run/mos/boot-policy.json")?;
    eprintln!("mos-init: verified deployment {id}; support mounted before system init");
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
                "mos-init: metrics elapsedMs={} peakRssKiB={peak}",
                started.elapsed().as_millis()
            );
        }
    }
    Err(Command::new("/sbin/switch_root")
        .args(["/newroot", "/sbin/init"])
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .exec())
    .context("switch_root failed")
}

fn main() {
    let _ = rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    );
    let mut recovery = false;
    let mut attempt = None;
    if let Err(error) = boot(&mut attempt) {
        recovery = error.is::<SharedSystemFailure>()
            || error.is::<mos_deploy::deployments::SharedDataFailure>();
        eprintln!("mos-init: boot refused: {error:#}");
        if !recovery {
            match attempt
                .context("boot selection could not be established")
                .and_then(|attempt| attempt.retire_failed_confirmed())
            {
                Ok(()) => (),
                Err(error) => {
                    eprintln!("mos-init: recovery required: {error:#}");
                    recovery = true;
                }
            }
        }
    }
    if std::process::id() != 1 {
        std::process::exit(1);
    }
    thread::sleep(Duration::from_secs(2));
    let command = if recovery {
        rustix::system::RebootCommand::PowerOff
    } else {
        rustix::system::RebootCommand::Restart
    };
    let _ = rustix::system::reboot(command);
    // If firmware cannot power off, hold recovery without consuming more
    // deployment attempts. A shared filesystem failure affects every entry.
    let mut watchdog = OpenOptions::new().write(true).open("/dev/watchdog").ok();
    loop {
        if let Some(watchdog) = &mut watchdog {
            let _ = watchdog.write_all(b"1");
        }
        thread::sleep(Duration::from_secs(10));
    }
}

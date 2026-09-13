//! Device entry point for signed component installation and boot confirmation.
#![forbid(unsafe_code)]
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand};
use mica_deploy::{
    acquisition::Acquisition,
    boot::BootKind,
    components::BootIdentity,
    deployments::{BootBackend, BootReceipt, DeploymentStore, SharedDataFailure, read_bounded},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    name = "mica-deploy",
    version,
    about = "Manage authenticated file deployments"
)]
struct Cli {
    #[arg(long, default_value_t = 512 * 1024 * 1024)]
    max_bytes: u64,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Status,
    Probe,
    Discard,
    Booted,
    FailBoot,
    FirmwareReadback {
        #[arg(default_value = "/mnt/data/meta/firmware.json")]
        manifest: PathBuf,
        #[arg(long)]
        record: bool,
    },
    Gc,
    Confirm,
    Reject {
        id: String,
    },
    Rollback,
    Check {
        #[arg(long)]
        source: String,
        #[arg(long)]
        channel: String,
    },
    Fetch {
        #[arg(long)]
        source: String,
        #[arg(long)]
        channel: String,
    },
    Import {
        archive: PathBuf,
    },
    Install {
        descriptor: PathBuf,
        #[arg(long)]
        objects: PathBuf,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Policy {
    identity: BootIdentity,
    public_keys: Vec<String>,
    system_part_uuid: String,
    data_part_uuid: String,
}

fn receipt() -> Result<BootReceipt> {
    Ok(serde_json::from_slice(&read_bounded(
        Path::new("/run/mica/boot.json"),
        4096,
    )?)?)
}
fn command(program: &str, args: &[&str]) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait()? {
            let output = child.wait_with_output()?;
            ensure!(status.success(), "{program} failed: {status}");
            ensure!(output.stdout.len() <= 4096, "excessive command output");
            return Ok(String::from_utf8(output.stdout)?.trim().to_owned());
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("command timed out: {program}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn remount(path: &str, mode: &str) -> Result<()> {
    command("/bin/mount", &["-o", &format!("remount,{mode}"), path])?;
    Ok(())
}
fn mount_device(text: &str, path: &str, filesystem: &str, writable: bool) -> Result<String> {
    let found = text
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            (fields.len() >= 10 && fields[4] == path).then_some(fields)
        })
        .collect::<Vec<_>>();
    ensure!(found.len() == 1, "shared storage unavailable: {path}");
    let fields = &found[0];
    ensure!(
        fields[3] == "/",
        "shared storage is a subdirectory bind: {path}"
    );
    let separator = fields
        .iter()
        .position(|field| *field == "-")
        .context("invalid mount record")?;
    ensure!(
        fields.get(separator + 1) == Some(&filesystem),
        "wrong filesystem: {path}"
    );
    if writable {
        ensure!(
            fields[5].split(',').any(|option| option == "rw"),
            "shared storage is read-only: {path}"
        );
    }
    Ok(fields[2].to_string())
}
fn require_mount(path: &str, filesystem: &str, writable: bool, number: u8) -> Result<PathBuf> {
    let device = mount_device(
        &std::fs::read_to_string("/proc/self/mountinfo")?,
        path,
        filesystem,
        writable,
    )?;
    let (major, minor) = device.split_once(':').context("invalid mount device")?;
    ensure!(
        !major.is_empty()
            && !minor.is_empty()
            && major
                .bytes()
                .chain(minor.bytes())
                .all(|c| c.is_ascii_digit()),
        "invalid mount device"
    );
    let physical = std::fs::canonicalize(format!("/sys/dev/block/{device}"))?;
    ensure!(
        std::fs::read_to_string(physical.join("partition"))?.trim() == number.to_string(),
        "wrong partition: {path}"
    );
    Ok(physical)
}

fn policy() -> Result<(Policy, Vec<[u8; 32]>)> {
    let policy: Policy = serde_json::from_slice(&read_bounded(
        Path::new("/run/mica/boot-policy.json"),
        4096,
    )?)?;
    ensure!(
        !policy.system_part_uuid.is_empty(),
        "missing SYSTEM binding"
    );
    let keys = policy
        .public_keys
        .iter()
        .map(|key| {
            let bytes = STANDARD.decode(key)?;
            ensure!(STANDARD.encode(&bytes) == *key, "noncanonical metadata key");
            bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid metadata key"))
        })
        .collect::<Result<_>>()?;
    Ok((policy, keys))
}

fn main() -> Result<()> {
    let result = execute();
    if let Err(error) = &result
        && error.is::<SharedDataFailure>()
    {
        std::fs::write("/run/mica/shared-data-failure", format!("{error:#}"))?;
    }
    result
}

fn execute() -> Result<()> {
    let cli = Cli::parse();
    let receipt = receipt().context("authenticated boot receipt is unavailable")?;
    let (policy, keys) = policy()?;
    let backend = BootKind::for_board(&policy.identity.board)?;
    ensure!(
        receipt.backend == backend,
        "boot receipt backend differs from signed policy"
    );
    let system = require_mount("/mnt/system", "ext4", false, 2)?;
    let boot = match backend {
        BootKind::Uefi => {
            let esp = require_mount("/boot", "vfat", false, 1)?;
            ensure!(
                system.parent() == esp.parent(),
                "ESP and SYSTEM are on different disks"
            );
            BootBackend::Uefi {
                esp: "/boot".into(),
            }
        }
        BootKind::UbootFit => BootBackend::Fit {
            layout: mica_deploy::fit_env::FitLayout::for_board(&policy.identity.board)?,
            firmware: mica_deploy::deployments::boot_partition(
                &system,
                backend,
                &policy.identity.board,
            )?,
        },
    };
    let store = DeploymentStore::new("/mnt/system".into(), boot, "/mnt/data/meta".into());
    let data_check = (|| -> Result<()> {
        let data = require_mount("/mnt/data", "ext4", true, 3)?;
        ensure!(
            system.parent() == data.parent(),
            "DATA and SYSTEM are on different disks"
        );
        let node = data
            .file_name()
            .context("missing DATA device")?
            .to_str()
            .context("invalid DATA device")?;
        ensure!(
            command(
                "/sbin/blkid",
                &["-s", "PARTUUID", "-o", "value", &format!("/dev/{node}")]
            )? == policy.data_part_uuid.to_ascii_lowercase(),
            "DATA differs from authenticated boot policy"
        );
        Ok(())
    })();
    data_check.context(SharedDataFailure)?;
    let node = system
        .file_name()
        .context("missing SYSTEM device")?
        .to_str()
        .context("invalid SYSTEM device")?;
    ensure!(
        command(
            "/sbin/blkid",
            &["-s", "PARTUUID", "-o", "value", &format!("/dev/{node}")]
        )? == policy.system_part_uuid.to_ascii_lowercase(),
        "mounted SYSTEM differs from authenticated boot policy"
    );
    if matches!(cli.command, Action::Booted) {
        ensure!(
            receipt.content_verified,
            "running content is not authenticated"
        );
        mica_deploy::deployments::valid_id(&receipt.deployment_id)?;
        println!("{}", receipt.deployment_id);
        return Ok(());
    }
    if matches!(cli.command, Action::Status) {
        println!(
            "{}",
            json!({"boot":receipt,"state":store.effective_state()?,"deployments":store.describe(&keys)?})
        );
        return Ok(());
    }
    ensure!(
        receipt.content_verified,
        "running content is not authenticated"
    );
    let _lock = store.lock()?;
    if let Action::FirmwareReadback { manifest, record } = &cli.command {
        let envelope = read_bounded(manifest, 6500)?;
        let firmware = mica_deploy::firmware::authenticate_firmware(&envelope, &keys)?;
        ensure!(
            firmware.board == policy.identity.board && firmware.arch == policy.identity.arch,
            "firmware target differs from signed board policy"
        );
        mica_deploy::firmware::verify_installed(&firmware, &store.boot)?;
        if *record {
            mica_deploy::deployments::atomic_write(
                Path::new("/mnt/data/meta/firmware.json"),
                &envelope,
            )?;
        }
        println!(
            "{}",
            json!({"firmwareId": firmware.id, "generation": firmware.generation, "readbackVerified": true, "receiptRecorded": record})
        );
        return Ok(());
    }
    if matches!(
        cli.command,
        Action::Probe
            | Action::Discard
            | Action::Check { .. }
            | Action::Fetch { .. }
            | Action::Import { .. }
    ) {
        require_workspace()?;
        let acquisition = Acquisition {
            root: "/mica/updates".into(),
            store: &store,
            keys: &keys,
            board: &policy.identity.board,
            arch: &policy.identity.arch,
            max_bytes: cli.max_bytes,
        };
        let result = match cli.command {
            Action::Probe => acquisition.probe()?,
            Action::Discard => json!({"removedFiles":acquisition.discard()?}),
            Action::Check { source, channel } => {
                let catalog =
                    acquisition.check(&source, &channel, chrono::Utc::now().timestamp())?;
                json!({"revision":catalog.checkpoint.revision,"channel":channel,"selected":catalog.selected})
            }
            Action::Fetch { source, channel } => {
                let catalog =
                    acquisition.check(&source, &channel, chrono::Utc::now().timestamp())?;
                match catalog.selected {
                    Some(selected) => json!(acquisition.fetch(selected)?),
                    None => serde_json::Value::Null,
                }
            }
            Action::Import { archive } => {
                ensure!(
                    archive.symlink_metadata()?.is_file(),
                    "archive is not a regular file"
                );
                json!(acquisition.import(&mut std::fs::File::open(archive)?)?)
            }
            _ => unreachable!(),
        };
        println!("{result}");
        return Ok(());
    }
    let install = matches!(cli.command, Action::Install { .. } | Action::Gc);
    if install {
        remount("/mnt/system", "rw")?;
    }
    if backend == BootKind::Uefi
        && let Err(error) = remount("/boot", "rw")
    {
        if install {
            remount("/mnt/system", "ro")?;
        }
        return Err(error);
    }
    let result: Result<_> = (|| match cli.command {
        Action::Confirm => store.confirm(&receipt).map(|value| json!(value)),
        Action::FailBoot => store
            .fail_boot(&receipt)
            .map(|retired| json!({"retired": retired})),
        Action::Reject { id } => store.reject(&id).map(|value| json!(value)),
        Action::Rollback => store.rollback(&receipt).map(|value| json!(value)),
        Action::Install {
            descriptor,
            objects,
        } => store
            .install(
                &read_bounded(&descriptor, 24576)?,
                &keys,
                &policy.identity.board,
                &policy.identity.arch,
                &objects,
                &receipt,
            )
            .map(|value| json!(value)),
        Action::Gc => store
            .collect(&receipt, &keys)
            .map(|count| json!({"removedFiles":count})),
        Action::Status
        | Action::Probe
        | Action::Discard
        | Action::Booted
        | Action::FirmwareReadback { .. }
        | Action::Check { .. }
        | Action::Fetch { .. }
        | Action::Import { .. } => unreachable!(),
    })();
    let esp_sync = if backend == BootKind::Uefi {
        remount("/boot", "ro")
    } else {
        Ok(())
    };
    let system_sync = if install {
        remount("/mnt/system", "ro")
    } else {
        Ok(())
    };
    esp_sync?;
    system_sync?;
    println!("{}", serde_json::to_string(&result?)?);
    Ok(())
}

fn require_workspace() -> Result<()> {
    let table = std::fs::read_to_string("/proc/self/mountinfo")?;
    let data = mount_device(&table, "/mnt/data", "ext4", true)?;
    let mut namespace = None;
    for line in table.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        if fields[4] == "/mica" {
            ensure!(
                namespace.is_none()
                    && fields[2] == data
                    && fields[3] == "/mica"
                    && fields[5].split(',').any(|option| option == "rw"),
                "invalid DATA namespace binding"
            );
            namespace = Some(());
        }
        ensure!(
            fields[4] != "/mica/updates" && !fields[4].starts_with("/mica/updates/"),
            "unexpected mount in update workspace"
        );
    }
    ensure!(namespace.is_some(), "DATA namespace is not mounted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::mount_device;

    #[test]
    fn storage_requires_one_complete_physical_mount() {
        let mount = "35 22 253:2 / /mnt/system ro,noatime - ext4 /dev/vda2 ro\n";
        assert_eq!(
            mount_device(mount, "/mnt/system", "ext4", false).unwrap(),
            "253:2"
        );
        assert!(mount_device(mount, "/mnt/system", "ext4", true).is_err());
        assert!(mount_device(mount, "/mnt/system", "vfat", false).is_err());
        assert!(mount_device(&mount.repeat(2), "/mnt/system", "ext4", false).is_err());
        assert!(
            mount_device(
                &mount.replace(" / /mnt", " /roots /mnt"),
                "/mnt/system",
                "ext4",
                false
            )
            .is_err()
        );
    }
}

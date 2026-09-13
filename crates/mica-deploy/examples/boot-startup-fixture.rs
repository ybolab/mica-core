//! Disposable guest acceptance driver; never exported by a native producer.
#![forbid(unsafe_code)]
use anyhow::{Context, Result, ensure};
use mica_deploy::{
    boot::startup::{native, verity},
    components::VerityImage,
};
use std::{
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::Path,
};

fn descriptor(path: &str) -> Result<VerityImage> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn target(device: &str, image: &VerityImage) -> Result<lifecycle_sys::DmTarget> {
    let dev = fs::metadata(device)?.rdev();
    verity::table(
        rustix::fs::major(dev),
        rustix::fs::minor(dev),
        "cryptsetup:mica-root",
        image,
    )
}
fn live() -> Result<Vec<lifecycle_sys::DmTarget>> {
    let fd = File::open("/dev/mapper/control")?;
    let device = fs::metadata("/dev/mapper/mica-root")?.rdev();
    let status = lifecycle_sys::dm_status(&fd, device)?;
    Ok(lifecycle_sys::dm_table(&fd, &status)?)
}
fn print_table(targets: Vec<lifecycle_sys::DmTarget>) {
    for t in targets {
        println!("{} {} {} {}", t.sector, t.length, t.kind, t.parameters);
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mode = args
        .first()
        .map(String::as_str)
        .unwrap_or("transition-result");
    match mode {
        "loop" => println!(
            "{}",
            native::attach_loop(Path::new(args.get(1).context("image path")?))?
        ),
        "partition" => println!(
            "{}",
            native::gpt::find_partition(args.get(1).context("UUID")?)?
        ),
        "open" => {
            ensure!(args.len() == 4, "open device signature descriptor");
            let image = descriptor(&args[3])?;
            verity::open(&args[1], "mica-root", Path::new(&args[2]), &image)?;
            print_table(live()?);
        }
        "table" => print_table(live()?),
        "status" => {
            let fd = File::open("/dev/mapper/control")?;
            let device = fs::metadata("/dev/mapper/mica-root")?.rdev();
            lifecycle_sys::dm_status(&fd, device)?;
            println!("READONLY_STATUS_PASS");
        }
        "expected" => {
            ensure!(args.len() == 3, "expected device descriptor");
            print_table(vec![target(&args[1], &descriptor(&args[2])?)?]);
        }
        "skip-key" | "omit-signature" => {
            ensure!(args.len() == 3, "mutation device descriptor");
            let expected = target(&args[1], &descriptor(&args[2])?)?;
            let mut mutated = expected.clone();
            if mode == "omit-signature" {
                mutated.parameters = mutated.parameters.replace(
                    "3 panic_on_corruption root_hash_sig_key_desc cryptsetup:mica-root",
                    "1 panic_on_corruption",
                );
            }
            let fd = File::open("/dev/mapper/control")?;
            let created = lifecycle_sys::dm_create(&fd, "mica-root", "MICA-startup-mutation")?;
            let loaded = lifecycle_sys::dm_load_verity(&fd, &created, &mutated);
            if mode == "skip-key" {
                ensure!(
                    loaded.is_err(),
                    "key insertion mutation unexpectedly loaded"
                );
                println!("KEY_INSERTION_MUTATION_TABLE_REFUSAL {:?}", loaded.err());
            } else {
                loaded?;
                let status = lifecycle_sys::dm_resume(&fd, &created)?;
                let targets = lifecycle_sys::dm_table(&fd, &status)?;
                ensure!(
                    verity::verify_table(&expected, &targets).is_err(),
                    "signature guard mutation unexpectedly passed"
                );
                println!("SIGNATURE_READBACK_MUTATION_REFUSED");
            }
            lifecycle_sys::dm_discard_created(&fd, &created)?;
        }
        "read" => {
            let mut bytes = Vec::new();
            File::open("/dev/mapper/mica-root")?.read_to_end(&mut bytes)?;
            ensure!(bytes.len() >= 8192, "short authenticated read");
            println!("AUTHENTICATED_READ_PASS {}", bytes.len());
        }
        "switch" => native::switch_root()?,
        "transition-result" => {
            ensure!(std::process::id() == 1, "transition init is not PID1");
            for (path, kind) in [
                ("/dev", 0x01021994),
                ("/proc", 0x9fa0),
                ("/sys", 0x62656572),
                ("/run", 0x01021994),
            ] {
                ensure!(
                    rustix::fs::statfs(path)?.f_type == kind,
                    "API mount transition failed: {path}"
                );
            }
            ensure!(
                fs::read_to_string("/run/transition-marker")? == "retained\n",
                "run mount content lost"
            );
            ensure!(
                !Path::new("/old-root-file").exists(),
                "old root file survived transition"
            );
            println!("NATIVE_SWITCH_ROOT_API_AND_RECLAMATION_PASS");
            rustix::system::reboot(rustix::system::RebootCommand::PowerOff)?;
        }
        _ => anyhow::bail!("unknown fixture operation"),
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("startup fixture: {error:#}");
        std::process::exit(1);
    }
}

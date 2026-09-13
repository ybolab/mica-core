//! PID 1 for the authenticated, memory-only retained lifecycle payload.
use anyhow::{Context, Result, ensure};
use mica_deploy::boot::shutdown::{self, Ownership, Request, Supervisor, SystemIo};
use std::{
    fs::{self, File},
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

fn shutdown(supervisor: &mut Supervisor, args: &[String]) -> Result<()> {
    let request = Request::parse(args)?;
    let action = request.action;
    supervisor.arm()?;
    let budget = supervisor.limit_shutdown(request.timeout_ms)?;
    supervisor.prepare_process()?;
    let file = File::from(
        rustix::fs::openat2(
            rustix::fs::CWD,
            "/storage.json",
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::NO_SYMLINKS,
        )
        .context("missing startup storage ownership record")?,
    );
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.len() <= 65536
            && metadata.dev() == fs::metadata("/")?.dev()
            && metadata.permissions().mode() & 0o7022 == 0,
        "invalid memory-only storage record"
    );
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "excessive storage record");
    let owner: Ownership = serde_json::from_slice(&bytes)?;
    mica_deploy::deployments::valid_id(&owner.deployment)?;
    ensure!(
        owner.allow_extra_loops,
        "partial startup record is not an exitrd handoff"
    );
    shutdown::diagnostic(&format!(
        "MICA_SHUTDOWN stage=entered action={} source=exitrd deployment={}",
        action.as_str(),
        owner.deployment
    ))?;
    shutdown::finish(
        &mut SystemIo {
            supervisor,
            executable: "/shutdown",
        },
        budget,
        &owner,
        action,
    )
}

pub fn main() {
    let _ = rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    );
    let args = std::env::args_os()
        .skip(1)
        .take(17)
        .map(|arg| arg.into_string())
        .collect::<Result<Vec<_>, _>>();
    let args = match args {
        Ok(args) => args,
        Err(_) => {
            let error = anyhow::anyhow!("shutdown arguments must be UTF-8");
            if std::process::id() == 1 {
                Supervisor::new().failure(&error);
            }
            let _ = shutdown::diagnostic(&error.to_string());
            std::process::exit(1);
        }
    };
    if let Some(result) = shutdown::worker(&args) {
        if let Err(error) = result {
            eprintln!("lifecycle worker refused: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if std::process::id() != 1 {
        eprintln!("mica-shutdown requires PID 1");
        std::process::exit(1);
    }
    let mut supervisor = Supervisor::new();
    let error = shutdown(&mut supervisor, &args)
        .err()
        .unwrap_or_else(|| anyhow::anyhow!("shutdown returned"));
    supervisor.failure(&error)
}

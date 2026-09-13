//! micad — management-plane daemon.
//!
//! Owns the settings tree (persisted via `micad-settings`) and a live-state
//! tree, exposed on D-Bus as `com.mica.micad` / `/com/mos/micad` /
//! `com.mica.micad1`. Configuration comes from the environment:
//!
//! - `MOSD_SETTINGS_PATH` — settings file (default
//!   `/var/lib/mica/settings.toml`).
//! - `MOSD_BUS` — `system` (default) or `session`.
//! - `MOSD_SHADOW_PATH` — the shadow file a transient root password is written
//!   into (default `/etc/shadow`, a symlink onto STATE on the mos image); the
//!   sshd reconciler honours the same variable.
//! - `MOSD_META_MANIFEST_PATH` — the baked update configuration
//!   (default `/usr/share/mica/meta/updates/manifest.json`, inside the
//!   read-only root); layer 1 of PLAN-070 §5.1. Read once at startup, since
//!   nothing on the device can write it. See
//!   [`micad_settings::configuration`].
//! - `MOSD_UPDATE_POLICY_PATH` — the operator document layer 1's defaults are
//!   overridden by (default `/mos/config/updates.json`, on DATA). apid reads
//!   the same two documents through the same resolver, so a status route and
//!   the update subsystem cannot disagree. See [`update_policy`].
//! - `MOSD_PROVISIONING_ROOT` — where the offline provisioning transport
//!   stages the media it found (default `/run/mica/provisioning`); tests point
//!   it at a temporary directory. See [`provisioning_doc`].
//! - `MOSD_DRY_RUN` — when `1`, first-boot provisioning is skipped, no
//!   reconcilers or service scan are constructed, power actions go to a no-op
//!   control, and the live-state root carries `{"dry_run": true}`; used by
//!   tests so the daemon never touches its host.
//! - `MOSD_SCAN` — a one-way test hook over the service scan ([`scan`]): `1`
//!   constructs the scan under `MOSD_DRY_RUN=1`, which alone constructs none.
//!   It can only turn the scan on; in production the scan is constructed
//!   unconditionally and the variable ignored. The scan is passive — a match
//!   rule and read-only bus calls writing only the in-RAM live-state tree — so
//!   lifting dry-run over it touches no file, unit or host state.
//!
//! `--version` (or `-V`) is answered before any of the above is read: it prints
//! `micad <crate version> (<build commit>)` and exits 0 without provisioning,
//! connecting to a bus or writing a file (see [`main`]). Anything else on the
//! command line is ignored.

#![forbid(unsafe_code)]

mod apply_queue;
mod bus;
mod deployment;
mod diagnostics;
mod fswrite;
mod identity;
mod network_state;
mod power;
mod provisioning;
mod provisioning_doc;
mod reconciler;
mod recovery;
mod reset;
mod scan;
mod storage_status;
mod system_info;
mod telemetry;
mod time_status;
mod transient;
mod update_auto;
mod update_codes;
mod update_lifecycle;
mod update_policy;
mod wgkeys;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use micad_settings::Store;
use serde_json::Value;
use tokio::signal::unix::{SignalKind, signal};

fn main() -> anyhow::Result<()> {
    // `--version` is answered and returned from here, above every line that
    // makes this process a daemon -- before the tokio runtime, the subscriber,
    // any environment read and the settings store. The position is the
    // requirement: first-boot provisioning writes /var/lib/mica/settings.toml
    // and /var/lib/mica/secrets/ before the daemon reaches the bus, so a handler
    // below it would answer a question and MUTATE the machine that asked.
    //
    // Only the version flag is intercepted; micad is started by systemd with no
    // arguments and ignores whatever else it is given.
    if wants_version(std::env::args().skip(1)) {
        println!("{}", version_line());
        return Ok(());
    }
    serve()
}

/// What the commit is reported as when the build supplied none.
///
/// An absent commit is a value, not a failure: a `--version` that exited
/// non-zero when `MICA_BUILD_COMMIT` is unset would turn "we do not know which
/// commit" into "this binary is broken". The smoke runner reads the same
/// distinction from the other side, asserting the commit only when the build
/// recorded one.
const UNKNOWN_COMMIT: &str = "unknown";

/// Whether an argv (argv[1..]) is asking for the version.
///
/// `-V` as well as `--version`, because clap gives `mica-mqttd` and
/// `mica-mqtt-broker` the pair and the four binaries spell the one question the
/// same way. A pure function over an iterator rather than a read of
/// `std::env::args`, so the tests below can drive it without spawning.
fn wants_version(args: impl IntoIterator<Item = String>) -> bool {
    args.into_iter()
        .any(|arg| arg == "--version" || arg == "-V")
}

/// The build commit, or [`UNKNOWN_COMMIT`], from whatever the build embedded.
///
/// Takes the embedded value as an argument rather than reading `option_env!`
/// itself: the absent case is then reachable from a test in a binary that WAS
/// built with a commit, which is the only build any of these tests ever run in.
fn commit_or_unknown(embedded: Option<&'static str>) -> &'static str {
    match embedded {
        Some(commit) if !commit.trim().is_empty() => commit,
        _ => UNKNOWN_COMMIT,
    }
}

/// The one line `--version` prints: `micad <version> (<commit>)`.
///
/// The version is `CARGO_PKG_VERSION`, the same `Cargo.toml` value
/// `verify/src/smoke-pins.ts` reads to decide what this binary must report,
/// so the comparison has two readers of one value. The commit is
/// `MICA_BUILD_COMMIT`, resolved on the host by `micad/hack/build-target.sh`:
/// there is deliberately no `build.rs` shelling out to git, because the build
/// mount is a git worktree and `git rev-parse HEAD` inside it fails with
/// `fatal: not a git repository`. Embedding a sha costs no reproducibility --
/// two builds of one commit agree -- but it does cost byte-identity across
/// commits, so anything comparing micad against apid across commits carries a
/// permanent expected difference.
fn version_line() -> String {
    format!(
        "{} {} ({})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        commit_or_unknown(option_env!("MICA_BUILD_COMMIT")),
    )
}

#[tokio::main]
async fn serve() -> anyhow::Result<()> {
    // INFO by default, not ERROR.
    //
    // `tracing_subscriber::fmt::init()` falls back to ERROR when RUST_LOG is
    // unset, which on a device means micad records what FAILED and never what
    // it did. RUST_LOG still wins, so a debug session is one variable away.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let settings_path = std::env::var("MOSD_SETTINGS_PATH")
        .unwrap_or_else(|_| micad_settings::DEFAULT_PATH.to_string());
    // The `/mos/config/` namespace. Relocatable for the same reason the
    // settings path is — the bus tests run a real daemon against a temporary
    // tree — and by a variable of its own rather than derived from
    // `MICA_DATA_ROOT`, because what micad reads is the `/mos` BIND and what
    // `mica-data-layout` and `reset.rs` write is the pool underneath it.
    let config_dir = std::env::var("MOSD_CONFIG_DIR")
        .unwrap_or_else(|_| micad_settings::DEFAULT_CONFIG_DIR.to_string());
    let bus_kind = std::env::var("MOSD_BUS").unwrap_or_else(|_| "system".to_string());
    let dry_run = std::env::var("MOSD_DRY_RUN").is_ok_and(|value| value == "1");

    let store = Store::new(&settings_path, &config_dir);
    // **Fail closed on the medium** (PLAN-070 §5.2.6). System configuration
    // lives on DATA now, so a DATA pool that does not mount is a device with
    // no configuration — and a device that cannot read its configuration must
    // not render a different one. Without this it would come up on schema
    // defaults, DHCP on every interface and sshd off, and look fine to
    // everything except the operator who configured a static address.
    // `RequiresMountsFor=/mos` on the unit is the first half; this is the half
    // that names the mount in the journal, and the error carries that name.
    // The recovery route is `docs/design/recovery.md`'s — the serial console
    // and the reset tiers — not a silently degraded network.
    //
    // `load_with_refusals` and not `load`, which is the other half
    // of the same rule (PLAN-070 §5.2.7, F6g — **the pour**). The medium is a
    // start refusal because a device that cannot reach its configuration must
    // not render a different one; a single DOCUMENT that does not parse is
    // not, because these files are hand-written by an integrator onto a device
    // that is not running, and a typo in `wifi.json` that stopped the daemon
    // would take the network reconciler down with it and put the device off
    // the air for a mistake in an unrelated subsystem. What that document
    // gates is refused; its neighbours run.
    let micad_settings::LoadedStore {
        mut settings,
        refusals,
    } = store
        .load_with_refusals()
        .with_context(|| format!("load settings from {settings_path} and {config_dir}"))?;
    // Loud, and at ERROR: a poured document that did not load is a device
    // running with a capability deliberately switched off, and the operator
    // who poured it has no other way to find out which file it was. The
    // message names the file, which is what the F6g gate requires of it.
    for refusal in &refusals {
        // `detail` is the parser's own sentence and it is logged HERE and
        // nowhere else: it quotes what it choked on, so a poured document's
        // site key can be in it, and the journal is on the device while
        // `configuration.refused` is served over the API.
        tracing::error!(
            document = refusal.document,
            subtrees = ?refusal.subtrees,
            detail = refusal.detail,
            "{}",
            refusal.message
        );
    }
    // **Narrowed here and once**, before anything on the boot path can save.
    // `provisioning::ensure_provisioned` writes the store on the very boot that
    // finds a pour — it mints the device identity — and `save` writes every
    // document out of an addressed tree in which a refused subtree sits at its
    // schema default. Without this the first boot after a bad pour would
    // overwrite the integrator's file with the default nobody chose, and the
    // second boot would come up clean on it (`Store::preserving`).
    let store = store.preserving(
        &refusals
            .iter()
            .map(|refusal| refusal.document.as_str())
            .collect::<Vec<_>>(),
    );
    // Before the reconcilers exist, so the very first reconcile already sees a
    // seeded tree rather than the built-in defaults. Skipped under dry-run,
    // which must not write to STATE at all.
    if dry_run {
        tracing::info!("dry run: first-boot provisioning skipped");
    } else {
        let state_dir = state_dir_for(&settings_path);
        // BEFORE both of the steps below, and before every reconciler: a
        // staged reset is what the operator asked this boot to do, so the rest
        // of the boot has to see the device the reset produced rather than
        // reconcile settings that are about to be taken away. Tiers 1 and 3
        // hand the seeded values back to `ensure_provisioned` below by putting
        // `provisioning.state` to `Pending`, which is why this call comes
        // first and not merely early.
        //
        // A device that is factory-reset here and then offered a provisioning
        // document is claimed by it on this same boot. That is the document
        // channel doing exactly what it is for — the reset made the device
        // unclaimed, and an unclaimed device with a stick in it is the case
        // `provisioning_doc` exists to serve.
        //
        // Its failure never stops the daemon, for `provisioning_doc`'s reason
        // and one more: the intent stays staged, so the next boot retries the
        // same tier rather than the operator losing the request
        // (`docs/design/recovery.md` §2.2).
        // BEFORE the staged reset, because this is where a staged reset can
        // come FROM: a board-declared physical recovery action
        // (`docs/design/recovery.md` §4) arrives as an intent on the kernel
        // command line, and mapping it stages the tier the board declared for
        // it. Running it first is what makes the action take effect on THIS
        // boot rather than the next one.
        //
        // Its failure never stops the daemon, for the same reason the two
        // steps below never do: a device whose operator asked for recovery and
        // whose assertion could not be written must still come up, legibly,
        // rather than not boot at all. On both shipped boards this refuses —
        // neither declares a physical action — and refusing is a recorded
        // outcome, not an error.
        match recovery::apply_boot_intent(&store, &mut settings, &recovery::Paths::from_env()) {
            Ok(outcome) => tracing::info!(?outcome, "boot recovery intent checked"),
            Err(err) => tracing::error!(
                error = %err,
                "a board-declared recovery action mapped and could not be carried out; \
                 the device boots normally and nothing is asserted"
            ),
        }
        match reset::apply_pending(&store, &mut settings, &reset::Roots::from_env()) {
            Ok(outcome) => tracing::info!(?outcome, "staged reset checked"),
            Err(error) => {
                // Keep application writers stopped until the staged scope is
                // complete; shared DATA failure needs recovery, not a new root.
                std::fs::write(
                    "/run/mica/shared-data-failure",
                    format!("staged reset failed: {error:#}"),
                )?;
                return Err(error).context("staged reset failed; DATA recovery required");
            }
        }
        // BEFORE seeding, and that order is the point: a factory-injected
        // `provisioning.deviceId` has to be in the tree when `ensure_identity`
        // decides whether to mint one, and when the hostname is derived from
        // it. Applied the other way round the device would mint an identity,
        // name itself after it, and only then be handed the identity the
        // factory recorded.
        //
        // Its failure NEVER stops the daemon, unlike the seeding below: a bad
        // document on a stick must leave a working, unclaimed appliance and a
        // legible record, not a device that will not boot. Only a failure to
        // WRITE reaches this arm — a refused document is a recorded outcome,
        // not an error.
        match provisioning_doc::import(
            &store,
            &mut settings,
            &provisioning_doc::staging_root_from_env(),
        ) {
            Ok(outcome) => tracing::info!(?outcome, "provisioning document checked"),
            Err(err) => tracing::error!(
                error = %err,
                "the provisioning document import could not be recorded; the device is \
                 unchanged and the next boot retries"
            ),
        }
        // Hard failure on purpose: an unwritable STATE means no device identity
        // and no device credential, so there is no usable device to serve. A
        // loud exit is better than a daemon that quietly serves an
        // unprovisioned tree the operator cannot log in to.
        let outcome = provisioning::ensure_provisioned(
            &store,
            &state_dir,
            Path::new(provisioning::DEFAULT_PROFILE_PATH),
            &mut settings,
        )
        .context("first-boot provisioning")?;
        tracing::info!(?outcome, state_dir = %state_dir.display(), "provisioning checked");
    }

    let reconcilers = if dry_run {
        Vec::new()
    } else {
        reconciler::all()
    };
    // Under dry-run the production control is never constructed, so a daemon
    // started by a test cannot reach systemd's manager at all.
    let power: Box<dyn power::PowerControl> = if dry_run {
        Box::new(power::DryRunPower)
    } else {
        Box::new(power::Systemd::new())
    };
    // The service registry exists only when a scan does, so that a daemon
    // running no scan answers `ForgetService` with "there is no registry"
    // rather than with an empty one it would never fill.
    //
    // Asymmetric on purpose: `MOSD_SCAN` can only ever turn the scan ON, for
    // `tests/scan.rs`, which must run under dry-run and so cannot otherwise
    // reach it. A symmetric form reads tidier but would also let it switch the
    // scan OFF, so one stray or mistyped variable (`MOSD_SCAN=0`,
    // `MOSD_SCAN=true`) would silently disable the service registry on a real
    // device, with nothing left running to report that it had.
    let scan_enabled = service_scan_enabled(dry_run, std::env::var("MOSD_SCAN").ok().as_deref());
    let registry = scan_enabled.then(|| Arc::new(scan::Registry::new()));
    tracing::info!(
        settings_path,
        config_dir,
        dry_run,
        reconcilers = reconcilers.len(),
        service_scan = scan_enabled,
        "micad starting"
    );

    let mut state = serde_json::Map::new();
    if dry_run {
        state.insert("dry_run".to_string(), Value::Bool(true));
    }
    // Layer 1 (PLAN-070 §5.1), read here and only here: the manifest is inside
    // the read-only dm-verity root, so its value cannot change while this
    // process runs and a re-read per decision would answer the same thing.
    // Read under dry-run too -- it is a read of one file in /usr/share and
    // touches nothing -- so a test daemon reports the same shape a device
    // does, with the error saying the host has no baked manifest.
    let meta_path = std::env::var("MOSD_META_MANIFEST_PATH").map_or_else(
        |_| PathBuf::from(micad_settings::configuration::DEFAULT_MANIFEST_PATH),
        PathBuf::from,
    );
    let meta = micad_settings::configuration::load_manifest(&meta_path);
    if let Some(error) = &meta.error {
        // Not fatal: the reader always answers with a document, and every
        // action the missing values gate refuses on its own terms. A build
        // refuses a manifest this reader would reject, so reaching this on a
        // device means the image is not the one the build produced.
        tracing::warn!(error, "baked update configuration unavailable");
    }
    state.insert("meta".to_string(), meta.to_json());

    let mut service = bus::MosdService::new(
        store,
        settings,
        reconcilers,
        power,
        transient::production_shadow_path(),
        Value::Object(state),
    );
    if !dry_run {
        service =
            service.with_network_state(Arc::new(network_state::SystemdNetworkState::production()));
        service = service.with_time_status(Arc::new(time_status::SystemdTimesync));
        service = service.with_storage_status(Arc::new(storage_status::HostStorage::production()));
        // The PLAN-052 observers, on the same rule: each reads the host
        // (sysfs, /etc, the journal), so a dry-run daemon is never given one.
        service = service.with_system_info(Arc::new(system_info::HostSystemInfo::production()));
        service = service.with_telemetry(Arc::new(telemetry::SysfsTelemetry::production()));
        service =
            service.with_failure_evidence(Arc::new(diagnostics::HostFailureEvidence::production()));
        // Same reasoning again: the rotation writes a private key onto STATE
        // and deletes a kernel device, so a dry-run daemon is never given one.
        service = service.with_wireguard(Arc::new(reconciler::network::KeyRotation::production()));
        let policy_path = std::env::var("MOSD_UPDATE_POLICY_PATH").map_or_else(
            |_| PathBuf::from(update_policy::DEFAULT_POLICY_PATH),
            PathBuf::from,
        );
        service = service.with_update(
            Arc::new(update_lifecycle::SubprocessClient::new(PathBuf::from(
                update_lifecycle::DEFAULT_CLIENT_PATH,
            ))),
            update_policy::PolicyStore::at(policy_path).with_baked(meta.manifest.update.clone()),
        );
    }
    if let Some(registry) = &registry {
        service = service.with_service_registry(Arc::clone(registry));
    }
    // Taken before the service moves onto the bus: the automatic driver
    // shares the lifecycle with the manual check and fetch routes, so both
    // read one policy and record into one state entry.
    let update_handle = service.update_handle();
    // BEFORE the first reconcile, which is the point: a refused document must
    // not be applied from its schema default even once, so the refusals have
    // to be in place before `apply_all` visits the reconcilers they gate.
    service.set_config_refusals(refusals).await;
    service.apply_all().await;
    let builder = match bus_kind.as_str() {
        "system" => zbus::connection::Builder::system()?,
        "session" => zbus::connection::Builder::session()?,
        other => anyhow::bail!("MOSD_BUS must be `system` or `session`, got `{other}`"),
    };
    let connection = builder
        .serve_at(bus::OBJECT_PATH, service)?
        .build()
        .await
        .with_context(|| format!("connect to {bus_kind} bus"))?;
    // The service scan, started before the well-known name is claimed so that
    // its NameOwnerChanged subscription is in place before anything can react
    // to micad appearing — a service that claims its name in that window is
    // seen by the signal rather than missed between the sweep and the
    // subscription.
    if let Some(registry) = registry {
        let service_ref = connection
            .object_server()
            .interface::<_, bus::MosdService>(bus::OBJECT_PATH)
            .await
            .context("look up served MosdService")?;
        tokio::spawn(scan::run(connection.clone(), registry, service_ref));
    }
    // The automatic update driver: the check cadence under `policy = check`,
    // and under `auto` the whole check/fetch/re-check/install pass with its
    // reboot. Started here rather than beside the lifecycle because the
    // install and reboot routes it calls live on the service the object
    // server now owns — the same reason the scan is started here. Never
    // under dry-run, whose lifecycle has no client to call anyway.
    if !dry_run {
        let service_ref = connection
            .object_server()
            .interface::<_, bus::MosdService>(bus::OBJECT_PATH)
            .await
            .context("look up served MosdService")?;
        tokio::spawn(update_auto::run(Arc::new(bus::BusRoutes::new(
            update_handle,
            service_ref,
        ))));
    }
    connection
        .request_name(bus::BUS_NAME)
        .await
        .with_context(|| format!("request name {}", bus::BUS_NAME))?;
    tracing::info!(bus = bus_kind, name = bus::BUS_NAME, "serving");

    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("SIGTERM received, exiting"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received, exiting"),
    }
    Ok(())
}

/// Directory holding STATE-backed data for a settings file at `settings_path`.
///
/// The secrets live beside the settings file, so tests that redirect
/// `MOSD_SETTINGS_PATH` into a temporary directory redirect the secrets with
/// it and never touch the host's `/var/lib/mica`.
fn state_dir_for(settings_path: &str) -> PathBuf {
    Path::new(settings_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(
            || PathBuf::from(identity::DEFAULT_STATE_DIR),
            Path::to_path_buf,
        )
}

/// Whether the service scan ([`scan`]) is constructed, from `dry_run` and the
/// raw value of `MOSD_SCAN` (`None` when it is unset).
///
/// One-way by construction. Production is unconditionally on; `MOSD_SCAN` is
/// read only to lift dry-run's suppression, so no value of it can take the
/// service registry away from a device that would otherwise have one.
fn service_scan_enabled(dry_run: bool, scan_override: Option<&str>) -> bool {
    !dry_run || scan_override == Some("1")
}

#[cfg(test)]
mod tests {
    use super::{
        UNKNOWN_COMMIT, commit_or_unknown, service_scan_enabled, version_line, wants_version,
    };

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    /// The two spellings, and the positive control for the negatives below.
    #[test]
    fn both_spellings_of_the_one_flag_are_recognised() {
        assert!(wants_version(argv(&["--version"])));
        assert!(wants_version(argv(&["-V"])));
    }

    /// Driven from the failing side. Every one of these must fall through into
    /// the daemon, and `-v` is the one that matters most: it is the spelling an
    /// operator reaches for, it is NOT this flag, and a loose match on it would
    /// silently stop micad from starting on any unit that passed it.
    #[test]
    fn nothing_else_is_this_flag() {
        for args in [
            vec![],
            argv(&["--help"]),
            argv(&["-h"]),
            argv(&["-v"]),
            argv(&["-VV"]),
            argv(&["--versions"]),
            argv(&["--version=1"]),
            argv(&["version"]),
            argv(&["--Version"]),
            argv(&[""]),
        ] {
            assert!(
                !wants_version(args.clone()),
                "argv {args:?} is not --version and must reach the daemon unchanged"
            );
        }
    }

    /// It is asked of the whole argv, not only of the first element -- the same
    /// place clap answers it for `mica-mqttd` and `mica-mqtt-broker`.
    #[test]
    fn the_flag_is_found_wherever_it_appears() {
        assert!(wants_version(argv(&[
            "--config",
            "/etc/x.toml",
            "--version"
        ])));
    }

    /// Absent is `unknown`, never an error -- and empty counts as absent,
    /// because `-e MICA_BUILD_COMMIT=` sets the variable to exactly that.
    #[test]
    fn a_commit_the_build_did_not_supply_reports_unknown() {
        assert_eq!(commit_or_unknown(None), UNKNOWN_COMMIT);
        assert_eq!(commit_or_unknown(Some("")), UNKNOWN_COMMIT);
        assert_eq!(commit_or_unknown(Some("   ")), UNKNOWN_COMMIT);
    }

    /// The positive control for the case above: a supplied value is passed
    /// through untouched, `-dirty` suffix and all. A helper that returned
    /// `unknown` for everything would satisfy the test above and nothing else.
    #[test]
    fn a_commit_the_build_did_supply_is_reported_verbatim() {
        assert_eq!(commit_or_unknown(Some("00b674ec0ffe")), "00b674ec0ffe");
        assert_eq!(
            commit_or_unknown(Some("00b674ec0ffe-dirty")),
            "00b674ec0ffe-dirty"
        );
    }

    /// The shape, not the values.
    ///
    /// There is deliberately no assertion here that the version equals
    /// `micad/micad/Cargo.toml`: `CARGO_PKG_VERSION` IS that file, so comparing
    /// the two here would compare a value with itself. The real comparison is
    /// made from outside, by `verify/src/smoke-pins.ts`, which parses the
    /// manifest independently and requires this binary to report what it says.
    #[test]
    fn the_version_line_names_the_binary_the_version_and_the_commit() {
        let line = version_line();
        assert!(line.starts_with("micad "), "got {line:?}");
        assert!(
            line.contains(env!("CARGO_PKG_VERSION")),
            "the crate version is missing from {line:?}"
        );
        let commit = line
            .rsplit_once(" (")
            .and_then(|(_, rest)| rest.strip_suffix(")"))
            .unwrap_or_else(|| panic!("no parenthesised commit in {line:?}"));
        assert!(!commit.is_empty(), "an empty commit in {line:?}");
        assert!(
            !commit.contains(char::is_whitespace),
            "the commit must be one token, got {commit:?}"
        );
        // One line, so a smoke runner reading the first line reads all of it.
        assert!(!line.contains('\n'), "got {line:?}");
    }

    /// The pin on the asymmetry: in production `MOSD_SCAN` is inert. A gate
    /// that read the variable symmetrically would let `MOSD_SCAN=0` — or any
    /// typo — switch the service registry off on a real device.
    #[test]
    fn production_ignores_mosd_scan_entirely() {
        for value in [
            None,
            Some("1"),
            Some("0"),
            Some(""),
            Some("true"),
            Some("no"),
        ] {
            assert!(
                service_scan_enabled(false, value),
                "production must scan whatever MOSD_SCAN holds; \
                 got service_scan=false for MOSD_SCAN={value:?}"
            );
        }
    }

    /// Dry-run on its own constructs no scan, as
    /// `tests/scan.rs::dry_run_constructs_no_scan` pins it end to end.
    #[test]
    fn dry_run_alone_constructs_no_scan() {
        for value in [None, Some("0"), Some(""), Some("true"), Some("yes")] {
            assert!(
                !service_scan_enabled(true, value),
                "dry-run must construct no scan unless MOSD_SCAN is exactly `1`; \
                 got service_scan=true for MOSD_SCAN={value:?}"
            );
        }
    }

    /// The one combination that turns the scan back on.
    #[test]
    fn dry_run_plus_mosd_scan_one_constructs_the_scan() {
        assert!(service_scan_enabled(true, Some("1")));
    }
}

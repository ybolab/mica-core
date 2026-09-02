//! Integration test: exercise `com.mos.mosd1` over a private session bus.
//!
//! Spawns a private `dbus-daemon --session` plus the `mosd` binary in
//! dry-run mode (no reconcilers, so the host is never touched), then drives
//! the interface with a zbus client.
//!
//! # This test does not skip
//!
//! `dbus-daemon` is a hard requirement, not an optional extra: without it
//! not one assertion below can be made. A test that printed
//! `skipping bus_roundtrip` and returned `Ok(())` when the binary is missing
//! would report green while asserting nothing. [`dbus_daemon`] panics
//! instead, naming the tool it could not find, the same way `tests/scan.rs`
//! does.
//!
//! CI provisions the dependency rather than opting out of the suite. The
//! `rust` job in `.github/workflows/check.yml` runs `mosd/hack/check.sh`,
//! whose `cargo nextest run --workspace` includes this file, and that job
//! installs the `dbus-daemon` package alongside the other build
//! dependencies. A runner that cannot supply it therefore goes red at the
//! install step, with a message that says which package is missing, rather
//! than quietly running one assertion fewer.

use std::future::poll_fn;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::pin::pin;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use zbus::export::futures_core::Stream;

/// Two accounts, nine fields each — the shape of a Debian `/etc/shadow`, with
/// `root` locked the way `mos-shadow-reconcile` leaves it.
const SHADOW: &str = "root:!:19000:0:99999:7:::\n\
    daemon:*:19000:0:99999:7:::\n";
const DEVICE_ID: &str = "00112233445566778899aabbccddeeff";

/// Kills the wrapped child on drop, including on panic.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Locate `dbus-daemon` (`/usr/bin/dbus-daemon` first, then `$PATH`), or
/// FAIL — never skip.
///
/// A missing bus daemon means this test cannot assert what it exists to
/// assert, and the only honest outcome for a test that cannot run is a red
/// one. See the module docs.
fn dbus_daemon() -> PathBuf {
    let fixed = PathBuf::from("/usr/bin/dbus-daemon");
    if fixed.exists() {
        return fixed;
    }
    let found = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("dbus-daemon"))
            .find(|candidate| candidate.exists())
    });
    found.unwrap_or_else(|| {
        panic!(
            "dbus-daemon was not found at /usr/bin/dbus-daemon or on PATH. This test asserts \
             real bus behaviour over a private session bus and MUST NOT skip: install it \
             (Debian/Ubuntu: the `dbus-daemon` package -- note that `dbus-bin` ships \
             dbus-send and dbus-monitor but NOT the daemon itself; Fedora: `dbus-daemon`) \
             and run it again."
        )
    })
}

/// The D-Bus error name a failed call travelled under.
///
/// A panic on any other variant: these assertions are about the name on the
/// wire, and a connection-level failure would be a different test failing.
fn error_name(err: &zbus::Error) -> &str {
    match err {
        zbus::Error::MethodError(name, _, _) => name.as_str(),
        other => panic!("expected a method error with a name, got {other:?}"),
    }
}

#[zbus::proxy(
    interface = "com.mos.mosd1",
    default_service = "com.mos.mosd",
    default_path = "/com/mos/mosd"
)]
trait Mosd {
    fn get_settings(&self, path: &str) -> zbus::Result<String>;
    fn set_settings(&self, path: &str, value_json: &str) -> zbus::Result<String>;
    fn get_task(&self, id: &str) -> zbus::Result<String>;
    fn get_state(&self, path: &str) -> zbus::Result<String>;
    fn report_health(&self, component: &str, status: &str, detail: &str) -> zbus::Result<()>;
    fn reboot(&self) -> zbus::Result<()>;
    fn power_off(&self) -> zbus::Result<()>;
    fn set_transient_root_password(&self, password: &str) -> zbus::Result<String>;
    fn install_update(&self, bundle_path: &str) -> zbus::Result<()>;
    fn get_update_state(&self) -> zbus::Result<String>;
    fn mark_update(&self, state: &str, slot: &str) -> zbus::Result<(String, String)>;
    #[zbus(signal)]
    fn settings_changed(&self, path: &str, value_json: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn task_changed(&self, task_json: &str) -> zbus::Result<()>;
}

#[tokio::test(flavor = "multi_thread")]
async fn bus_roundtrip() -> anyhow::Result<()> {
    // Private session bus; never the host system bus.
    let mut bus_child = Command::new(dbus_daemon())
        .args(["--session", "--print-address=1", "--nofork"])
        .stdout(Stdio::piped())
        .spawn()?;
    let bus_stdout = bus_child.stdout.take().expect("piped stdout");
    let _bus_guard = ChildGuard(bus_child);
    let mut address = String::new();
    BufReader::new(bus_stdout).read_line(&mut address)?;
    let address = address.trim().to_string();
    anyhow::ensure!(!address.is_empty(), "dbus-daemon printed no address");

    let dir = tempfile::tempdir()?;
    let settings_path = dir.path().join("settings.toml");
    let mut seeded = mosd_settings::Settings::default();
    seeded.provisioning.device_id = Some(DEVICE_ID.to_string());
    mosd_settings::Store::new(&settings_path).save(&seeded)?;
    // The daemon must never be pointed at the host's /etc/shadow, so the
    // transient-password method gets a throwaway file of its own.
    let shadow_path = dir.path().join("shadow");
    let marker_path = dir.path().join("transient-root-password");
    std::fs::write(&shadow_path, SHADOW)?;
    // The update workspace: installs are admitted only from its verified/,
    // so the daemon is pointed at one inside the tempdir (the client's own
    // override variable, which mosd forwards to it).
    let update_root = dir.path().join("updates");
    std::fs::create_dir_all(update_root.join("verified"))?;
    // MOSD_DRY_RUN=1 is a hard safety requirement: production reconcilers
    // must never be constructed in tests.
    let _mosd_guard = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_mosd"))
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env("MOSD_BUS", "session")
            .env("MOSD_DRY_RUN", "1")
            .env("MOSD_SETTINGS_PATH", &settings_path)
            .env("MOSD_SHADOW_PATH", &shadow_path)
            .env("RAUC_UPDATE_ROOT", &update_root)
            .spawn()?,
    );

    let connection = zbus::connection::Builder::address(address.as_str())?
        .build()
        .await?;
    let proxy = MosdProxy::new(&connection).await?;

    // Wait for the daemon to claim the well-known name.
    let defaults = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match proxy.get_settings("").await {
                Ok(json) => break json,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
    .await?;
    let defaults: serde_json::Value = serde_json::from_str(&defaults)?;
    assert_eq!(defaults["hostname"], "mos");
    assert_eq!(defaults["schema_version"], mosd_settings::SCHEMA_VERSION);

    let mut changed = proxy.receive_settings_changed().await?;
    let mut task_changed = proxy.receive_task_changed().await?;
    let task_id = proxy.set_settings("hostname", "\"unit-test-host\"").await?;

    let signal = tokio::time::timeout(Duration::from_secs(10), async {
        let mut changed = pin!(&mut changed);
        poll_fn(|cx| changed.as_mut().poll_next(cx)).await
    })
    .await?
    .expect("signal stream ended");
    let args = signal.args()?;
    assert_eq!(args.path(), &"hostname");
    assert_eq!(args.value_json(), &"\"unit-test-host\"");

    let task_signal = tokio::time::timeout(Duration::from_secs(10), async {
        let mut task_changed = pin!(&mut task_changed);
        poll_fn(|cx| task_changed.as_mut().poll_next(cx)).await
    })
    .await?
    .expect("TaskChanged stream ended");
    let task: serde_json::Value = serde_json::from_str(task_signal.args()?.task_json())?;
    assert_eq!(task["id"], task_id);
    assert_eq!(task["operation"], "settings-write");
    assert_eq!(task["dotPath"], "hostname");
    assert_eq!(task["status"], "queued");

    let finished = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let task: serde_json::Value = serde_json::from_str(&proxy.get_task(&task_id).await?)?;
            if task["status"] == "finished" {
                break anyhow::Ok(task);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(finished["outcome"], "succeeded");
    assert_eq!(finished["foldedCount"], 0);

    let hostname = proxy.get_settings("hostname").await?;
    assert_eq!(hostname, "\"unit-test-host\"");

    let persisted = std::fs::read_to_string(&settings_path)?;
    assert!(
        persisted.contains("unit-test-host"),
        "settings.toml should contain the new hostname, got:\n{persisted}"
    );

    let state = proxy.get_state("").await?;
    let state: serde_json::Value = serde_json::from_str(&state)?;
    assert_eq!(state["dry_run"], true);

    // Uptime is a live-state fact served over the bus: a bare JSON number of
    // whole seconds at `uptime`, present in the whole tree too, so apid needs
    // no `/proc` reader of its own.
    let uptime = proxy.get_state("uptime").await?;
    let uptime: u64 = serde_json::from_str(&uptime)?;
    assert!(
        state["uptime"]
            .as_u64()
            .is_some_and(|whole| uptime >= whole),
        "the whole tree must carry uptime, no newer than a later read: {state}"
    );

    // ReportHealth: the health gate's /var pressure report lands in the
    // live-state tree and reads back through the existing GetState call.
    proxy
        .report_health("var", "degraded", "/var at 91% of capacity (threshold 85%)")
        .await?;
    let health = proxy.get_state("health.var").await?;
    let health: serde_json::Value = serde_json::from_str(&health)?;
    assert_eq!(health["status"], "degraded");
    assert_eq!(health["detail"], "/var at 91% of capacity (threshold 85%)");
    assert!(proxy.report_health("", "ok", "").await.is_err());

    // Power actions. Assert the safety precondition FIRST: the daemon under
    // test must be in dry-run, where its PowerControl is the no-op one and
    // `Systemd` — the only thing that can reach the host system bus — is never
    // constructed. If someone drops MOSD_DRY_RUN from the spawn above, this
    // fails before a single power method is invoked rather than after.
    let dry_run = proxy.get_state("dry_run").await?;
    assert_eq!(
        dry_run, "true",
        "refusing to invoke power methods against a daemon that is not in dry-run"
    );

    // What is asserted below is the wiring — that D-Bus member `Reboot` runs
    // the reboot handler and `PowerOff` runs the power-off handler, each
    // recording itself in the live-state tree before acting.
    proxy.reboot().await?;
    let power = proxy.get_state("power").await?;
    let power: serde_json::Value = serde_json::from_str(&power)?;
    assert_eq!(power["last_action"], "reboot");
    assert!(
        power["requested_by"]
            .as_str()
            .is_some_and(|sender| sender.starts_with(':')),
        "requested_by should be the caller's unique bus name, got {power}"
    );

    proxy.power_off().await?;
    let power = proxy.get_state("power").await?;
    let power: serde_json::Value = serde_json::from_str(&power)?;
    assert_eq!(power["last_action"], "power_off");

    // Settings are untouched by power actions: they are actions, not state.
    let after = proxy.get_settings("hostname").await?;
    assert_eq!(after, "\"unit-test-host\"");

    // SetTransientRootPassword. What is asserted first is the MEMBER NAME on
    // the real interface: zbus renames a snake_case method to PascalCase, and
    // a client that guesses wrong gets UnknownMethod, not a compile error. So
    // read the interface back out of the daemon instead of trusting the rename.
    let introspectable = zbus::fdo::IntrospectableProxy::builder(&connection)
        .destination("com.mos.mosd")?
        .path("/com/mos/mosd")?
        .build()
        .await?;
    let xml = introspectable.introspect().await?;
    assert!(
        !xml.contains("<method name=\"GetDeviceId\">"),
        "device identity is runtime configuration for mqttd, not a mosd D-Bus member:\n{xml}"
    );
    let root_xml = zbus::fdo::IntrospectableProxy::builder(&connection)
        .destination("com.mos.mosd")?
        .path("/")?
        .build()
        .await?
        .introspect()
        .await?;
    assert!(
        !root_xml.contains("com.mos.Item1"),
        "system management must not expose an application item tree at /:\n{root_xml}"
    );
    let opening = "<method name=\"SetTransientRootPassword\">";
    let start = xml
        .find(opening)
        .unwrap_or_else(|| panic!("no SetTransientRootPassword on com.mos.mosd1:\n{xml}"));
    let body = &xml[start + opening.len()..];
    let body = &body[..body
        .find("</method>")
        .expect("the method element must close")];
    assert_eq!(
        body.matches("direction=\"in\"").count(),
        1,
        "SetTransientRootPassword takes exactly one input, got:\n{body}"
    );
    assert!(
        body.contains("type=\"s\"") && body.contains("direction=\"in\""),
        "its one argument must be an `in` string, got:\n{body}"
    );
    assert!(
        body.contains("direction=\"out\""),
        "the returned task id must be an out argument, got:\n{body}"
    );
    assert!(
        !xml.contains("set_transient_root_password"),
        "the snake_case name must NOT be what a client sees:\n{xml}"
    );

    // Then the behaviour, over the bus, against the daemon's own shadow file.
    let password_task = proxy
        .set_transient_root_password("correct horse battery")
        .await?;
    let after = std::fs::read_to_string(&shadow_path)?;
    let root_hash = after
        .lines()
        .find(|line| line.starts_with("root:"))
        .and_then(|line| line.split(':').nth(1))
        .expect("root entry");
    assert!(
        bcrypt::verify("correct horse battery", root_hash)?,
        "the shadow root hash must verify against the password that was set"
    );
    assert_eq!(
        std::fs::read_to_string(&marker_path)?,
        format!("{root_hash}\n"),
        "the marker beside the shadow file must repeat the stored hash exactly"
    );
    assert_eq!(
        after.lines().skip(1).collect::<Vec<_>>(),
        SHADOW.lines().skip(1).collect::<Vec<_>>(),
        "every other account must survive byte-for-byte"
    );
    let task_json = proxy.get_task(&password_task).await?;
    assert!(
        !task_json.contains("correct horse battery"),
        "plaintext password entered the task record: {task_json}"
    );
    let task: serde_json::Value = serde_json::from_str(&task_json)?;
    assert_eq!(task["operation"], "transient-password");
    assert_eq!(task["dotPath"], "access.ssh");

    // A rejected password is an error, changes nothing, and does not echo the
    // password back to the caller.
    let err = proxy
        .set_transient_root_password("short12")
        .await
        .expect_err("seven bytes is below the floor");
    assert!(!err.to_string().contains("short12"), "leaked: {err}");
    assert_eq!(
        std::fs::read_to_string(&shadow_path)?,
        after,
        "a rejected password must leave the shadow file exactly as it was"
    );

    // A password is never a setting: the tree is untouched and nothing about it
    // reached the persisted file.
    assert_eq!(proxy.get_settings("hostname").await?, "\"unit-test-host\"");
    let persisted = std::fs::read_to_string(&settings_path)?;
    assert!(
        !persisted.contains("correct horse"),
        "the password reached settings.toml:\n{persisted}"
    );

    // Three distinct failures travel under three distinct error names, so a
    // caller can separate a missing path, a read-only path and a bad value
    // without parsing message prose. The names are spelled out here rather
    // than imported: they are the bus contract, and a test that borrowed the
    // constant from the code under test would follow it wherever it moved.
    let err = proxy
        .get_settings("no.such.path")
        .await
        .expect_err("a missing dot-path must be an error");
    assert_eq!(error_name(&err), "com.mos.mosd1.Error.NotFound");
    let err = proxy
        .set_settings("schema_version", "2")
        .await
        .expect_err("a read-only dot-path must refuse the write");
    assert_eq!(error_name(&err), "com.mos.mosd1.Error.ReadOnly");
    let err = proxy
        .set_settings("hostname", "42")
        .await
        .expect_err("a value the typed tree rejects must be an error");
    assert_eq!(error_name(&err), "org.freedesktop.DBus.Error.InvalidArgs");
    assert!(proxy.set_settings("hostname", "not json").await.is_err());

    // Update orchestration, against the dry-run RAUC client — the
    // same guarantee as the power methods above: MOSD_DRY_RUN=1 means the
    // production client was never constructed, so nothing here can install a
    // bundle on, or mark a slot of, the build host.
    //
    // The MEMBER NAMES first, read back out of the daemon for the same reason
    // SetTransientRootPassword's was: zbus renames snake_case to PascalCase,
    // and a client that guesses wrong gets UnknownMethod at runtime, not a
    // compile error.
    let xml = introspectable.introspect().await?;
    for member in ["InstallUpdate", "GetUpdateState", "MarkUpdate"] {
        assert!(
            xml.contains(&format!("<method name=\"{member}\">")),
            "no {member} on com.mos.mosd1:\n{xml}"
        );
    }
    for leaked in ["install_update", "get_update_state", "mark_update"] {
        assert!(
            !xml.contains(leaked),
            "the snake_case name must NOT be what a client sees:\n{xml}"
        );
    }

    // GetUpdateState queries and records: the dry-run client reports an idle
    // installer with no slots, and the same entry lands in the state tree.
    let update = proxy.get_update_state().await?;
    let update: serde_json::Value = serde_json::from_str(&update)?;
    assert_eq!(update["operation"], "idle");
    assert_eq!(update["pending_not_confirmed"], false);
    assert_eq!(update["slots"], serde_json::json!({}));
    let recorded = proxy.get_state("update").await?;
    let recorded: serde_json::Value = serde_json::from_str(&recorded)?;
    assert_eq!(recorded, update);

    // InstallUpdate validates the path over the bus...
    assert!(
        proxy.install_update("relative.raucb").await.is_err(),
        "a relative bundle path must be refused"
    );
    assert!(
        proxy
            .install_update(dir.path().join("gone.raucb").to_str().expect("utf-8"))
            .await
            .is_err(),
        "a missing bundle must be refused"
    );
    // ...refuses a regular file that is not inside the workspace's verified/
    // (only a verified bundle is handed to RAUC, whoever names the path)...
    let outside = dir.path().join("outside.raucb");
    std::fs::write(&outside, b"bundle bytes")?;
    let err = proxy
        .install_update(outside.to_str().expect("utf-8"))
        .await
        .expect_err("a bundle outside verified/ must be refused");
    assert_eq!(error_name(&err), "org.freedesktop.DBus.Error.InvalidArgs");
    // ...and a valid request is admitted, runs in the background, and records
    // its outcome where GetState can see it.
    let bundle_path = update_root.join("verified").join("ok.raucb");
    std::fs::write(&bundle_path, b"bundle bytes")?;
    proxy
        .install_update(bundle_path.to_str().expect("utf-8"))
        .await?;
    let install = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(install) = proxy.get_state("update.install").await {
                let install: serde_json::Value =
                    serde_json::from_str(&install).expect("install entry is JSON");
                if install["status"] == "done" {
                    break install;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    assert_eq!(install["bundle"], bundle_path.to_str().expect("utf-8"));
    assert!(
        install["requested_by"]
            .as_str()
            .is_some_and(|sender| sender.starts_with(':')),
        "requested_by should be the caller's unique bus name, got {install}"
    );

    // MarkUpdate: the offered vocabulary only, validated before RAUC.
    assert!(
        proxy.mark_update("active", "other").await.is_err(),
        "activation is not offered on this surface"
    );
    assert!(
        proxy.mark_update("good", "rootfs.0").await.is_err(),
        "slots are addressed as booted/other only"
    );
    let (_slot_name, message) = proxy.mark_update("good", "booted").await?;
    assert!(message.contains("good"), "message: {message}");
    let last_mark = proxy.get_state("update.last_mark").await?;
    let last_mark: serde_json::Value = serde_json::from_str(&last_mark)?;
    assert_eq!(last_mark["state"], "good");
    assert_eq!(last_mark["slot"], "booted");

    Ok(())
}

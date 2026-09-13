//! Integration test: micad's service registry against a REAL private session
//! bus (`docs/design/bus.md`, service registry).
//!
//! Every test here spawns a private `dbus-daemon --session` and the `micad`
//! binary, then claims `com.mica.*` names from this process with real zbus
//! connections and reads back what the registry made of them. Nothing here
//! stubs the bus: the point of the exercise is that the daemon's own
//! `NameOwnerChanged` subscription, its probe and its conformance judgement
//! all work against a bus that behaves like the one on the device.
//!
//! # This test does not skip
//!
//! A test that skips itself into a green tick when `dbus-daemon` is missing
//! reports success while asserting nothing, which is worse than no test at
//! all. [`dbus_daemon`] panics instead, naming the tool it could not find.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use zbus::zvariant::{OwnedValue, Value};

/// Two accounts, nine fields each — the shape of a Debian `/etc/shadow`. The
/// daemon under test never writes a password here; the file exists so that
/// nothing can point it at the host's.
const SHADOW: &str = "root:!:19000:0:99999:7:::\n\
    daemon:*:19000:0:99999:7:::\n";

/// The seven mandatory paths (`docs/design/bus.md`, service registry), restated here on
/// purpose: a test that imported the daemon's own list could not notice the
/// daemon dropping one from it.
const MANDATORY_PATHS: [&str; 7] = [
    "/Mgmt/ProcessName",
    "/Mgmt/ProcessVersion",
    "/Mgmt/Connection",
    "/DeviceInstance",
    "/ProductId",
    "/ProductName",
    "/Connected",
];

/// How long any wait-for-the-registry loop is given before it fails.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Kills the wrapped child on drop, including on panic.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Locate `dbus-daemon`, or FAIL — never skip.
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
             real bus behaviour and MUST NOT skip: install dbus-daemon (Debian/Ubuntu: \
             `dbus-bin` or `dbus`; Fedora: `dbus-daemon`) and run it again."
        )
    })
}

#[zbus::proxy(
    interface = "com.mica.micad1",
    default_service = "com.mica.micad",
    default_path = "/com/mos/micad"
)]
trait Mosd {
    fn get_settings(&self, path: &str) -> zbus::Result<String>;
    fn get_state(&self, path: &str) -> zbus::Result<String>;
    fn forget_service(&self, bus_name: &str) -> zbus::Result<()>;
}

/// A fake `com.mica.*` service: it answers `com.mica.Item1.GetItems` with
/// whatever item map it was built with, and nothing else.
struct FakeItems {
    items: HashMap<String, HashMap<String, OwnedValue>>,
}

#[zbus::interface(name = "com.mica.Item1")]
impl FakeItems {
    async fn get_items(&self) -> HashMap<String, HashMap<String, OwnedValue>> {
        self.items.clone()
    }
}

/// A fake that owns a `com.mica.*` name and answers something that is NOT
/// `com.mica.Item1` — the common shape of a service that is on the bus for its
/// own reasons, and the one that makes the probe fail immediately with
/// `UnknownInterface` rather than by timing out.
struct FakeOther;

#[zbus::interface(name = "com.example.NotAnItem1")]
impl FakeOther {
    async fn ping(&self) -> &str {
        "pong"
    }
}

/// One item's attribute dict, as `docs/design/bus.md` shapes it.
fn attrs(value: Value<'static>) -> HashMap<String, OwnedValue> {
    HashMap::from([
        (
            "value".to_string(),
            OwnedValue::try_from(value).expect("an item value"),
        ),
        (
            "writable".to_string(),
            OwnedValue::try_from(Value::from(false)).expect("a writable flag"),
        ),
    ])
}

/// A fully conforming item set: all seven mandatory paths, with `instance` as
/// `/DeviceInstance`.
fn conforming(instance: i64) -> HashMap<String, HashMap<String, OwnedValue>> {
    let mut items = HashMap::new();
    for path in MANDATORY_PATHS {
        let value = match path {
            "/DeviceInstance" => Value::from(instance),
            "/ProductId" => Value::from(0xA1B2_i64),
            "/Connected" => Value::from(1_i64),
            _ => Value::from("fake"),
        };
        items.insert(path.to_string(), attrs(value));
    }
    items
}

/// The same, minus `/DeviceInstance` — a service that publishes no instance.
fn conforming_without_instance() -> HashMap<String, HashMap<String, OwnedValue>> {
    let mut items = conforming(0);
    items.remove("/DeviceInstance");
    items
}

/// A private bus, a `micad` on it, and a client connection to drive it with.
struct Harness {
    address: String,
    connection: zbus::Connection,
    /// The daemon's log. `tracing_subscriber::fmt` writes to STDOUT, so that
    /// is the stream captured here — piping stderr would collect an empty
    /// string and make every log assertion below vacuous.
    log: Arc<Mutex<String>>,
    _bus: ChildGuard,
    _mosd: ChildGuard,
    _dir: tempfile::TempDir,
}

impl Harness {
    /// Start a private session bus and a `micad` with the service scan running,
    /// and wait until the daemon has claimed its name.
    async fn start() -> Self {
        Self::start_with_scan(true).await
    }

    /// The same, with `MOSD_SCAN` left unset so that dry-run's default decides
    /// — which is what the daemon does in production under dry-run.
    async fn start_without_scan() -> Self {
        Self::start_with_scan(false).await
    }

    async fn start_with_scan(scan: bool) -> Self {
        let mut bus_child = Command::new(dbus_daemon())
            .args(["--session", "--print-address=1", "--nofork"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn dbus-daemon");
        let bus_stdout = bus_child.stdout.take().expect("piped stdout");
        let bus = ChildGuard(bus_child);
        let mut address = String::new();
        BufReader::new(bus_stdout)
            .read_line(&mut address)
            .expect("read the bus address");
        let address = address.trim().to_string();
        assert!(!address.is_empty(), "dbus-daemon printed no address");

        let dir = tempfile::tempdir().expect("tempdir");
        let settings_path = dir.path().join("settings.toml");
        // The `/mos/config/` namespace micad reads its configuration from. It
        // must exist before the daemon starts: an absent namespace is the DATA
        // medium being gone, and micad refuses to start rather than render a
        // configuration nobody chose (PLAN-070 §5.2.6).
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create the config namespace");
        let shadow_path = dir.path().join("shadow");
        std::fs::write(&shadow_path, SHADOW).expect("seed shadow");

        // MOSD_DRY_RUN=1 is a hard safety requirement: production reconcilers
        // and the systemd power control must never be constructed in a test.
        // MOSD_SCAN=1 turns the one thing under test back on — the scan is
        // passive and speaks only to the private bus named by
        // DBUS_SESSION_BUS_ADDRESS, so it reaches nothing on the host.
        let mut command = Command::new(env!("CARGO_BIN_EXE_micad"));
        command
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env("MOSD_BUS", "session")
            .env("MOSD_DRY_RUN", "1")
            .env("MOSD_SETTINGS_PATH", &settings_path)
            .env("MOSD_CONFIG_DIR", &config_dir)
            .env("MOSD_SHADOW_PATH", &shadow_path)
            .stdout(Stdio::piped());
        if scan {
            command.env("MOSD_SCAN", "1");
        }
        let mut micad_child = command.spawn().expect("spawn micad");
        // Drained on a thread, both so the daemon's log can be asserted on and
        // so a full pipe can never block the daemon mid-test.
        let log = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&log);
        let micad_log = micad_child.stdout.take().expect("piped stdout");
        std::thread::spawn(move || {
            let mut reader = BufReader::new(micad_log);
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
                sink.lock().expect("log lock").push_str(&line);
                line.clear();
            }
        });
        let micad = ChildGuard(micad_child);

        let connection = zbus::connection::Builder::address(address.as_str())
            .expect("bus address")
            .build()
            .await
            .expect("connect to the private bus");
        let harness = Self {
            address,
            connection,
            log,
            _bus: bus,
            _mosd: micad,
            _dir: dir,
        };

        let proxy = harness.proxy().await;
        tokio::time::timeout(TIMEOUT, async {
            while proxy.get_settings("").await.is_err() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("micad never claimed com.mica.micad on the private bus");

        // The safety precondition, asserted before anything else runs: the
        // daemon under test is in dry-run, so no reconciler and no systemd
        // power control was ever constructed. If someone drops MOSD_DRY_RUN
        // from the spawn above, every test here fails at its first line.
        assert_eq!(
            proxy.get_state("dry_run").await.expect("dry_run state"),
            "true",
            "refusing to run against a daemon that is not in dry-run"
        );
        harness
    }

    async fn proxy(&self) -> MosdProxy<'_> {
        MosdProxy::new(&self.connection)
            .await
            .expect("com.mica.micad1 proxy")
    }

    /// Claim `name` on the private bus with a service answering `GetItems`
    /// with `items`. The returned connection owns the name until it is
    /// released or dropped.
    async fn fake(&self, name: &str, items: HashMap<String, HashMap<String, OwnedValue>>) -> Fake {
        let connection = zbus::connection::Builder::address(self.address.as_str())
            .expect("bus address")
            .serve_at("/", FakeItems { items })
            .expect("serve com.mica.Item1")
            .name(name)
            .expect("a well-known name")
            .build()
            .await
            .unwrap_or_else(|err| panic!("claim {name}: {err}"));
        Fake {
            name: name.to_string(),
            connection,
        }
    }

    /// Claim `name` with a connection that serves NOTHING: the name is owned,
    /// and the probe gets no answer at all.
    async fn fake_that_answers_nothing(&self, name: &str) -> Fake {
        let connection = zbus::connection::Builder::address(self.address.as_str())
            .expect("bus address")
            .name(name)
            .expect("a well-known name")
            .build()
            .await
            .unwrap_or_else(|err| panic!("claim {name}: {err}"));
        Fake {
            name: name.to_string(),
            connection,
        }
    }

    /// Claim `name` with a connection serving an interface that is not
    /// `com.mica.Item1`, so the probe is refused rather than ignored.
    async fn fake_with_another_interface(&self, name: &str) -> Fake {
        let connection = zbus::connection::Builder::address(self.address.as_str())
            .expect("bus address")
            .serve_at("/", FakeOther)
            .expect("serve the other interface")
            .name(name)
            .expect("a well-known name")
            .build()
            .await
            .unwrap_or_else(|err| panic!("claim {name}: {err}"));
        Fake {
            name: name.to_string(),
            connection,
        }
    }

    /// The registry as the live-state tree currently carries it.
    async fn services(&self) -> Json {
        let proxy = self.proxy().await;
        match proxy.get_state("services").await {
            Ok(json) => serde_json::from_str(&json).expect("the registry is JSON"),
            Err(_) => Json::Null,
        }
    }

    /// Poll the registry until `done` accepts it; panic with the last
    /// snapshot when it never does.
    async fn wait_for(&self, what: &str, done: impl Fn(&Json) -> bool) -> Json {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let services = self.services().await;
            if done(&services) {
                return services;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the registry was:\n{services:#}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Poll the daemon's log until it carries every fragment in `fragments`.
    async fn wait_for_log(&self, fragments: &[&str]) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let log = self.log.lock().expect("log lock").clone();
            if fragments.iter().all(|fragment| log.contains(fragment)) {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {fragments:?} in micad's log; the log was:\n{log}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// A fake service holding a well-known name.
struct Fake {
    name: String,
    connection: zbus::Connection,
}

impl Fake {
    /// Give the name up, the way a service that exits does.
    async fn vanish(self) {
        self.connection
            .release_name(self.name.as_str())
            .await
            .expect("release the name");
    }
}

/// The entry for `name`, or a failure that prints the whole registry.
fn entry<'a>(services: &'a Json, name: &str) -> &'a Json {
    services
        .get(name)
        .unwrap_or_else(|| panic!("no registry entry for {name}; the registry was:\n{services:#}"))
}

/// Whether the registry carries an entry for `name`.
fn has(services: &Json, name: &str) -> bool {
    services.get(name).is_some()
}

/// 1. A conforming service appears with its direct-name class and an empty
///    conformance object.
#[tokio::test(flavor = "multi_thread")]
async fn a_conforming_service_appears_with_no_conformance_gaps() {
    let harness = Harness::start().await;
    let name = "com.mica.sensor.fake";
    let _fake = harness.fake(name, conforming(3)).await;

    let services = harness.wait_for(name, |services| has(services, name)).await;
    let entry = entry(&services, name);

    assert_eq!(entry["name"], name);
    assert_eq!(
        entry["class"], "sensor",
        "the class of com.mica.sensor.fake is `sensor`; got {entry:#}"
    );
    assert_eq!(entry["connected"], true, "{entry:#}");
    assert_eq!(entry["instance"], 3, "{entry:#}");
    assert_eq!(
        entry["conformance"],
        serde_json::json!({}),
        "a service publishing all seven mandatory paths has no conformance gaps; got {entry:#}"
    );
    assert_eq!(entry["instance_collision"], false, "{entry:#}");

    // The WHOLE entry, field for field. Exact equality is the assertion that
    // the registry publishes the enumerated fields and NOTHING else: the fake
    // above answers `GetItems` with items of its own, and a registry that
    // dumped what a third-party service publishes would put whatever a service
    // chose to put in its items — secrets included — onto the bus and into the
    // logs. An extra key here fails this test.
    assert_eq!(
        entry,
        &serde_json::json!({
            "name": "com.mica.sensor.fake",
            "class": "sensor",
            "connected": true,
            "instance": 3,
            "conformance": {},
            "instance_collision": false,
        }),
        "the published entry must carry exactly the enumerated fields"
    );

    // micad keeps its own name out of the registry: the registry is its view of
    // the other services on the bus.
    assert!(
        !has(&services, "com.mica.micad"),
        "micad must not register itself; got {services:#}"
    );
}

/// 2. A vanished service is retained as disconnected; `ForgetService` drops it,
///    and refuses to drop one that is still connected.
#[tokio::test(flavor = "multi_thread")]
async fn a_vanished_service_is_retained_and_only_then_forgettable() {
    let harness = Harness::start().await;
    let name = "com.mica.sensor.fake";
    let fake = harness.fake(name, conforming(1)).await;
    harness.wait_for(name, |services| has(services, name)).await;

    // The MEMBER NAME on the real interface, read back out of the daemon
    // rather than trusted: zbus renames a snake_case method to PascalCase, and
    // a client that guesses wrong gets UnknownMethod rather than a compile
    // error — including the client below, which is generated by the same
    // rename and so cannot detect a mismatch on its own.
    let introspectable = zbus::fdo::IntrospectableProxy::builder(&harness.connection)
        .destination("com.mica.micad")
        .expect("destination")
        .path("/com/mos/micad")
        .expect("path")
        .build()
        .await
        .expect("introspectable proxy");
    let xml = introspectable.introspect().await.expect("introspect");
    let opening = "<method name=\"ForgetService\">";
    let start = xml
        .find(opening)
        .unwrap_or_else(|| panic!("no ForgetService on com.mica.micad1:\n{xml}"));
    let body = &xml[start + opening.len()..];
    let body = &body[..body
        .find("</method>")
        .expect("the method element must close")];
    assert_eq!(
        body.matches("<arg").count(),
        1,
        "ForgetService takes exactly one argument, got:\n{body}"
    );
    assert!(
        body.contains("type=\"s\"") && body.contains("direction=\"in\""),
        "its one argument must be an `in` string — the bus name to forget, got:\n{body}"
    );

    // Connected: the removal is refused, and says why.
    let proxy = harness.proxy().await;
    let refused = proxy
        .forget_service(name)
        .await
        .expect_err("a connected service must not be forgettable");
    assert!(
        refused.to_string().contains("still connected"),
        "the refusal must name its reason: {refused}"
    );
    assert!(
        has(&harness.services().await, name),
        "a refused ForgetService must change nothing"
    );

    fake.vanish().await;

    // Retained, not evicted: an operator has to be able to see that a service
    // they installed has stopped appearing.
    let services = harness
        .wait_for("the entry to go disconnected", |services| {
            services.get(name).is_some_and(|e| e["connected"] == false)
        })
        .await;
    let entry = entry(&services, name);
    assert_eq!(entry["class"], "sensor", "{entry:#}");
    assert_eq!(entry["instance"], 1, "the cached facts survive: {entry:#}");

    // Disconnected: the removal is accepted, and the entry is gone.
    proxy
        .forget_service(name)
        .await
        .expect("a disconnected service can be forgotten");
    let services = harness.services().await;
    assert!(
        !has(&services, name),
        "ForgetService must drop the entry; got {services:#}"
    );
    assert!(
        proxy.forget_service(name).await.is_err(),
        "forgetting what is already gone is an error, not a silent success"
    );
}

/// 3. A service that answers no `com.mica.Item1` is still registered — warned
///    about, never refused. Both shapes of "no Item1" are here: the service
///    that refuses the call, and the one that never answers it.
#[tokio::test(flavor = "multi_thread")]
async fn a_service_without_item1_is_registered_and_warned_about() {
    let harness = Harness::start().await;
    let silent = "com.mica.sensor.silent";
    let other = "com.mica.sensor.other";
    let _silent = harness.fake_that_answers_nothing(silent).await;
    let _other = harness.fake_with_another_interface(other).await;

    let services = harness
        .wait_for("both non-conforming services", |services| {
            has(services, silent) && has(services, other)
        })
        .await;

    for name in [silent, other] {
        let entry = entry(&services, name);
        assert_eq!(
            entry["conformance"]["item1"], false,
            "a service answering no com.mica.Item1 must be published saying so — that fact IS \
             the registry entry, and an operator who cannot see it concludes the bridge is \
             broken; got {entry:#}"
        );
        assert_eq!(entry["connected"], true, "{entry:#}");
        assert_eq!(entry["class"], "sensor", "{entry:#}");
        assert_eq!(
            entry["instance"], 0,
            "no /DeviceInstance means the fallback instance: {entry:#}"
        );
        let missing = entry["conformance"]["missing_paths"]
            .as_array()
            .unwrap_or_else(|| panic!("missing_paths must be a list; got {entry:#}"));
        assert_eq!(
            missing.len(),
            MANDATORY_PATHS.len(),
            "a service that answers nothing publishes none of the seven: {entry:#}"
        );
    }

    // Warned about exactly once each, naming the service — and nothing was
    // refused: the entries above are the proof of that.
    let log = harness
        .wait_for_log(&[silent, other, "does not conform"])
        .await;
    for name in [silent, other] {
        let warnings = log
            .lines()
            .filter(|line| line.contains("does not conform") && line.contains(name))
            .count();
        assert_eq!(
            warnings, 1,
            "exactly one warning per non-conforming service, got {warnings} for {name} in:\n{log}"
        );
    }
}

/// 4. No `/DeviceInstance` — the Venus fallback, named as the gap it is.
#[tokio::test(flavor = "multi_thread")]
async fn a_service_without_a_device_instance_falls_back_to_zero() {
    let harness = Harness::start().await;
    let name = "com.mica.meter.noinstance";
    let _fake = harness.fake(name, conforming_without_instance()).await;

    let services = harness.wait_for(name, |services| has(services, name)).await;
    let entry = entry(&services, name);

    assert_eq!(
        entry["instance"], 0,
        "a service with no /DeviceInstance falls back to instance 0; got {entry:#}"
    );
    assert_eq!(
        entry["conformance"]["device_instance"], false,
        "and the fallback is recorded as the gap it is, or an operator reads instance 0 as a \
         number the service chose; got {entry:#}"
    );
    assert_eq!(
        entry["conformance"]["missing_paths"],
        serde_json::json!(["/DeviceInstance"]),
        "{entry:#}"
    );
    assert_eq!(entry["class"], "meter", "{entry:#}");
}

/// 5. Two services of one class both falling back to instance 0: BOTH are
///    marked, and neither is dropped or shadowed by the other.
#[tokio::test(flavor = "multi_thread")]
async fn two_instanceless_services_of_a_class_both_carry_the_collision() {
    let harness = Harness::start().await;
    let one = "com.mica.sensor.one";
    let two = "com.mica.sensor.two";
    let _first = harness.fake(one, conforming_without_instance()).await;
    let _second = harness.fake(two, conforming_without_instance()).await;

    let services = harness
        .wait_for("both services", |services| {
            has(services, one) && has(services, two)
        })
        .await;

    for name in [one, two] {
        let entry = entry(&services, name);
        assert_eq!(entry["instance"], 0, "{entry:#}");
        assert_eq!(entry["class"], "sensor", "{entry:#}");
        assert_eq!(
            entry["instance_collision"], true,
            "{name} shares class `sensor` and instance 0 with another connected service, so it \
             must be marked as colliding — both sides, since neither is more at fault than the \
             other; the registry was:\n{services:#}"
        );
        assert_eq!(
            entry["connected"], true,
            "neither service may be dropped or shadowed by the collision; got {entry:#}"
        );
    }
}

/// 6. `ext` is an ordinary direct class, not a privileged namespace.
#[tokio::test(flavor = "multi_thread")]
async fn ext_is_an_ordinary_direct_class() {
    let harness = Harness::start().await;
    let name = "com.mica.ext";
    let _fake = harness.fake(name, conforming(9)).await;

    let services = harness.wait_for(name, |services| has(services, name)).await;
    let entry = entry(&services, name);

    assert_eq!(entry["class"], "ext", "{entry:#}");
    assert_eq!(
        entry["conformance"],
        serde_json::json!({}),
        "the fake publishes all seven mandatory paths; got {entry:#}"
    );
    assert_eq!(entry["instance"], 9, "{entry:#}");
}

/// The dry-run gate: with `MOSD_SCAN` unset, a dry-run daemon constructs no
/// scan at all — no subscription, no probe, and no registry to forget from.
///
/// The negative that matters is the last one, and it is the deterministic one:
/// `ForgetService` answers "no service registry" only when the daemon holds
/// none, and the registry and the scan task are gated on the very same
/// `Option`, so a daemon that answers this way cannot be running a scan.
#[tokio::test(flavor = "multi_thread")]
async fn dry_run_constructs_no_scan() {
    let harness = Harness::start_without_scan().await;
    let name = "com.mica.sensor.unwatched";
    let _fake = harness.fake(name, conforming(2)).await;

    let refused = harness
        .proxy()
        .await
        .forget_service(name)
        .await
        .expect_err("a daemon with no scan has no registry to forget from");
    assert!(
        refused.to_string().contains("no service registry"),
        "the refusal must say the registry is absent, not that the name is: {refused}"
    );

    // And nothing was ever published: a service came up on this bus and the
    // live-state tree never grew a `services` key for it.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let services = harness.services().await;
    assert_eq!(
        services,
        Json::Null,
        "a dry-run daemon must publish no service registry at all; got {services:#}"
    );
}

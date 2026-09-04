//! the proxy's `SettingsChanged` member, received over a real
//! private bus from a fake mosd that emits it exactly as the real one does —
//! and the watcher semantics on top: the subscription going live marks
//! the cache synchronised, an access-touching change invalidates it, an
//! unrelated change does not, and the stream lapsing drops the cache back to
//! direct reads.
//!
//! Same harness rules as [`super::power_bus`]: a private `dbus-daemon
//! --session`, never the host's bus, and a missing daemon panics rather than
//! skips — a run that returned early would report green while asserting
//! nothing.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use zbus::object_server::SignalEmitter;

use crate::access_cache::AccessCache;
use crate::bus_client::{self, BusSettings};
use crate::settings_api::SettingsApi;
use crate::task_registry::TaskRegistry;

/// Kills the wrapped child on drop, including on panic.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Locate `dbus-daemon` (`/usr/bin/dbus-daemon` first, then `$PATH`), or
/// FAIL — never skip. See the module docs, and `power_bus`'s copy.
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
             (Debian/Ubuntu: the `dbus-daemon` package; Fedora: `dbus-daemon`) and run it again."
        )
    })
}

/// Just enough of `com.mos.mosd1` for this module: the settings read a cache
/// fill uses, and a write that emits `SettingsChanged` the way the real mosd
/// does — after the write, with the dot-path and the JSON value.
struct FakeMosd;

#[zbus::interface(name = "com.mos.mosd1")]
impl FakeMosd {
    async fn get_settings(&self, path: &str) -> zbus::fdo::Result<String> {
        match path {
            "access" => Ok(r#"{"webAdmin":{"password_hash":"seeded"}}"#.to_string()),
            other => Err(zbus::fdo::Error::Failed(format!(
                "path not found: `{other}`"
            ))),
        }
    }

    async fn set_settings(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        path: &str,
        value_json: &str,
    ) -> zbus::fdo::Result<String> {
        Self::settings_changed(&emitter, path, value_json)
            .await
            .map_err(|err| zbus::fdo::Error::Failed(format!("emit SettingsChanged: {err}")))?;
        let task = serde_json::json!({
            "id": "fake-task-1",
            "operation": "settings-write",
            "dotPath": path,
            "source": ":1.9",
            "status": "running",
            "enqueuedAt": "2026-08-31T00:00:00.000Z",
            "startedAt": "2026-08-31T00:00:01.000Z",
            "foldedCount": 0
        });
        Self::task_changed(&emitter, &task.to_string())
            .await
            .map_err(|err| zbus::fdo::Error::Failed(format!("emit TaskChanged: {err}")))?;
        Ok("fake-task-1".to_string())
    }

    #[zbus(signal)]
    async fn settings_changed(
        emitter: &SignalEmitter<'_>,
        path: &str,
        value_json: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn task_changed(emitter: &SignalEmitter<'_>, task_json: &str) -> zbus::Result<()>;
}

/// Poll `predicate` until it holds or a ~2 s deadline passes, then report it.
///
/// Returning the last answer rather than asserting keeps the caller's message
/// on the assertion that actually failed.
async fn settles(predicate: impl Fn() -> bool) -> bool {
    for _ in 0..500 {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(4)).await;
    }
    predicate()
}

/// The whole watcher lifecycle over one private bus. One test rather than
/// several because the phases share expensive fixtures and each phase is the
/// next one's precondition; every assertion carries its own message.
#[tokio::test(flavor = "multi_thread")]
async fn the_settings_changed_subscription_feeds_the_access_cache() {
    // Private session bus; never the host's.
    let mut child = Command::new(dbus_daemon())
        .args(["--session", "--print-address=1", "--nofork"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn dbus-daemon");
    let stdout = child.stdout.take().expect("piped stdout");
    let bus = ChildGuard(child);
    let mut address = String::new();
    BufReader::new(stdout)
        .read_line(&mut address)
        .expect("read the bus address");
    let address = address.trim().to_string();
    assert!(!address.is_empty(), "dbus-daemon printed no address");

    let _server = zbus::connection::Builder::address(address.as_str())
        .expect("bus address")
        .name("com.mos.mosd")
        .expect("well-known name")
        .serve_at("/com/mos/mosd", FakeMosd)
        .expect("serve com.mos.mosd1")
        .build()
        .await
        .expect("fake mosd on the private bus");
    let watcher_connection = zbus::connection::Builder::address(address.as_str())
        .expect("bus address")
        .build()
        .await
        .expect("watcher connection");
    let writer_connection = zbus::connection::Builder::address(address.as_str())
        .expect("bus address")
        .build()
        .await
        .expect("writer connection");

    // The watcher, wrapped the way `watch_settings_changed`'s loop wraps it:
    // the pump marks the subscription live, the wrapper marks the lapse.
    let cache = Arc::new(AccessCache::new());
    let watcher = tokio::spawn({
        let cache = cache.clone();
        let connection = watcher_connection.clone();
        async move {
            let _ = bus_client::watch_connection(&connection, &cache).await;
            cache.lapsed();
        }
    });
    let registry = Arc::new(TaskRegistry::new());
    let task_watcher = tokio::spawn({
        let registry = registry.clone();
        let connection = watcher_connection.clone();
        async move {
            let _ = bus_client::watch_task_connection(&connection, &registry, None).await;
            registry.lapsed();
        }
    });
    assert!(
        settles(|| cache.is_synchronised()).await,
        "the subscription never went live"
    );
    assert!(
        settles(|| registry.is_synchronised()).await,
        "the task subscription never went live"
    );

    // Fill the way the gate does: generation before the read, through the
    // real client, over the real bus.
    let client = BusSettings::with_connection(&writer_connection)
        .await
        .expect("mosd client");
    let generation = cache.generation();
    let access = client.get_settings("access").await.expect("access subtree");
    cache.fill(generation, access);
    assert!(cache.get().is_some(), "the fill must land while subscribed");

    // A change under `access` — the real proxy write, so the signal crosses
    // the real bus — invalidates the cache.
    client
        .set_settings(
            "access.webAdmin",
            &serde_json::json!({ "password_hash": "rotated" }),
        )
        .await
        .expect("access write");
    assert!(
        settles(|| cache.get().is_none()).await,
        "the access change never invalidated the cache"
    );
    assert!(
        settles(|| registry.get("fake-task-1").is_some()).await,
        "TaskChanged never reached the registry"
    );

    // A change elsewhere leaves a refilled cache standing: the filter is by
    // dot segments, not by 'any signal at all'.
    //
    // Nothing here waits a fixed 100 ms for the hostname signal to arrive, as
    // this once did: a signal slower than the wait made the assertion vacuous
    // rather than red, which on a loaded machine is whenever it mattered.
    // What is counted instead is INVALIDATIONS, and the count is taken after
    // the join below — see there.
    let generation = cache.generation();
    cache.fill(generation, serde_json::json!({ "webAdmin": {} }));
    assert!(
        cache.get().is_some(),
        "the refill did not land, so the assertions below are about nothing"
    );
    client
        .set_settings("hostname", &Value::String("probe".into()))
        .await
        .expect("hostname write");
    client
        .set_settings(
            "access.webAdmin",
            &serde_json::json!({ "password_hash": "rotated again" }),
        )
        .await
        .expect("second access write");
    assert!(
        settles(|| cache.get().is_none()).await,
        "the second access change never invalidated the cache"
    );

    // The lapse: the bus dies under the stream, and the watcher's exit path
    // drops the cache back to direct reads — the lockout rule's fallback.
    //
    // The watchers are joined BEFORE anything below is asserted, because
    // their exit is what performs the lapse: `watch_connection` returns when
    // the stream ends and the wrapper then calls `lapsed()`, so a completed
    // join is the event itself. Polling for the flag instead put a deadline
    // on how fast the daemon could die and the runtime could reschedule,
    // which is a property of the machine and not of the watcher.
    drop(bus);
    watcher.await.expect("the watcher task must exit cleanly");
    task_watcher
        .await
        .expect("the task watcher must exit cleanly");
    assert!(
        !cache.is_synchronised(),
        "the stream's end was never observed as a lapse"
    );
    assert_eq!(cache.get(), None, "nothing may be served across a lapse");
    assert_eq!(
        registry.get("fake-task-1"),
        None,
        "a stale running task must not be served across a lapse"
    );
    // The join is what makes this a count and not a sample: every signal the
    // watcher was ever going to see has been seen. Two bumps are owed since
    // `generation` was taken — the access write's invalidation and the
    // lapse's. A third is the hostname write costing the cache, which is what
    // segment-wise filtering exists to prevent.
    assert_eq!(
        cache.generation(),
        generation + 2,
        "an unrelated change invalidated the cache: the dot-segment filter matched `hostname`"
    );
}

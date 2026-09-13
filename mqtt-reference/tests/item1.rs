//! Private-bus integration coverage for the reference application's public
//! Item1 tree. The system-bus policy has its own image and hardware checks;
//! this test proves the D-Bus object paths, values and signal ordering without
//! ever connecting to the host bus.

#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use mica_mqtt_reference::{
    BUS_NAME, ITEM_PATHS, Items, REFUSED_OUT_OF_RANGE, REFUSED_READ_ONLY, ROOT_PATH,
    SETPOINT_INITIAL, SETPOINT_MAX, SETPOINT_MIN, SETPOINT_PATH, start,
};
use zbus::zvariant::{OwnedValue, Value};

/// Kill the private bus even when an assertion fails.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The test must not silently skip when it cannot start a real D-Bus daemon.
fn dbus_daemon() -> anyhow::Result<PathBuf> {
    let fixed = PathBuf::from("/usr/bin/dbus-daemon");
    if fixed.exists() {
        return Ok(fixed);
    }
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&search_path)
        .map(|dir| dir.join("dbus-daemon"))
        .find(|candidate| candidate.exists())
        .context("dbus-daemon is required for the Item1 integration test")
}

struct Bus {
    address: String,
    _child: ChildGuard,
}

impl Bus {
    fn start() -> anyhow::Result<Self> {
        let mut child = Command::new(dbus_daemon()?)
            .args(["--session", "--print-address=1", "--nofork"])
            .stdout(Stdio::piped())
            .spawn()
            .context("start private dbus-daemon")?;
        let stdout = child
            .stdout
            .take()
            .context("capture private dbus-daemon address")?;
        let mut address = String::new();
        BufReader::new(stdout)
            .read_line(&mut address)
            .context("read private dbus-daemon address")?;
        let address = address.trim().to_string();
        anyhow::ensure!(
            !address.is_empty(),
            "private dbus-daemon printed no address"
        );
        Ok(Self {
            address,
            _child: ChildGuard(child),
        })
    }

    async fn connection(&self) -> anyhow::Result<zbus::Connection> {
        zbus::connection::Builder::address(self.address.as_str())
            .context("parse private D-Bus address")?
            .build()
            .await
            .context("connect to private D-Bus")
    }
}

fn attributes<'a>(
    items: &'a Items,
    path: &str,
) -> anyhow::Result<&'a std::collections::HashMap<String, OwnedValue>> {
    items
        .get(path)
        .with_context(|| format!("GetItems omitted {path}"))
}

fn integer_attribute(items: &Items, path: &str, key: &str) -> anyhow::Result<i64> {
    let value = attributes(items, path)?
        .get(key)
        .with_context(|| format!("{path} omitted {key}"))?;
    i64::try_from(value).with_context(|| format!("{path}.{key} was not an i64"))
}

fn boolean_attribute(items: &Items, path: &str, key: &str) -> anyhow::Result<bool> {
    let value = attributes(items, path)?
        .get(key)
        .with_context(|| format!("{path} omitted {key}"))?;
    bool::try_from(value).with_context(|| format!("{path}.{key} was not a bool"))
}

#[tokio::test(flavor = "multi_thread")]
async fn item_tree_is_bounded_and_changes_only_after_an_accepted_write() -> anyhow::Result<()> {
    let bus = Bus::start()?;
    let _reference = start(bus.connection().await?).await?;
    let client = bus.connection().await?;

    let root = zbus::Proxy::new(&client, BUS_NAME, ROOT_PATH, "com.mica.Item1")
        .await
        .context("create root Item1 proxy")?;
    let items: Items = root
        .call("GetItems", &())
        .await
        .context("call root GetItems")?;
    anyhow::ensure!(
        items.len() == ITEM_PATHS.len(),
        "GetItems returned {} paths, expected {}",
        items.len(),
        ITEM_PATHS.len(),
    );
    for path in ITEM_PATHS {
        anyhow::ensure!(
            items.contains_key(path),
            "GetItems omitted required sample path {path}"
        );
    }
    anyhow::ensure!(
        integer_attribute(&items, SETPOINT_PATH, "value")? == SETPOINT_INITIAL,
        "initial setpoint differs from the published reference value",
    );
    anyhow::ensure!(boolean_attribute(&items, SETPOINT_PATH, "writable")?);
    anyhow::ensure!(integer_attribute(&items, SETPOINT_PATH, "min")? == SETPOINT_MIN);
    anyhow::ensure!(integer_attribute(&items, SETPOINT_PATH, "max")? == SETPOINT_MAX);
    anyhow::ensure!(
        !boolean_attribute(&items, "/Example/ReadOnly", "writable")?,
        "read-only sample item is marked writable",
    );

    let mut changes = root
        .receive_signal("ItemsChanged")
        .await
        .context("subscribe to root ItemsChanged")?;
    anyhow::ensure!(
        tokio::time::timeout(Duration::from_millis(150), changes.next())
            .await
            .is_err(),
        "the reference emitted ItemsChanged without an accepted mutation",
    );

    let setpoint = zbus::Proxy::new(&client, BUS_NAME, SETPOINT_PATH, "com.mica.Item1")
        .await
        .context("create setpoint Item1 proxy")?;
    let accepted: i32 = setpoint
        .call("SetValue", &(Value::from(43_i64),))
        .await
        .context("write in-range setpoint")?;
    anyhow::ensure!(
        accepted == 0,
        "in-range SetValue returned {accepted}, expected 0"
    );

    let signal = tokio::time::timeout(Duration::from_secs(3), changes.next())
        .await
        .context("accepted SetValue did not emit ItemsChanged")?
        .context("ItemsChanged stream ended")?;
    let changed: Items = signal
        .body()
        .deserialize()
        .context("decode ItemsChanged item batch")?;
    anyhow::ensure!(changed.len() == 1 && changed.contains_key(SETPOINT_PATH));
    anyhow::ensure!(integer_attribute(&changed, SETPOINT_PATH, "value")? == 43);

    let current: OwnedValue = setpoint
        .call("GetValue", &())
        .await
        .context("read setpoint")?;
    anyhow::ensure!(
        i64::try_from(&current)? == 43,
        "GetValue did not observe accepted setpoint"
    );

    let out_of_range: i32 = setpoint
        .call("SetValue", &(Value::from(SETPOINT_MAX + 1),))
        .await
        .context("write out-of-range setpoint")?;
    anyhow::ensure!(
        out_of_range == REFUSED_OUT_OF_RANGE,
        "out-of-range SetValue returned {out_of_range}, expected {REFUSED_OUT_OF_RANGE}",
    );
    anyhow::ensure!(
        tokio::time::timeout(Duration::from_millis(150), changes.next())
            .await
            .is_err(),
        "rejected SetValue emitted ItemsChanged",
    );
    let unchanged: OwnedValue = setpoint
        .call("GetValue", &())
        .await
        .context("re-read setpoint")?;
    anyhow::ensure!(
        i64::try_from(&unchanged)? == 43,
        "rejected SetValue changed the value"
    );

    let read_only = zbus::Proxy::new(&client, BUS_NAME, "/Example/ReadOnly", "com.mica.Item1")
        .await
        .context("create read-only Item1 proxy")?;
    let refused: i32 = read_only
        .call("SetValue", &(Value::from("changed"),))
        .await
        .context("write read-only item")?;
    anyhow::ensure!(
        refused == REFUSED_READ_ONLY,
        "read-only SetValue returned {refused}, expected {REFUSED_READ_ONLY}",
    );
    anyhow::ensure!(
        tokio::time::timeout(Duration::from_millis(150), changes.next())
            .await
            .is_err(),
        "read-only SetValue emitted ItemsChanged",
    );
    Ok(())
}

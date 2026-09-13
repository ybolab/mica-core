//! The deliberately small `com.mica.Item1` service used to prove the MQTT
//! application boundary on a real image.
//!
//! It is an application, not bridge infrastructure: it owns one exact
//! sample-only name, exposes a fixed item tree, and changes one bounded value
//! only after a caller addresses that item's object path. The immutable
//! package policy decides who can invoke each method.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::Mutex;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Str, Value};

/// Exact package enrollment and systemd `BusName=` for this reference only.
pub const BUS_NAME: &str = "com.mica.mqttsample.reference";
/// The Item1 tree root, where discovery and change signals are served.
pub const ROOT_PATH: &str = "/";
/// The bounded, writable example used by the full-mode hardware proof.
pub const SETPOINT_PATH: &str = "/Example/Setpoint";
pub const SETPOINT_MIN: i64 = 0;
pub const SETPOINT_MAX: i64 = 100;
pub const SETPOINT_INITIAL: i64 = 42;

/// Application-defined `SetValue` return codes.
pub const REFUSED_READ_ONLY: i32 = 1;
pub const REFUSED_INVALID_VALUE: i32 = 2;
pub const REFUSED_OUT_OF_RANGE: i32 = 3;
pub const REFUSED_SIGNAL_FAILURE: i32 = 4;

/// Every item the reference service exports, including the seven registry
/// facts required by `docs/design/bus.md`.
pub const ITEM_PATHS: [&str; 9] = [
    "/Mgmt/ProcessName",
    "/Mgmt/ProcessVersion",
    "/Mgmt/Connection",
    "/DeviceInstance",
    "/ProductId",
    "/ProductName",
    "/Connected",
    "/Example/ReadOnly",
    SETPOINT_PATH,
];

/// `a{sa{sv}}`, the wire shape of Item1 discovery and change batches.
pub type Items = HashMap<String, HashMap<String, OwnedValue>>;

#[derive(Debug)]
struct State {
    setpoint: i64,
}

type SharedState = Arc<Mutex<State>>;

struct Root {
    state: SharedState,
}

struct Item {
    path: &'static str,
    state: SharedState,
    root_emitter: SignalEmitter<'static>,
}

/// A claimed reference service. Retaining this value retains its D-Bus
/// connection and therefore its exact well-known name.
pub struct RunningReference {
    _connection: zbus::Connection,
}

/// Register the reference tree before claiming [`BUS_NAME`].
///
/// Registering every object before the name becomes visible prevents the
/// bridge's initial `GetItems`/`ItemsChanged` setup from observing a claimed
/// service whose root has not yet been installed.
pub async fn start(connection: zbus::Connection) -> anyhow::Result<RunningReference> {
    let state = Arc::new(Mutex::new(State {
        setpoint: SETPOINT_INITIAL,
    }));
    let root_emitter = SignalEmitter::new(&connection, ROOT_PATH)
        .context("create root ItemsChanged emitter")?
        .to_owned();

    connection
        .object_server()
        .at(
            ROOT_PATH,
            Root {
                state: Arc::clone(&state),
            },
        )
        .await
        .context("serve Item1 root")?;
    for path in ITEM_PATHS {
        connection
            .object_server()
            .at(
                path,
                Item {
                    path,
                    state: Arc::clone(&state),
                    root_emitter: root_emitter.clone(),
                },
            )
            .await
            .with_context(|| format!("serve Item1 item {path}"))?;
    }
    connection
        .request_name(BUS_NAME)
        .await
        .with_context(|| format!("request D-Bus name {BUS_NAME}"))?;
    tracing::info!(name = BUS_NAME, "reference Item1 application is serving");

    Ok(RunningReference {
        _connection: connection,
    })
}

/// Serve the reference package on the real system bus until systemd stops it.
pub async fn serve_system() -> anyhow::Result<()> {
    let connection = zbus::Connection::system()
        .await
        .context("connect to system D-Bus")?;
    let _reference = start(connection).await?;

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("SIGTERM received, exiting"),
        result = tokio::signal::ctrl_c() => {
            result.context("wait for SIGINT")?;
            tracing::info!("SIGINT received, exiting");
        }
    }
    Ok(())
}

#[zbus::interface(name = "com.mica.Item1")]
impl Root {
    /// Whole-tree discovery. This is intentionally a snapshot only; the
    /// bridge publishes it after a client opens the documented keepalive
    /// window, not merely because this application appeared.
    async fn get_items(&self) -> Items {
        let state = self.state.lock().await;
        snapshot(state.setpoint)
    }

    /// Emitted at `/` after an accepted mutation.
    #[zbus(signal)]
    async fn items_changed(emitter: &SignalEmitter<'_>, items: &Items) -> zbus::Result<()>;
}

#[zbus::interface(name = "com.mica.Item1")]
impl Item {
    /// Read one item's current value from its addressed object path.
    async fn get_value(&self) -> zbus::fdo::Result<OwnedValue> {
        let state = self.state.lock().await;
        value_for(self.path, state.setpoint).ok_or_else(|| {
            zbus::fdo::Error::Failed(format!("reference item {} has no value", self.path))
        })
    }

    /// Set only the bounded sample value. Read-only paths and malformed input
    /// are application refusals, not D-Bus transport faults, so the bridge can
    /// distinguish its lack of a later `ItemsChanged` notification.
    async fn set_value(&self, value: Value<'_>) -> i32 {
        if self.path != SETPOINT_PATH {
            tracing::info!(
                path = self.path,
                "reference write refused: item is read-only"
            );
            return REFUSED_READ_ONLY;
        }
        let Some(value) = integer_value(&value) else {
            tracing::info!(
                path = self.path,
                "reference write refused: value is not an integer"
            );
            return REFUSED_INVALID_VALUE;
        };
        if !(SETPOINT_MIN..=SETPOINT_MAX).contains(&value) {
            tracing::info!(
                path = self.path,
                "reference write refused: value is outside configured bounds"
            );
            return REFUSED_OUT_OF_RANGE;
        }

        // Serialise successful writes with the signal. A `SetValue` returning
        // zero therefore means the later bridge-visible signal has already
        // been accepted for emission, rather than reporting success before
        // there is a state transition to observe.
        let mut state = self.state.lock().await;
        let previous = state.setpoint;
        state.setpoint = value;
        let changed = setpoint_change(value);
        if let Err(error) = Root::items_changed(&self.root_emitter, &changed).await {
            state.setpoint = previous;
            tracing::error!(path = self.path, %error, "reference write refused: emit ItemsChanged failed");
            return REFUSED_SIGNAL_FAILURE;
        }
        tracing::info!(path = self.path, "reference setpoint updated");
        0
    }
}

fn snapshot(setpoint: i64) -> Items {
    ITEM_PATHS
        .into_iter()
        .filter_map(|path| item_attributes(path, setpoint).map(|attrs| (path.to_string(), attrs)))
        .collect()
}

fn setpoint_change(value: i64) -> Items {
    HashMap::from([(
        SETPOINT_PATH.to_string(),
        HashMap::from([
            ("value".to_string(), OwnedValue::from(value)),
            ("writable".to_string(), OwnedValue::from(true)),
            ("min".to_string(), OwnedValue::from(SETPOINT_MIN)),
            ("max".to_string(), OwnedValue::from(SETPOINT_MAX)),
        ]),
    )])
}

fn item_attributes(path: &str, setpoint: i64) -> Option<HashMap<String, OwnedValue>> {
    let value = value_for(path, setpoint)?;
    let mut attrs = HashMap::from([
        ("value".to_string(), value),
        (
            "writable".to_string(),
            OwnedValue::from(path == SETPOINT_PATH),
        ),
    ]);
    if path == SETPOINT_PATH {
        attrs.insert("min".to_string(), OwnedValue::from(SETPOINT_MIN));
        attrs.insert("max".to_string(), OwnedValue::from(SETPOINT_MAX));
    }
    Some(attrs)
}

fn value_for(path: &str, setpoint: i64) -> Option<OwnedValue> {
    Some(match path {
        "/Mgmt/ProcessName" => string_value("mica-mqtt-reference"),
        "/Mgmt/ProcessVersion" => string_value(env!("CARGO_PKG_VERSION")),
        "/Mgmt/Connection" => string_value("connected"),
        "/DeviceInstance" => OwnedValue::from(1_i64),
        "/ProductId" => string_value("mica-mqtt-reference"),
        "/ProductName" => string_value("mos MQTT reference"),
        "/Connected" => OwnedValue::from(true),
        "/Example/ReadOnly" => string_value("ready"),
        SETPOINT_PATH => OwnedValue::from(setpoint),
        _ => return None,
    })
}

fn string_value(value: &'static str) -> OwnedValue {
    OwnedValue::from(Str::from(value))
}

fn integer_value(value: &Value<'_>) -> Option<i64> {
    match value {
        Value::U8(number) => Some(i64::from(*number)),
        Value::I16(number) => Some(i64::from(*number)),
        Value::U16(number) => Some(i64::from(*number)),
        Value::I32(number) => Some(i64::from(*number)),
        Value::U32(number) => Some(i64::from(*number)),
        Value::I64(number) => Some(*number),
        Value::U64(number) => i64::try_from(*number).ok(),
        Value::Value(inner) => integer_value(inner),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        REFUSED_INVALID_VALUE, REFUSED_OUT_OF_RANGE, SETPOINT_MAX, SETPOINT_MIN, integer_value,
    };
    use zbus::zvariant::Value;

    #[test]
    fn integer_value_accepts_only_integral_dbus_values() {
        assert_eq!(integer_value(&Value::from(7_i64)), Some(7));
        assert_eq!(integer_value(&Value::from(7_u64)), Some(7));
        assert_eq!(integer_value(&Value::from(1.5_f64)), None);
        assert_eq!(integer_value(&Value::from("7")), None);
    }

    #[test]
    fn refusal_codes_remain_nonzero_and_bounds_are_ordered() {
        assert_ne!(REFUSED_INVALID_VALUE, 0);
        assert_ne!(REFUSED_OUT_OF_RANGE, 0);
        const { assert!(SETPOINT_MIN < SETPOINT_MAX) };
    }
}

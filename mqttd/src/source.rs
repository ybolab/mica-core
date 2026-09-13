//! The application side of the bridge: `com.mica.Item1` reads, signals and
//! writes on explicitly enrolled application services.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value as Json;
use zbus::zvariant::{OwnedValue, Value};

use crate::item::Item;
use crate::topic::Application;

/// What became of an application `SetValue`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// `SetValue` returned `0`.
    Accepted,
    /// `SetValue` returned a non-zero result code.
    Refused { code: i32 },
    /// No item object exists at the requested path.
    UnknownObject,
    /// The JSON value has no D-Bus representation used by this bridge.
    Unrepresentable,
    /// The service or bus could not be reached.
    Unreachable { detail: String },
}

impl WriteOutcome {
    pub fn accepted(&self) -> bool {
        matches!(self, Self::Accepted)
    }
}

/// The two operations MQTT performs on an application item tree.
#[async_trait]
pub trait ItemSource: Send + Sync {
    async fn get_items(&self, application: &Application) -> anyhow::Result<BTreeMap<String, Item>>;

    async fn set_value(&self, application: &Application, path: &str, value: Json) -> WriteOutcome;
}

/// How long one call into an application may take before it counts as
/// unanswered.
///
/// An application is a third-party process, and a bus call to one has no
/// bound of its own: zbus waits for the reply indefinitely. One application
/// that accepts a call and never answers must not hold the heartbeat and
/// every other application with it. micad's registry probe applies the same
/// figure for the same reason.
pub const APPLICATION_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// An [`ItemSource`] whose calls are bounded by a timeout.
///
/// A `GetItems` that does not answer in time is an error, which leaves the
/// application unactivated; a `SetValue` that does not answer is an
/// unreachable write. Either way the runtime moves on.
pub struct Bounded<S> {
    inner: S,
    timeout: Duration,
}

impl<S> Bounded<S> {
    pub fn new(inner: S, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}

#[async_trait]
impl<S: ItemSource> ItemSource for Bounded<S> {
    async fn get_items(&self, application: &Application) -> anyhow::Result<BTreeMap<String, Item>> {
        tokio::time::timeout(self.timeout, self.inner.get_items(application))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "{} did not answer GetItems within {:?}",
                    application.bus_name(),
                    self.timeout
                )
            })?
    }

    async fn set_value(&self, application: &Application, path: &str, value: Json) -> WriteOutcome {
        match tokio::time::timeout(self.timeout, self.inner.set_value(application, path, value))
            .await
        {
            Ok(outcome) => outcome,
            Err(_) => WriteOutcome::Unreachable {
                detail: format!("SetValue did not answer within {:?}", self.timeout),
            },
        }
    }
}

/// Convert a D-Bus value into the JSON carried by MQTT payloads.
pub fn json_of(value: &Value<'_>) -> Json {
    match value {
        Value::Bool(flag) => Json::Bool(*flag),
        Value::U8(number) => Json::from(*number),
        Value::I16(number) => Json::from(*number),
        Value::U16(number) => Json::from(*number),
        Value::I32(number) => Json::from(*number),
        Value::U32(number) => Json::from(*number),
        Value::I64(number) => Json::from(*number),
        Value::U64(number) => Json::from(*number),
        Value::F64(number) => Json::from(*number),
        Value::Str(text) => Json::String(text.to_string()),
        Value::ObjectPath(path) => Json::String(path.to_string()),
        Value::Signature(signature) => Json::String(signature.to_string()),
        Value::Value(inner) => json_of(inner),
        Value::Array(items) if items.is_empty() => Json::Null,
        Value::Array(items) => Json::Array(items.iter().map(json_of).collect()),
        Value::Dict(entries) => Json::Object(
            entries
                .iter()
                .filter_map(|(key, value)| match key {
                    Value::Str(key) => Some((key.to_string(), json_of(value))),
                    _ => None,
                })
                .collect(),
        ),
        _ => Json::Null,
    }
}

/// Convert JSON back into the D-Bus value an application `SetValue` carries.
pub fn value_of(json: &Json) -> Option<Value<'static>> {
    Some(match json {
        Json::Null => return None,
        Json::Bool(flag) => Value::from(*flag),
        Json::Number(number) => {
            if let Some(int) = number.as_i64() {
                Value::from(int)
            } else if let Some(uint) = number.as_u64() {
                Value::from(uint)
            } else {
                Value::from(number.as_f64()?)
            }
        }
        Json::String(text) => Value::from(text.clone()),
        Json::Array(items) => Value::from(items.iter().filter_map(value_of).collect::<Vec<_>>()),
        Json::Object(map) => Value::from(
            map.iter()
                .filter_map(|(key, child)| value_of(child).map(|child| (key.clone(), child)))
                .collect::<HashMap<_, _>>(),
        ),
    })
}

/// The item one `a{sv}` attribute dictionary describes.
pub fn item_of(attrs: &HashMap<String, OwnedValue>) -> Option<Item> {
    let bound = |key: &str| attrs.get(key).map(|value| json_of(value));
    let value = bound("value")?;
    if value.is_null() {
        return None;
    }
    Some(Item {
        value,
        writable: attrs
            .get("writable")
            .map(|value| json_of(value))
            .is_some_and(|writable| writable == Json::Bool(true)),
        min: bound("min"),
        max: bound("max"),
    })
}

/// Convert one `ItemsChanged` payload into bridge state.
pub fn batch_of(
    items: HashMap<String, HashMap<String, OwnedValue>>,
) -> BTreeMap<String, Option<Item>> {
    items
        .into_iter()
        .map(|(path, attrs)| (path, item_of(&attrs)))
        .collect()
}

/// The tree-wide half of `com.mica.Item1`, served at `/` by each application.
#[zbus::proxy(interface = "com.mica.Item1", default_path = "/")]
pub trait ItemTree {
    fn get_items(&self) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;

    #[zbus(signal)]
    fn items_changed(
        &self,
        items: HashMap<String, HashMap<String, OwnedValue>>,
    ) -> zbus::Result<()>;
}

/// [`ItemSource`] over a real D-Bus connection.
pub struct BusSource {
    connection: zbus::Connection,
}

impl BusSource {
    pub fn new(connection: zbus::Connection) -> Self {
        Self { connection }
    }
}

const NO_SUCH_ITEM: [&str; 3] = [
    "org.freedesktop.DBus.Error.UnknownObject",
    "org.freedesktop.DBus.Error.UnknownInterface",
    "org.freedesktop.DBus.Error.UnknownMethod",
];

#[async_trait]
impl ItemSource for BusSource {
    async fn get_items(&self, application: &Application) -> anyhow::Result<BTreeMap<String, Item>> {
        let proxy = ItemTreeProxy::builder(&self.connection)
            .destination(application.bus_name().to_string())?
            .build()
            .await?;
        Ok(proxy
            .get_items()
            .await?
            .into_iter()
            .filter_map(|(path, attrs)| item_of(&attrs).map(|item| (path, item)))
            .collect())
    }

    async fn set_value(&self, application: &Application, path: &str, value: Json) -> WriteOutcome {
        let Some(value) = value_of(&value) else {
            return WriteOutcome::Unrepresentable;
        };
        let proxy = match zbus::Proxy::new(
            &self.connection,
            application.bus_name(),
            path,
            "com.mica.Item1",
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(zbus::Error::Variant(_) | zbus::Error::InvalidField) => {
                return WriteOutcome::UnknownObject;
            }
            Err(err) => {
                return WriteOutcome::Unreachable {
                    detail: err.to_string(),
                };
            }
        };
        match proxy.call::<_, _, i32>("SetValue", &(value,)).await {
            Ok(0) => WriteOutcome::Accepted,
            Ok(code) => WriteOutcome::Refused { code },
            Err(zbus::Error::MethodError(name, _, _)) if NO_SUCH_ITEM.contains(&name.as_str()) => {
                WriteOutcome::UnknownObject
            }
            Err(err) => WriteOutcome::Unreachable {
                detail: err.to_string(),
            },
        }
    }
}

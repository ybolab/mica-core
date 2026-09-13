//! The service registry: what micad knows about every OTHER `com.mica.*`
//! service on the bus (`docs/design/bus.md`, service registry).
//!
//! micad subscribes to `org.freedesktop.DBus`'s `NameOwnerChanged` under
//! `arg0namespace='com.mica'`, so the bus does the filtering and micad is never
//! woken for a name it would only discard. A name that gains an owner is
//! probed once and published; a name that loses its owner is **retained**
//! with `connected: false` until an operator drops it through
//! [`ForgetService`](crate::bus::MicadService::forget_service).
//!
//! # Best-effort, never refused
//!
//! A service that does not conform to the registry contract is published anyway, with a
//! `conformance` object naming what is missing and exactly one `warn!` on the
//! way in. Nothing here refuses, drops or panics on a malformed service: the
//! difference between best-effort and silently degraded is that the registry
//! can say WHY a service publishes oddly, and an operator who cannot see that
//! concludes the bridge is broken.
//!
//! # What is published, and what is not
//!
//! Only the enumerated fields of [`Entry::to_json`] ever reach the live-state
//! tree — never the probed service's items. A third-party service may put
//! anything in them, including secrets, and the registry is not a place for
//! anything a service did not intend for the whole bus. The probe therefore
//! reads exactly two things out of a service's item map: which of the seven
//! mandatory paths are present, and the value of `/DeviceInstance`.
//!
//! # The class comes from `mica-busname`, never from here
//!
//! `com.mica.sensor.abc123` is class `sensor`. That fact comes out of
//! [`mica_busname::parse`], which is the one implementation of the direct
//! `com.mica.<class>[.<suffix>]` rule in the tree.

use std::collections::{BTreeMap, HashMap};
use std::future::poll_fn;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value as Json};
use zbus::export::futures_core::Stream;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, fdo};

use crate::bus::{BUS_NAME, MicadService};

/// Live-state key the whole registry is published under.
pub const STATE_KEY: &str = "services";

/// The seven paths every `com.mica.*` service must publish from the moment it
/// claims its name (`docs/design/bus.md`, service registry). Absence is a conformance gap,
/// never a reason to refuse the service.
const MANDATORY_PATHS: [&str; 7] = [
    "/Mgmt/ProcessName",
    "/Mgmt/ProcessVersion",
    "/Mgmt/Connection",
    "/DeviceInstance",
    "/ProductId",
    "/ProductName",
    "/Connected",
];

/// The mandatory path carrying the instance number.
const DEVICE_INSTANCE: &str = "/DeviceInstance";

/// The instance a service that publishes none is recorded under — the Venus
/// fallback. Deliberately a real value rather than `null`: consumers key on
/// the instance, and a service with no instance still has to be addressable.
/// Every such service lands on the same number, which is what makes
/// `instance_collision` worth reporting.
const FALLBACK_INSTANCE: i64 = 0;

/// How long one probe waits for `GetItems` before giving up on it.
///
/// A third-party service that accepts the call and never answers must not
/// wedge the registry for every service behind it, so a probe that does not
/// come back in this long is recorded exactly as a service that answers no
/// `com.mica.Item1` at all.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The `a{sa{sv}}` body of `com.mica.Item1.GetItems`: item path -> attributes.
type Items = HashMap<String, HashMap<String, OwnedValue>>;

/// What one probe of a service established.
struct Probe {
    /// The service answered `com.mica.Item1.GetItems` at the root.
    item1: bool,
    /// Which of [`MANDATORY_PATHS`] the service does not publish. All seven
    /// when there is no `Item1` to ask: a service that answers nothing
    /// publishes none of them, and saying so is more useful than an empty
    /// list that reads as "nothing missing".
    missing_paths: Vec<&'static str>,
    /// The `/DeviceInstance` value, or `None` when the service publishes no
    /// usable one.
    instance: Option<i64>,
}

/// What a service does NOT do that the `docs/design/bus.md` registry contract requires.
///
/// Every field is a gap, and an all-false `Conformance` is a conforming
/// service. The JSON only ever carries the gaps ([`Self::to_json`]), so an
/// empty object means "nothing missing" and a reader never has to know the
/// full vocabulary to interpret one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Conformance {
    /// No `com.mica.Item1` answered at the service's root object.
    no_item1: bool,
    /// No usable `/DeviceInstance`, so `instance` fell back to
    /// [`FALLBACK_INSTANCE`].
    no_device_instance: bool,
    /// The subset of [`MANDATORY_PATHS`] the service does not publish.
    missing_paths: Vec<&'static str>,
}

impl Conformance {
    /// Nothing is missing.
    fn is_clean(&self) -> bool {
        !self.no_item1 && !self.no_device_instance && self.missing_paths.is_empty()
    }

    /// The gaps, as the `conformance` object: absent keys are things that are
    /// fine, so `{}` is a fully conforming service.
    fn to_json(&self) -> Json {
        let mut out = Map::new();
        if self.no_item1 {
            out.insert("item1".to_string(), Json::Bool(false));
        }
        if self.no_device_instance {
            out.insert("device_instance".to_string(), Json::Bool(false));
        }
        if !self.missing_paths.is_empty() {
            out.insert(
                "missing_paths".to_string(),
                Json::Array(
                    self.missing_paths
                        .iter()
                        .map(|path| Json::String((*path).to_string()))
                        .collect(),
                ),
            );
        }
        Json::Object(out)
    }
}

/// One service as the registry holds it.
///
/// Owns its strings on purpose: [`mica_busname::BusName`] borrows from the name
/// it parsed, and the registry outlives every one of those borrows.
#[derive(Debug, Clone)]
struct Entry {
    class: Option<String>,
    connected: bool,
    instance: i64,
    conformance: Conformance,
}

impl Entry {
    /// This entry as it appears under `services.<bus name>`.
    ///
    /// `collision` is not a property of the entry alone, so it is passed in:
    /// see [`Registry::snapshot_of`].
    fn to_json(&self, name: &str, collision: bool) -> Json {
        serde_json::json!({
            "name": name,
            "class": self.class,
            "connected": self.connected,
            "instance": self.instance,
            "conformance": self.conformance.to_json(),
            "instance_collision": collision,
        })
    }
}

/// Every `com.mica.*` service micad has seen, keyed by bus name.
///
/// Shared between the scan task, which fills it, and
/// [`MicadService`](crate::bus::MicadService), whose `ForgetService` empties one
/// entry of it. The lock is a plain [`Mutex`] and is never held across an
/// await: every method takes it, finishes, and hands back an owned snapshot
/// for the caller to publish.
pub struct Registry {
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// An empty registry — the state of a bus with no services on it, which
    /// is a fact worth publishing rather than an absent key.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// The whole registry as JSON, for publishing into the live-state tree.
    pub fn snapshot(&self) -> Json {
        Self::snapshot_of(&self.entries.lock().expect("service registry lock"))
    }

    /// Record `entry` for `name`, replacing whatever was there: a service that
    /// re-appears is re-probed, so the newer answer is the true one.
    fn record(&self, name: &str, entry: Entry) -> Json {
        let mut entries = self.entries.lock().expect("service registry lock");
        entries.insert(name.to_string(), entry);
        Self::snapshot_of(&entries)
    }

    /// Mark `name` disconnected, keeping the entry. `None` when the registry
    /// does not carry that name, in which case there is nothing to publish.
    fn disconnect(&self, name: &str) -> Option<Json> {
        let mut entries = self.entries.lock().expect("service registry lock");
        let entry = entries.get_mut(name)?;
        entry.connected = false;
        Some(Self::snapshot_of(&entries))
    }

    /// Drop the entry for `name`, which must be present and disconnected.
    ///
    /// # Errors
    ///
    /// [`fdo::Error::InvalidArgs`] when there is no such entry, or when it is
    /// still connected — dropping a live service would only re-add it on the
    /// next event, so the refusal is the honest answer rather than a silent
    /// no-op.
    pub fn forget(&self, name: &str) -> fdo::Result<Json> {
        let mut entries = self.entries.lock().expect("service registry lock");
        match entries.get(name).map(|entry| entry.connected) {
            None => Err(fdo::Error::InvalidArgs(format!(
                "no service registry entry for `{name}`"
            ))),
            Some(true) => Err(fdo::Error::InvalidArgs(format!(
                "`{name}` is still connected; a connected service cannot be forgotten"
            ))),
            Some(false) => {
                entries.remove(name);
                Ok(Self::snapshot_of(&entries))
            }
        }
    }

    /// Render `entries`, computing `instance_collision` across the whole table
    /// as it goes.
    ///
    /// A collision is two CONNECTED services of the same class publishing the
    /// same instance (`docs/design/bus.md`: unique within the class), and
    /// **both** sides are marked — neither is more at fault than the other,
    /// and neither is dropped or shadowed, because a registry that hid one of
    /// them would hide the very fact an operator needs. Disconnected entries
    /// do not take part — a service that is not there publishes nothing to
    /// collide with.
    fn snapshot_of(entries: &BTreeMap<String, Entry>) -> Json {
        let mut counts: HashMap<(&str, i64), usize> = HashMap::new();
        for entry in entries.values().filter(|entry| entry.connected) {
            if let Some(class) = entry.class.as_deref() {
                *counts.entry((class, entry.instance)).or_default() += 1;
            }
        }
        let mut out = Map::new();
        for (name, entry) in entries {
            let collision = entry.connected
                && entry.class.as_deref().is_some_and(|class| {
                    counts.get(&(class, entry.instance)).is_some_and(|n| *n > 1)
                });
            out.insert(name.clone(), entry.to_json(name, collision));
        }
        Json::Object(out)
    }
}

/// The integer an item's `value` attribute carries, if it carries one.
///
/// Wider than "an `i64` variant" on purpose: a bus is a place where other
/// people's services publish, and one that publishes its instance as a `u32`
/// or as a whole-numbered double is publishing an instance. A fractional
/// double is not an instance number and is refused, which lands the service in
/// `conformance.device_instance` rather than under a rounded instance nobody
/// chose.
fn as_i64(value: &Value<'_>) -> Option<i64> {
    match value {
        Value::U8(n) => Some(i64::from(*n)),
        Value::I16(n) => Some(i64::from(*n)),
        Value::U16(n) => Some(i64::from(*n)),
        Value::I32(n) => Some(i64::from(*n)),
        Value::U32(n) => Some(i64::from(*n)),
        Value::I64(n) => Some(*n),
        Value::U64(n) => i64::try_from(*n).ok(),
        #[allow(clippy::cast_possible_truncation)]
        Value::F64(n) if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_992.0 => Some(*n as i64),
        Value::Value(inner) => as_i64(inner),
        _ => None,
    }
}

/// Ask `name` for its item map and read the two things the registry needs out
/// of it: which mandatory paths are present, and `/DeviceInstance`.
///
/// Never returns an error. Every way this can fail — no such interface, no
/// object at the root, a service that answers nothing — is the same registry
/// fact: no `com.mica.Item1`, which is published as a conformance gap.
async fn probe(connection: &Connection, name: &str) -> Probe {
    let call = connection.call_method(Some(name), "/", Some("com.mica.Item1"), "GetItems", &());
    let items = match tokio::time::timeout(PROBE_TIMEOUT, call).await {
        Ok(Ok(reply)) => {
            let body = reply.body();
            match body.deserialize::<Items>() {
                Ok(items) => Some(items),
                Err(err) => {
                    tracing::debug!(service = name, error = %err, "GetItems reply was not the a{{sa{{sv}}}} item map");
                    None
                }
            }
        }
        Ok(Err(err)) => {
            tracing::debug!(service = name, error = %err, "service answers no com.mica.Item1");
            None
        }
        Err(_) => {
            tracing::debug!(
                service = name,
                seconds = PROBE_TIMEOUT.as_secs(),
                "service did not answer GetItems in time"
            );
            None
        }
    };
    let item1 = items.is_some();
    let items = items.unwrap_or_default();
    Probe {
        item1,
        missing_paths: MANDATORY_PATHS
            .into_iter()
            .filter(|path| !items.contains_key(*path))
            .collect(),
        instance: items
            .get(DEVICE_INSTANCE)
            .and_then(|attrs| attrs.get("value"))
            .and_then(|value| as_i64(value)),
    }
}

/// Probe `name` and publish the entry it produces.
///
/// The whole of the "a name appeared" path, and the whole of the "the daemon
/// just started and this name was already there" path — one behaviour, so a
/// service discovered by the initial sweep is recorded exactly as one
/// discovered by a signal.
async fn record_appearance(
    connection: &Connection,
    registry: &Registry,
    service: &InterfaceRef<MicadService>,
    name: &str,
) {
    // Never micad itself: the registry is micad's view of the OTHER services on
    // the bus, and probing our own name would have the daemon call into its
    // own object server and wait for itself.
    if name == BUS_NAME {
        return;
    }
    // The one rule, taken rather than restated (`mica-busname`). A name that
    // does not parse is not one of ours — `com.mica` itself reaches here,
    // because `arg0namespace='com.mica'` matches the namespace as well as the
    // names under it.
    let Some(parsed) = mica_busname::parse(name) else {
        tracing::debug!(service = name, "not a mica bus name; not registered");
        return;
    };
    let probe = probe(connection, name).await;
    let conformance = Conformance {
        no_item1: !probe.item1,
        no_device_instance: probe.instance.is_none(),
        missing_paths: probe.missing_paths,
    };
    if !conformance.is_clean() {
        // Exactly one warning per non-conforming service, naming the service
        // and every gap at once. A warning, never an error and never a
        // refusal: the entry below is published either way.
        tracing::warn!(
            service = name,
            item1 = probe.item1,
            device_instance = probe.instance.is_some(),
            missing_paths = ?conformance.missing_paths,
            "service does not conform to docs/design/bus.md service registry contract; \
             registered best-effort"
        );
    }
    let entry = Entry {
        // `BusName` borrows from `name`, so an entry that outlives this call
        // has to own its class.
        class: Some(parsed.class.to_string()),
        connected: true,
        instance: probe.instance.unwrap_or(FALLBACK_INSTANCE),
        conformance,
    };
    publish(service, registry.record(name, entry)).await;
}

/// Mark `name` disconnected and publish, if the registry carries it at all.
async fn record_disappearance(
    registry: &Registry,
    service: &InterfaceRef<MicadService>,
    name: &str,
) {
    if let Some(snapshot) = registry.disconnect(name) {
        tracing::info!(service = name, "service disconnected; entry retained");
        publish(service, snapshot).await;
    }
}

/// Write a registry snapshot into the live-state tree.
async fn publish(service: &InterfaceRef<MicadService>, snapshot: Json) {
    service.get().await.publish_services(snapshot).await;
}

/// The match rule the scan listens on: `NameOwnerChanged` from the bus itself,
/// narrowed to our namespace with `arg0namespace`, so the **bus** discards
/// every name that is not ours and micad is never woken to discard it here.
///
/// The namespace is [`mica_busname::PREFIX`] without its trailing dot rather
/// than a literal of its own — `arg0namespace` matches on whole dotted
/// components, which is the same boundary the parser and the D-Bus policy
/// draw, so `com.mica.extra` is matched (it is ours) and `com.mosquitto` is
/// not.
fn name_owner_changed_rule() -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg0ns(mica_busname::PREFIX.trim_end_matches('.'))?
        .build())
}

/// Watch the bus and keep the registry in step with it, for as long as the
/// connection lives.
///
/// Ordering is deliberate: the match rule is installed BEFORE the bus is asked
/// which names already exist, so a service that claims its name during startup
/// is caught by the signal even if it missed the sweep. A name seen twice is
/// simply probed twice; the entry is keyed by name and the second answer wins.
///
/// Never constructed under `MICAD_DRY_RUN=1` (see `main.rs`).
pub async fn run(
    connection: Connection,
    registry: Arc<Registry>,
    service: InterfaceRef<MicadService>,
) {
    let rule = match name_owner_changed_rule() {
        Ok(rule) => rule,
        Err(err) => {
            tracing::error!(error = %err, "service scan: could not build the match rule");
            return;
        }
    };
    let stream = match MessageStream::for_match_rule(rule, &connection, None).await {
        Ok(stream) => stream,
        Err(err) => {
            // No refusal to serve: the rest of the daemon is unaffected by a
            // bus that will not take a match rule, and an operator who can
            // still read settings can still be told the registry is not there.
            tracing::error!(error = %err, "service scan: could not subscribe to NameOwnerChanged; \
                 the service registry will stay empty");
            return;
        }
    };
    // The names that were already there. Published even when it finds nothing,
    // so `services` exists from startup: an empty registry is an answer, a
    // missing key is not.
    match fdo::DBusProxy::new(&connection).await {
        Ok(dbus) => match dbus.list_names().await {
            Ok(names) => {
                for name in names {
                    record_appearance(&connection, &registry, &service, name.as_str()).await;
                }
            }
            Err(err) => tracing::warn!(error = %err, "service scan: ListNames failed; \
                 only services appearing from now on will be registered"),
        },
        Err(err) => tracing::warn!(error = %err, "service scan: no bus proxy; \
             only services appearing from now on will be registered"),
    }
    publish(&service, registry.snapshot()).await;

    let mut stream = pin!(stream);
    while let Some(message) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        let Ok(message) = message else {
            continue;
        };
        let body = message.body();
        // `NameOwnerChanged` is `sss`, with an empty string for "no owner" at
        // either end. Deserialized as plain strings so that the empty owner
        // stays representable, which is the whole signal here.
        let Ok((name, _old_owner, new_owner)) = body.deserialize::<(String, String, String)>()
        else {
            tracing::debug!("NameOwnerChanged with an unexpected body; ignored");
            continue;
        };
        if new_owner.is_empty() {
            record_disappearance(&registry, &service, &name).await;
        } else {
            record_appearance(&connection, &registry, &service, &name).await;
        }
    }
    tracing::info!("service scan stopped: the bus connection ended");
}

#[cfg(test)]
mod tests {
    use super::{Conformance, Entry, Registry, as_i64, name_owner_changed_rule};
    use zbus::zvariant::Value;

    /// The rule the BUS is given, in the form it is given it: the filtering
    /// happens on the daemon's side of the socket, not on ours, and the only
    /// way to see that is the rule string `AddMatch` carries.
    #[test]
    fn the_bus_filters_on_the_namespace() {
        let rule = name_owner_changed_rule()
            .expect("a valid match rule")
            .to_string();
        assert!(
            rule.contains("arg0namespace='com.mica'"),
            "the bus must do the namespace filtering: {rule}"
        );
        assert!(rule.contains("member='NameOwnerChanged'"), "{rule}");
        assert!(rule.contains("interface='org.freedesktop.DBus'"), "{rule}");
        assert!(rule.contains("type='signal'"), "{rule}");
    }

    fn entry(class: Option<&str>, instance: i64, connected: bool) -> Entry {
        Entry {
            class: class.map(str::to_string),
            connected,
            instance,
            conformance: Conformance::default(),
        }
    }

    /// A conforming service carries an EMPTY conformance object — the shape an
    /// operator reads as "nothing missing".
    #[test]
    fn a_clean_conformance_is_an_empty_object() {
        let clean = Conformance::default();
        assert!(clean.is_clean());
        assert_eq!(clean.to_json(), serde_json::json!({}));
    }

    /// Every gap has a name, and the names are the ones the bus contract uses.
    #[test]
    fn every_gap_names_itself() {
        let gaps = Conformance {
            no_item1: true,
            no_device_instance: true,
            missing_paths: vec!["/DeviceInstance", "/ProductId"],
        };
        assert!(!gaps.is_clean());
        assert_eq!(
            gaps.to_json(),
            serde_json::json!({
                "item1": false,
                "device_instance": false,
                "missing_paths": ["/DeviceInstance", "/ProductId"],
            })
        );
    }

    /// Two connected services of one class on one instance: BOTH marked, and
    /// both still there.
    #[test]
    fn a_shared_instance_marks_both_sides() {
        let registry = Registry::new();
        registry.record("com.mica.sensor.one", entry(Some("sensor"), 0, true));
        let snapshot = registry.record("com.mica.sensor.two", entry(Some("sensor"), 0, true));

        assert_eq!(snapshot["com.mica.sensor.one"]["instance_collision"], true);
        assert_eq!(snapshot["com.mica.sensor.two"]["instance_collision"], true);
    }

    /// The neighbouring cases: a different class, a different instance, and a
    /// service that is no longer connected, none of which collide.
    #[test]
    fn what_does_not_collide() {
        let registry = Registry::new();
        registry.record("com.mica.sensor.one", entry(Some("sensor"), 0, true));
        registry.record("com.mica.meter.two", entry(Some("meter"), 0, true));
        registry.record("com.mica.sensor.three", entry(Some("sensor"), 1, true));
        let snapshot = registry.record("com.mica.sensor.gone", entry(Some("sensor"), 0, false));

        for name in [
            "com.mica.sensor.one",
            "com.mica.meter.two",
            "com.mica.sensor.three",
            "com.mica.sensor.gone",
        ] {
            assert_eq!(
                snapshot[name]["instance_collision"], false,
                "{name} does not collide with anything"
            );
        }
    }

    /// Retention and removal: a vanished service stays, `ForgetService`'s
    /// backing call drops it, and a connected one is refused.
    #[test]
    fn forget_takes_a_disconnected_entry_and_refuses_a_connected_one() {
        let registry = Registry::new();
        registry.record("com.mica.sensor.fake", entry(Some("sensor"), 3, true));

        let refused = registry
            .forget("com.mica.sensor.fake")
            .expect_err("a connected service must not be forgettable");
        assert!(
            refused.to_string().contains("still connected"),
            "the refusal must say why: {refused}"
        );

        let snapshot = registry
            .disconnect("com.mica.sensor.fake")
            .expect("the entry is there");
        assert_eq!(snapshot["com.mica.sensor.fake"]["connected"], false);

        let snapshot = registry
            .forget("com.mica.sensor.fake")
            .expect("a disconnected entry can be forgotten");
        assert_eq!(snapshot, serde_json::json!({}));

        assert!(
            registry.forget("com.mica.sensor.fake").is_err(),
            "forgetting what is already gone is an error, not a silent no-op"
        );
    }

    /// A name the registry never saw is not silently accepted.
    #[test]
    fn forgetting_an_unknown_name_is_refused() {
        let registry = Registry::new();
        let err = registry
            .forget("com.mica.sensor.never")
            .expect_err("unknown name");
        assert!(
            err.to_string().contains("no service registry entry"),
            "{err}"
        );
    }

    /// An instance is an integer whatever variant it arrived in, and is not
    /// invented from something that is not one.
    #[test]
    fn an_instance_is_read_out_of_any_integer_variant() {
        assert_eq!(as_i64(&Value::from(7i32)), Some(7));
        assert_eq!(as_i64(&Value::from(7u32)), Some(7));
        assert_eq!(as_i64(&Value::from(7i64)), Some(7));
        assert_eq!(as_i64(&Value::from(7u8)), Some(7));
        assert_eq!(as_i64(&Value::from(7.0f64)), Some(7));
        // A variant inside a variant, which is what an item whose `value`
        // attribute was built by hand rather than by zvariant can arrive as.
        assert_eq!(as_i64(&Value::Value(Box::new(Value::from(7i32)))), Some(7));

        assert_eq!(as_i64(&Value::from(7.5f64)), None);
        assert_eq!(as_i64(&Value::from("7")), None);
        assert_eq!(as_i64(&Value::from(true)), None);
    }
}

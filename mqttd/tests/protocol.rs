//! Protocol tests for the MQTT bridge.
//!
//! These drive the production path — the real [`Bridge`], the real payload
//! encoder and masker, the real [`apply`] — against an in-memory
//! [`Transport`] and [`ItemSource`], per the decision recorded in the crate
//! docs. No broker, no D-Bus daemon, no filesystem: **none of these tests can
//! skip**, which is deliberate, because a test that skips reports green while
//! asserting nothing.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use rumqttc::{AsyncClient, MqttOptions};
use serde_json::{Value as Json, json};

use mos_mqttd::bridge::{Bridge, Effects, Publication};
use mos_mqttd::config::{Mode, Timings};
use mos_mqttd::enrollment::Enrollment;
use mos_mqttd::item::Item;
use mos_mqttd::runtime::{ReconnectBackoff, apply};
use mos_mqttd::source::{ItemSource, WriteOutcome};
use mos_mqttd::topic::{self, Address, Request};
use mos_mqttd::transport::Transport;

/// The device identity supplied as root-rendered runtime configuration.
const DEVICE: &str = "abc123";
/// One exact service enrolled by its application package.
const APPLICATION_SERVICE: &str = "com.mos.sensor.abc123";
/// The class [`APPLICATION_SERVICE`] must publish under.
const APPLICATION_CLASS: &str = "sensor";
const CLASS: &str = APPLICATION_CLASS;

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

/// Everything an item notification for `path` is addressed by.
fn notify(path: &str) -> String {
    format!("N/{DEVICE}/{CLASS}/0{path}")
}

/// A representative application tree. Device identity is deliberately absent:
/// it is process runtime configuration, outside every publishable item tree.
fn tree() -> BTreeMap<String, Item> {
    BTreeMap::from([
        ("/DeviceInstance".to_string(), Item::new(json!(0))),
        ("/Temperature".to_string(), Item::new(json!(21))),
        ("/SampleCount".to_string(), Item::new(json!(42))),
        ("/Enabled".to_string(), Item::writable(json!(true))),
        ("/Calibration/offset".to_string(), Item::writable(json!(0))),
    ])
}

fn application() -> topic::Application {
    application_named(APPLICATION_SERVICE)
}

fn application_named(bus_name: &str) -> topic::Application {
    Enrollment::from_names([bus_name])
        .expect("fixture is a valid enrollment")
        .application(bus_name)
        .expect("fixture name is enrolled")
}

/// A [`Transport`] that records instead of connecting.
#[derive(Default)]
struct Recorder {
    published: Mutex<Vec<Publication>>,
    filters: Mutex<Vec<String>>,
}

#[async_trait]
impl Transport for Recorder {
    async fn publish(&self, publication: &Publication) -> anyhow::Result<()> {
        self.published.lock().unwrap().push(publication.clone());
        Ok(())
    }

    async fn subscribe(&self, filter: &str) -> anyhow::Result<()> {
        self.filters.lock().unwrap().push(filter.to_string());
        Ok(())
    }

    async fn unsubscribe(&self, filter: &str) -> anyhow::Result<()> {
        self.filters.lock().unwrap().retain(|held| held != filter);
        Ok(())
    }
}

impl Recorder {
    fn topics(&self) -> Vec<String> {
        self.published
            .lock()
            .unwrap()
            .iter()
            .map(|publication| publication.topic.clone())
            .collect()
    }

    /// The decoded payload of the one publication on `topic`.
    fn payload(&self, topic: &str) -> Json {
        let published = self.published.lock().unwrap();
        // Built before the lookup, not inside the panic: a `self.topics()`
        // there would take the same lock a second time and hang the test
        // instead of failing it.
        let seen: Vec<&str> = published
            .iter()
            .map(|publication| publication.topic.as_str())
            .collect();
        let mut matching = published
            .iter()
            .filter(|publication| publication.topic == topic);
        let publication = matching
            .next()
            .unwrap_or_else(|| panic!("nothing published on {topic}; saw {seen:?}"));
        assert!(
            matching.next().is_none(),
            "{topic} was published more than once"
        );
        serde_json::from_slice(&publication.payload).expect("payload is JSON")
    }

    /// The decoded payload of the most recent publication on `topic`, for
    /// topics a test expects more than one of.
    fn last_payload(&self, topic: &str) -> Json {
        let published = self.published.lock().unwrap();
        let publication = published
            .iter()
            .rev()
            .find(|publication| publication.topic == topic)
            .unwrap_or_else(|| panic!("nothing published on {topic}"));
        serde_json::from_slice(&publication.payload).expect("payload is JSON")
    }

    fn count(&self, topic: &str) -> usize {
        self.published
            .lock()
            .unwrap()
            .iter()
            .filter(|publication| publication.topic == topic)
            .count()
    }

    fn clear(&self) {
        self.published.lock().unwrap().clear();
    }
}

/// An [`ItemSource`] that records writes instead of making them.
struct Fake {
    items: BTreeMap<String, Item>,
    writes: Mutex<Vec<(String, String, Json)>>,
    outcome: WriteOutcome,
}

impl Fake {
    fn new() -> Self {
        Self {
            items: tree(),
            writes: Mutex::new(Vec::new()),
            outcome: WriteOutcome::Accepted,
        }
    }

    fn writes(&self) -> Vec<(String, String, Json)> {
        self.writes.lock().unwrap().clone()
    }
}

#[async_trait]
impl ItemSource for Fake {
    async fn get_items(
        &self,
        application: &topic::Application,
    ) -> anyhow::Result<BTreeMap<String, Item>> {
        assert_eq!(application.bus_name(), APPLICATION_SERVICE);
        Ok(self.items.clone())
    }

    async fn set_value(
        &self,
        application: &topic::Application,
        path: &str,
        value: Json,
    ) -> WriteOutcome {
        self.writes.lock().unwrap().push((
            application.bus_name().to_string(),
            path.to_string(),
            value.clone(),
        ));
        self.outcome.clone()
    }
}

/// Bridge, transport and source wired exactly as the daemon wires them.
struct Harness {
    bridge: Bridge,
    transport: Recorder,
    source: Fake,
}

impl Harness {
    fn new(mode: Mode) -> Self {
        Self {
            bridge: Bridge::new(DEVICE, mode, Timings::default()),
            transport: Recorder::default(),
            source: Fake::new(),
        }
    }

    /// Carry out one event's effects through the production [`apply`].
    async fn run(&self, effects: Effects) {
        apply(effects, &self.transport, &self.source)
            .await
            .expect("effects applied");
    }

    /// The startup path: `GetItems` into the mirror, then a keepalive to open
    /// the alive window every later publication is gated on.
    async fn start(&mut self, now: Duration) {
        let application = application();
        let items = self.source.get_items(&application).await.expect("seed");
        let effects = self.bridge.upsert_service(now, application, items);
        self.run(effects).await;
        let effects = self.bridge.on_keepalive(now);
        self.run(effects).await;
    }
}

/// (a) N, R and W each map to what the grammar says they map to.
#[tokio::test]
async fn verbs_map_to_notify_read_and_write() {
    let mut harness = Harness::new(Mode::Full);
    harness.start(secs(0)).await;

    // N: the full republish carries every item under its own topic.
    assert_eq!(
        harness.transport.payload(&notify("/Temperature")),
        json!({"value": 21})
    );
    assert_eq!(
        harness.transport.payload(&notify("/Enabled")),
        json!({"value": true})
    );
    assert_eq!(
        harness.transport.payload(&notify("/Calibration/offset")),
        json!({"value": 0})
    );
    assert_eq!(
        harness
            .transport
            .payload(&format!("N/{DEVICE}/full_publish_completed")),
        json!({"value": 5}),
        "the marker reports how many items preceded it"
    );

    // N: one item change publishes exactly that item, and nothing else.
    harness.transport.clear();
    let effects = harness.bridge.on_items_changed(
        secs(1),
        APPLICATION_SERVICE,
        BTreeMap::from([("/Temperature".to_string(), Some(Item::new(json!(22))))]),
    );
    harness.run(effects).await;
    assert_eq!(harness.transport.topics(), vec![notify("/Temperature")]);
    assert_eq!(
        harness.transport.payload(&notify("/Temperature")),
        json!({"value": 22})
    );
    assert!(
        harness.transport.published.lock().unwrap()[0].retain,
        "item notifications are retained so a late subscriber still finds the tree"
    );

    // R: the addressed item is republished on its N topic.
    harness.transport.clear();
    let effects =
        harness
            .bridge
            .on_request(secs(2), &format!("R/{DEVICE}/{CLASS}/0/SampleCount"), b"");
    harness.run(effects).await;
    assert_eq!(harness.transport.topics(), vec![notify("/SampleCount")]);
    assert_eq!(
        harness.transport.payload(&notify("/SampleCount")),
        json!({"value": 42})
    );

    // W: the addressed item is written, and nothing is published for it.
    harness.transport.clear();
    let effects = harness.bridge.on_request(
        secs(3),
        &format!("W/{DEVICE}/{CLASS}/0/Enabled"),
        br#"{"value": false}"#,
    );
    harness.run(effects).await;
    assert_eq!(
        harness.source.writes(),
        vec![(
            APPLICATION_SERVICE.to_string(),
            "/Enabled".to_string(),
            json!(false)
        )]
    );
    assert!(harness.transport.topics().is_empty());

    // An item that became invalid publishes the JSON form of the bus sentinel,
    // so a subscriber sees the transition rather than inferring it.
    harness.transport.clear();
    let effects = harness.bridge.on_items_changed(
        secs(4),
        APPLICATION_SERVICE,
        BTreeMap::from([("/SampleCount".to_string(), None)]),
    );
    harness.run(effects).await;
    assert_eq!(
        harness.transport.payload(&notify("/SampleCount")),
        json!({ "value": Json::Null })
    );

    // Topics belonging to someone else are not instructions to this bridge.
    harness.transport.clear();
    for foreign in [
        format!("R/other-device/{CLASS}/0/Temperature"),
        format!("R/{DEVICE}/meter/0/Temperature"),
        format!("R/{DEVICE}/{CLASS}/7/Temperature"),
        notify("/Temperature"),
    ] {
        let effects = harness.bridge.on_request(secs(5), &foreign, b"");
        assert_eq!(effects, Effects::default(), "{foreign} was acted on");
    }
}

/// (b) A keepalive triggers a full republish, and a storm of them does not
/// multiply it.
#[tokio::test]
async fn keepalive_republishes_fully_and_is_rate_limited() {
    let mut harness = Harness::new(Mode::Full);
    let marker = format!("N/{DEVICE}/full_publish_completed");

    // Nothing at all before the first keepalive: publishing is gated on the
    // alive window.
    let application = application();
    let items = harness.source.get_items(&application).await.expect("seed");
    let effects = harness.bridge.upsert_service(secs(0), application, items);
    harness.run(effects).await;
    assert!(
        harness.transport.topics().is_empty(),
        "the bridge published before any keepalive armed it"
    );

    // Ten keepalives inside the rate limit's floor.
    for tenth in 0..10 {
        let now = Duration::from_millis(tenth * 100);
        let effects = harness
            .bridge
            .on_request(now, &format!("R/{DEVICE}/keepalive"), b"");
        harness.run(effects).await;
    }
    assert_eq!(
        harness.transport.count(&marker),
        1,
        "a keepalive storm produced more than one full republish"
    );
    assert_eq!(
        harness.transport.count(&notify("/Temperature")),
        1,
        "the storm republished the tree more than once"
    );

    // Every one of them renewed the window, which is what must not be
    // throttled: the last keepalive landed at 0.9 s, so the window runs to
    // 60.9 s.
    assert!(harness.bridge.alive(secs(60)));
    assert!(!harness.bridge.alive(secs(61)));

    // The republishes the floor swallowed coalesce into exactly one, once the
    // floor has passed.
    let effects = harness.bridge.on_tick(secs(5));
    harness.run(effects).await;
    assert_eq!(
        harness.transport.count(&marker),
        2,
        "the deferred republish did not coalesce into exactly one"
    );

    // The heartbeat runs at 3 s while alive: the tick above beat at 5 s, so
    // the next is due at 8 s and the one after at 11 s, and the ticks in
    // between produce nothing.
    harness.transport.clear();
    let beat = format!("N/{DEVICE}/heartbeat");
    for (tick, expected) in [
        (secs(6), 0),
        (secs(8), 1),
        (secs(9), 1),
        (secs(10), 1),
        (secs(11), 2),
    ] {
        let effects = harness.bridge.on_tick(tick);
        harness.run(effects).await;
        assert_eq!(
            harness.transport.count(&beat),
            expected,
            "heartbeat cadence is wrong at {tick:?}"
        );
    }
    assert_eq!(
        harness.transport.last_payload(&beat),
        json!({"value": 11}),
        "the beat carries the bridge's uptime in seconds"
    );
    harness.transport.clear();
    let effects = harness.bridge.on_tick(secs(120));
    harness.run(effects).await;
    assert!(
        harness.transport.topics().is_empty(),
        "the bridge kept publishing after the alive window expired"
    );
}

/// (c) Secrets are masked at publish, structurally, at any depth.
#[tokio::test]
async fn secrets_are_masked_at_publish() {
    const SECRET: &str = "s3cr3t-never-on-the-wire";

    let mut harness = Harness::new(Mode::Full);
    // Applications are responsible for their own source-side redaction; the
    // bridge's structural masking is an independent last line of defence.
    harness
        .source
        .items
        .insert("/Credentials/hash".to_string(), Item::new(json!(SECRET)));
    harness.source.items.insert(
        "/Networks".to_string(),
        Item::new(json!([
            {"ssid": "home", "psk": SECRET},
            {"ssid": "field", "password_hash": SECRET, "nested": {"passwordHash": SECRET}},
        ])),
    );
    harness.start(secs(0)).await;

    // A secret-named path is the secret, so it publishes as invalid rather
    // than as a masked value.
    assert_eq!(
        harness.transport.payload(&notify("/Credentials/hash")),
        json!({ "value": Json::Null })
    );
    // A secret-named key inside a value is stripped, and its siblings are not.
    assert_eq!(
        harness.transport.payload(&notify("/Networks")),
        json!({"value": [
            {"ssid": "home"},
            {"ssid": "field", "nested": {}},
        ]})
    );
    // And, the assertion that does not depend on knowing where to look: the
    // secret is in no payload this bridge produced, anywhere.
    for publication in harness.transport.published.lock().unwrap().iter() {
        let payload = String::from_utf8_lossy(&publication.payload);
        assert!(
            !payload.contains(SECRET),
            "{} carried the secret: {payload}",
            publication.topic
        );
    }
}

/// (d) Read-only mode refuses W, and full mode does not — the same request,
/// so the refusal cannot be an accident of the fixture.
#[tokio::test]
async fn read_only_mode_refuses_writes() {
    let topic = format!("W/{DEVICE}/{CLASS}/0/Enabled");
    let payload = br#"{"value": false}"#;

    let mut read_only = Harness::new(Mode::ReadOnly);
    read_only.start(secs(0)).await;
    read_only.transport.clear();
    let effects = read_only.bridge.on_request(secs(1), &topic, payload);
    read_only.run(effects).await;
    assert!(
        read_only.source.writes().is_empty(),
        "a read-only bridge carried a write through to SetValue"
    );
    assert!(read_only.transport.topics().is_empty());
    assert_eq!(
        read_only.bridge.subscriptions(),
        vec![format!("R/{DEVICE}/#")],
        "a read-only bridge subscribed to W topics"
    );

    let mut full = Harness::new(Mode::Full);
    full.start(secs(0)).await;
    let effects = full.bridge.on_request(secs(1), &topic, payload);
    full.run(effects).await;
    assert_eq!(
        full.source.writes(),
        vec![(
            APPLICATION_SERVICE.to_string(),
            "/Enabled".to_string(),
            json!(false)
        )],
        "the same request must reach the bus in full mode"
    );
    assert_eq!(
        full.bridge.subscriptions(),
        vec![format!("R/{DEVICE}/#"), format!("W/{DEVICE}/#")]
    );
}

/// A refused write puts nothing on the wire.
///
/// `SetValue` result codes are deliberately not part of the MQTT grammar. A
/// client observes a successful write through the application's subsequent
/// `ItemsChanged`; a refusal is the absence of that update.
#[tokio::test]
async fn a_refused_write_publishes_nothing() {
    for (path, outcome) in [
        ("/Enabled", WriteOutcome::Refused { code: -2 }),
        ("/Calibration/offset", WriteOutcome::UnknownObject),
    ] {
        let mut harness = Harness::new(Mode::Full);
        harness.source.outcome = outcome;
        harness.start(secs(0)).await;
        harness.transport.clear();

        let effects = harness.bridge.on_request(
            secs(1),
            &format!("W/{DEVICE}/{CLASS}/0{path}"),
            br#"{"value": 1}"#,
        );
        harness.run(effects).await;

        assert_eq!(
            harness.source.writes(),
            vec![(APPLICATION_SERVICE.to_string(), path.to_string(), json!(1))],
            "the request must still reach SetValue"
        );
        assert!(
            harness.transport.topics().is_empty(),
            "a refused write on {path} was reported on the wire"
        );
    }
}

/// (e) An application that leaves the bus has its retained state cleared.
#[tokio::test]
async fn a_vanished_application_clears_its_retained_state() {
    let mut harness = Harness::new(Mode::Full);
    harness.start(secs(0)).await;
    let published: Vec<String> = harness
        .transport
        .topics()
        .into_iter()
        .filter(|topic| topic.contains(CLASS))
        .collect();
    assert_eq!(published.len(), 5, "the fixture tree published five items");

    harness.transport.clear();
    let effects = harness
        .bridge
        .on_service_vanished(secs(1), APPLICATION_SERVICE);
    harness.run(effects).await;

    let mut cleared = harness.transport.topics();
    cleared.sort();
    let mut expected = published;
    expected.sort();
    assert_eq!(
        cleared, expected,
        "the clear did not cover every item topic"
    );
    for publication in harness.transport.published.lock().unwrap().iter() {
        assert!(
            publication.payload.is_empty(),
            "{} was cleared with a payload, which sets state instead of deleting it",
            publication.topic
        );
        assert!(
            publication.retain,
            "{} was cleared without the retain flag, so the retained message survives",
            publication.topic
        );
    }
}

/// An application that vanishes while the bridge is silent still owes clears,
/// and pays them at the next keepalive.
///
/// The alive gate makes the vanish itself silent, and the vanish takes the
/// so the pending deletes must survive until the next keepalive.
#[tokio::test]
async fn clears_owed_while_silent_are_paid_at_the_next_keepalive() {
    let mut harness = Harness::new(Mode::Full);
    harness.start(secs(0)).await;
    let published: Vec<String> = harness
        .transport
        .topics()
        .into_iter()
        .filter(|topic| topic.contains(CLASS))
        .collect();

    // The window shuts, and only then does the device leave the bus.
    harness.transport.clear();
    let effects = harness
        .bridge
        .on_service_vanished(secs(120), APPLICATION_SERVICE);
    harness.run(effects).await;
    assert!(
        harness.transport.topics().is_empty(),
        "the bridge published after the alive window expired"
    );

    // The keepalive still parses — the device id outlives the process that
    // reported it — so the window reopens and the owed clears go out.
    let effects = harness
        .bridge
        .on_request(secs(121), &format!("R/{DEVICE}/keepalive"), b"");
    harness.run(effects).await;
    let mut cleared: Vec<String> = harness
        .transport
        .topics()
        .into_iter()
        .filter(|topic| topic.contains(CLASS))
        .collect();
    cleared.sort();
    let mut expected = published;
    expected.sort();
    assert_eq!(cleared, expected, "the owed clears were never paid");
    assert_eq!(
        harness
            .transport
            .payload(&format!("N/{DEVICE}/full_publish_completed")),
        json!({"value": 0}),
        "the republish after the application vanished carries no items"
    );
}

/// A direct service publishes under the third component of its exact name.
#[tokio::test]
async fn a_direct_service_publishes_under_its_class() {
    let mut harness = Harness::new(Mode::Full);
    harness.start(secs(0)).await;

    let published = harness.transport.topics();
    assert!(
        published.contains(&format!("N/{DEVICE}/{APPLICATION_CLASS}/0/Temperature")),
        "{APPLICATION_SERVICE} did not publish under its class {APPLICATION_CLASS}; saw {published:?}"
    );
}

#[test]
fn invalid_application_paths_never_reach_mqtt_topics() {
    let mut bridge = Bridge::new(DEVICE, Mode::Full, Timings::default());
    bridge.upsert_service(
        secs(0),
        application(),
        BTreeMap::from([
            ("/DeviceInstance".to_string(), Item::new(json!(0))),
            ("relative".to_string(), Item::new(json!(1))),
            ("/wild/#".to_string(), Item::new(json!(2))),
        ]),
    );
    let topics: Vec<String> = bridge
        .on_keepalive(secs(0))
        .publications
        .into_iter()
        .map(|publication| publication.topic)
        .collect();

    assert!(topics.contains(&"N/abc123/sensor/0/DeviceInstance".to_string()));
    assert!(
        topics
            .iter()
            .all(|topic| !topic.contains("relative") && !topic.contains('#')),
        "an application-controlled invalid object path reached MQTT: {topics:?}"
    );
    assert!(
        bridge
            .on_items_changed(
                secs(1),
                APPLICATION_SERVICE,
                BTreeMap::from([("/bad/+".to_string(), Some(Item::new(json!(3))))]),
            )
            .publications
            .is_empty(),
        "an invalid ItemsChanged key reached MQTT"
    );
}

#[test]
fn mqtt_topic_identity_and_item_path_inputs_are_strict() {
    for segment in [
        "",
        "device/other",
        "device+",
        "device#",
        "device other",
        "device$other",
        "设备",
        "device\0other",
        "device\nother",
    ] {
        assert!(
            !topic::valid_topic_segment(segment),
            "invalid MQTT segment was accepted: {segment:?}"
        );
    }
    assert!(topic::valid_topic_segment(DEVICE));

    for request in [
        format!("R/{DEVICE}/{CLASS}/0/not-an-object-path"),
        format!("W/{DEVICE}/{CLASS}/0/bad-path"),
    ] {
        assert_eq!(
            topic::parse(&request, DEVICE),
            None,
            "invalid D-Bus path was accepted: {request}"
        );
    }
}

#[test]
fn multiple_applications_share_one_device_liveness_protocol() {
    let sensor = application();
    let meter = application_named("com.mos.meter.abc123");
    let mut bridge = Bridge::new(DEVICE, Mode::Full, Timings::default());
    bridge.upsert_service(
        secs(0),
        sensor,
        BTreeMap::from([
            ("/DeviceInstance".to_string(), Item::new(json!(0))),
            ("/Temperature".to_string(), Item::new(json!(21))),
        ]),
    );
    bridge.upsert_service(
        secs(0),
        meter,
        BTreeMap::from([
            ("/DeviceInstance".to_string(), Item::new(json!(2))),
            ("/Power".to_string(), Item::new(json!(900))),
        ]),
    );

    let full = bridge.on_keepalive(secs(0));
    let topics: Vec<&str> = full
        .publications
        .iter()
        .map(|publication| publication.topic.as_str())
        .collect();
    assert!(topics.contains(&"N/abc123/sensor/0/Temperature"));
    assert!(topics.contains(&"N/abc123/meter/2/Power"));
    assert_eq!(
        topics
            .iter()
            .filter(|topic| **topic == "N/abc123/full_publish_completed")
            .count(),
        1,
        "a device-wide full publish has one completion marker"
    );
    assert_eq!(
        bridge
            .on_tick(secs(3))
            .publications
            .iter()
            .filter(|publication| publication.topic == "N/abc123/heartbeat")
            .count(),
        1,
        "multiple applications must not duplicate the device heartbeat"
    );
}

#[test]
fn a_class_instance_collision_fails_closed_for_publication_and_control() {
    let first = application();
    let second = application_named("com.mos.sensor.second");
    let items = BTreeMap::from([
        ("/DeviceInstance".to_string(), Item::new(json!(0))),
        ("/Enabled".to_string(), Item::writable(json!(true))),
    ]);
    let mut bridge = Bridge::new(DEVICE, Mode::Full, Timings::default());
    bridge.upsert_service(secs(0), first, items.clone());
    bridge.on_keepalive(secs(0));

    let collision = bridge.upsert_service(secs(1), second, items);
    assert!(
        collision
            .publications
            .iter()
            .all(|publication| publication.payload.is_empty()),
        "introducing a collision may clear old retained state but must publish neither claimant"
    );
    assert!(
        bridge
            .on_request(secs(2), "W/abc123/sensor/0/Enabled", br#"{"value": false}"#,)
            .writes
            .is_empty(),
        "an ambiguous write must not reach either application"
    );

    let restored = bridge.on_service_vanished(secs(5), "com.mos.sensor.second");
    assert!(
        restored
            .publications
            .iter()
            .any(|publication| publication.topic == "N/abc123/sensor/0/Enabled"),
        "the remaining application must become publishable when the collision clears"
    );
}

/// Building and parsing agree for a direct application's address.
#[test]
fn an_application_topic_round_trips() {
    let address = Address {
        device_id: DEVICE.to_string(),
        class: topic::class_of(APPLICATION_SERVICE)
            .expect("a com.mos.* bus name")
            .to_string(),
        instance: 0,
    };

    let read = address.item_topic(topic::READ, "/SampleCount");
    assert_eq!(
        read,
        format!("R/{DEVICE}/{APPLICATION_CLASS}/0/SampleCount")
    );
    assert_eq!(
        topic::parse(&read, DEVICE),
        Some(Request::Read {
            class: APPLICATION_CLASS.to_string(),
            instance: 0,
            path: "/SampleCount".to_string()
        })
    );

    let under_ext = format!("R/{DEVICE}/ext/0/SampleCount");
    assert_eq!(
        topic::parse(&under_ext, DEVICE),
        Some(Request::Read {
            class: "ext".to_string(),
            instance: 0,
            path: "/SampleCount".to_string()
        }),
        "the grammar parses an address before the application bridge decides whether a service owns it"
    );
}

/// `ext` is an ordinary direct class now; only the bare `com.mos` namespace
/// has no class.
#[test]
fn direct_name_classification_has_no_extension_special_case() {
    assert_eq!(topic::class_of("com.mos.ext"), Some("ext"));
    assert_eq!(topic::class_of("com.mos.ext.sensor"), Some("ext"));
    assert_eq!(topic::class_of("com.mos"), None);
}

// ---------------------------------------------------------------------------
// Reconnect backoff
// ---------------------------------------------------------------------------

/// The schedule itself: doubling from the floor, capped, and reset by any
/// successful poll.
#[test]
fn reconnect_backoff_doubles_to_a_ceiling_and_resets() {
    let mut backoff = ReconnectBackoff::default();
    let climb: Vec<u64> = (0..8).map(|_| backoff.next_delay().as_secs()).collect();
    assert_eq!(
        climb,
        vec![1, 2, 4, 8, 16, 30, 30, 30],
        "the delay must double from 1s and then hold at the 30s ceiling; a backoff that \
         keeps doubling eventually stops retrying a broker that is merely slow to come back"
    );

    backoff.reset();
    assert_eq!(
        backoff.next_delay(),
        Duration::from_secs(1),
        "a connection that worked must not inherit the penalty of the outage before it"
    );
}

/// The FIRST delay is what stops the spin, so it is asserted on its own: a
/// backoff whose floor drifted to zero is a backoff that does nothing, and
/// every other assertion in the test above still passes.
#[test]
fn the_first_reconnect_delay_is_not_zero() {
    let first = ReconnectBackoff::default().next_delay();
    assert!(
        first >= Duration::from_millis(500),
        "the first retry delay is {first:?}; a refused connection returns in microseconds, so \
         anything near zero is still a spin loop"
    );
}

/// The PREMISE the backoff exists for, held as a test against the real
/// upstream event loop.
///
/// `rumqttc::EventLoop::poll` reconnects with no delay of its own. This is
/// asserted rather than trusted, because the whole justification for the
/// backoff in `runtime.rs` is that upstream lacks one: if a future rumqttc
/// grows a reconnect delay, this test fails and says so at the exact place
/// the workaround is described, instead of leaving a second backoff stacked
/// silently on top of theirs.
///
/// Port 1 on loopback with nothing listening: `connect` returns ECONNREFUSED
/// immediately. No broker, no network, no skip.
#[tokio::test]
async fn rumqttc_reconnects_with_no_delay_of_its_own() {
    let mut options = MqttOptions::new("mos-mqttd-backoff-premise", "127.0.0.1", 1);
    options.set_keep_alive(Duration::from_secs(30));
    let (_client, mut eventloop) = AsyncClient::new(options, 8);

    let started = std::time::Instant::now();
    let mut refusals = 0;
    for _ in 0..5 {
        if eventloop.poll().await.is_err() {
            refusals += 1;
        }
    }
    let elapsed = started.elapsed();

    assert_eq!(
        refusals, 5,
        "nothing listens on 127.0.0.1:1, so every poll must fail; if they succeeded this \
         test is measuring something other than a refused connection"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "five refused reconnects took {elapsed:?}. rumqttc now delays between attempts, so \
         runtime.rs's ReconnectBackoff is stacked on top of an upstream backoff and should \
         be reconsidered -- see the RECONNECT_BACKOFF_MIN docs"
    );
}

/// An application that accepts a call and never answers must not hold the
/// bridge: the bound is applied above the bus, so a hung `GetItems` is an
/// error and a hung `SetValue` is an unreachable write, both within the
/// configured window.
struct Hung;

#[async_trait]
impl ItemSource for Hung {
    async fn get_items(
        &self,
        _application: &topic::Application,
    ) -> anyhow::Result<BTreeMap<String, Item>> {
        std::future::pending().await
    }

    async fn set_value(
        &self,
        _application: &topic::Application,
        _path: &str,
        _value: Json,
    ) -> WriteOutcome {
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_hung_application_is_bounded_by_the_call_timeout() {
    let source = mos_mqttd::source::Bounded::new(Hung, Duration::from_millis(50));
    let application = application();

    let started = std::time::Instant::now();
    let items = source.get_items(&application).await;
    let outcome = source
        .set_value(&application, "/Enabled", json!(true))
        .await;
    let elapsed = started.elapsed();

    assert!(
        items.is_err(),
        "a hung GetItems must be an error, not a wait"
    );
    assert!(
        matches!(outcome, WriteOutcome::Unreachable { .. }),
        "a hung SetValue is an unreachable write: {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "both calls together took {elapsed:?}; the bound is not being applied"
    );
}

/// A read of a path no application publishes is not an invitation to create
/// a retained topic under the client's chosen name. Nothing is published,
/// and the bridge remembers nothing about the path.
#[tokio::test]
async fn a_read_of_an_unknown_path_publishes_nothing() {
    let mut harness = Harness::new(Mode::ReadOnly);
    harness.start(secs(0)).await;
    harness.transport.clear();

    let effects = harness.bridge.on_request(
        secs(1),
        &format!("R/{DEVICE}/{CLASS}/0/NoSuchItem/at/all"),
        b"",
    );
    harness.run(effects).await;

    assert!(
        harness.transport.topics().is_empty(),
        "an unknown path must not become a retained topic: {:?}",
        harness.transport.topics()
    );
}

//! MQTT and D-Bus I/O around the application-only protocol state machine.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use zbus::{MatchRule, MessageStream};

use crate::bridge::{Bridge, Effects, Write};
use crate::config::{Mode, Timings};
use crate::enrollment::Enrollment;
use crate::source::{
    APPLICATION_CALL_TIMEOUT, Bounded, BusSource, ItemSource, ItemTreeProxy, batch_of,
};
use crate::topic::{self, Application};
use crate::transport::{MqttTransport, Transport};

/// Carry out one event's broker publications and application writes.
pub async fn apply(
    effects: Effects,
    transport: &dyn Transport,
    source: &dyn ItemSource,
) -> anyhow::Result<()> {
    for publication in &effects.publications {
        transport.publish(publication).await?;
    }
    for Write {
        application,
        path,
        value,
    } in &effects.writes
    {
        let outcome = source.set_value(application, path, value.clone()).await;
        if outcome.accepted() {
            tracing::info!(
                application = application.bus_name(),
                path,
                "write request carried through to application SetValue"
            );
        } else {
            tracing::warn!(
                application = application.bus_name(),
                path,
                outcome = ?outcome,
                "application write request was not carried out"
            );
        }
    }
    Ok(())
}

/// Everything the daemon is told at startup.
pub struct Settings {
    pub device_id: String,
    pub applications_dir: PathBuf,
    pub broker_host: String,
    pub broker_port: u16,
    pub client_id: String,
    /// Optional-on-disk JSON credentials, readable only by the bridge account.
    pub credentials_file: PathBuf,
    pub mode: Mode,
    pub session_bus: bool,
    pub timings: Timings,
}

/// Construct the broker connection used by the runtime and integration tests.
pub fn mqtt_options(settings: &Settings) -> anyhow::Result<MqttOptions> {
    let mut options = MqttOptions::new(
        settings.client_id.clone(),
        settings.broker_host.clone(),
        settings.broker_port,
    );
    options.set_keep_alive(Duration::from_secs(30));
    if let Some((username, password)) =
        crate::config::broker_credentials(&settings.credentials_file)?
    {
        options.set_credentials(username, password);
    }
    Ok(options)
}

const REQUEST_CAPACITY: usize = 64;
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long after a failed activation the bus is swept again.
///
/// An application can fail its first `GetItems` and still own its name: one
/// that claims the name before it registers `/`, or one whose `ItemsChanged`
/// stream ended. No `NameOwnerChanged` will follow, so the runtime comes
/// back on its own rather than waiting for a restart.
const ACTIVATION_RETRY: Duration = Duration::from_secs(5);

/// Doubling broker reconnect backoff with a floor and ceiling.
#[derive(Debug, Clone, Copy)]
pub struct ReconnectBackoff {
    next: Duration,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self {
            next: RECONNECT_BACKOFF_MIN,
        }
    }
}

impl ReconnectBackoff {
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(RECONNECT_BACKOFF_MAX);
        delay
    }

    pub fn reset(&mut self) {
        self.next = RECONNECT_BACKOFF_MIN;
    }
}

/// A message on a subscribed topic, handed from the event-loop task to the
/// runtime.
struct Incoming {
    topic: String,
    payload: Vec<u8>,
}

/// Hand one broker message to the runtime without waiting for it.
///
/// The event-loop task is the only thing that drains rumqttc's request
/// channel, and [`apply`] waits on that channel when it is full. If this task
/// waited on the runtime in turn, a burst of requests arriving during a large
/// full publish would leave each side waiting for the other. A request the
/// runtime has no room for is dropped instead: the protocol is QoS 0, and a
/// keepalive asks for everything again. Returns `false` once the runtime is
/// gone.
fn forward(tx: &mpsc::Sender<Incoming>, message: Incoming) -> bool {
    match tx.try_send(message) {
        Ok(()) => true,
        Err(TrySendError::Full(dropped)) => {
            tracing::warn!(
                topic = dropped.topic,
                "runtime is busy; dropping the broker request"
            );
            true
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

enum ApplicationEvent {
    Items {
        bus_name: String,
        generation: u64,
        items: BTreeMap<String, Option<crate::item::Item>>,
    },
    WatcherStopped {
        bus_name: String,
        generation: u64,
        detail: String,
    },
}

struct ActiveApplication {
    owner: String,
    generation: u64,
    watcher: JoinHandle<()>,
}

impl Drop for ActiveApplication {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

fn merge(mut left: Effects, right: Effects) -> Effects {
    left.publications.extend(right.publications);
    left.writes.extend(right.writes);
    left
}

/// Apply one application event. The flag asks the runtime to sweep the bus
/// again: a watcher that stopped while its owner is still there is an
/// application worth re-activating.
fn handle_application_event(
    now: Duration,
    bridge: &mut Bridge,
    active: &mut BTreeMap<String, ActiveApplication>,
    event: ApplicationEvent,
) -> (Effects, bool) {
    match event {
        ApplicationEvent::Items {
            bus_name,
            generation,
            items,
        } => {
            if active
                .get(&bus_name)
                .is_some_and(|current| current.generation == generation)
            {
                (bridge.on_items_changed(now, &bus_name, items), false)
            } else {
                (Effects::default(), false)
            }
        }
        ApplicationEvent::WatcherStopped {
            bus_name,
            generation,
            detail,
        } => {
            if active
                .get(&bus_name)
                .is_some_and(|current| current.generation == generation)
            {
                active.remove(&bus_name);
                tracing::warn!(
                    application = bus_name,
                    error = detail,
                    "application ItemsChanged watcher stopped; withdrawing stale MQTT state"
                );
                (bridge.on_service_vanished(now, &bus_name), true)
            } else {
                (Effects::default(), false)
            }
        }
    }
}

/// Match ownership changes in the mos namespace.
///
/// The signal comes from the bus daemon, not from the named service. The
/// runtime checks the exact enrollment before asking for an owner or creating
/// an Item1 proxy, so an unregistered service such as micad is observed only as
/// a string and is never called or subscribed to.
fn mos_owner_rule() -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg0ns(mica_busname::PREFIX.trim_end_matches('.'))?
        .build())
}

async fn watch_application(
    connection: zbus::Connection,
    application: Application,
    generation: u64,
    ready: oneshot::Sender<Result<(), String>>,
    tx: mpsc::Sender<ApplicationEvent>,
) -> anyhow::Result<()> {
    let proxy = match ItemTreeProxy::builder(&connection)
        .destination(application.bus_name().to_string())?
        .build()
        .await
    {
        Ok(proxy) => proxy,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return Err(err.into());
        }
    };
    let mut changes = match proxy.receive_items_changed().await {
        Ok(changes) => changes,
        Err(err) => {
            let _ = ready.send(Err(err.to_string()));
            return Err(err.into());
        }
    };
    let _ = ready.send(Ok(()));
    while let Some(signal) = changes.next().await {
        let items = batch_of(signal.args()?.items);
        if tx
            .send(ApplicationEvent::Items {
                bus_name: application.bus_name().to_string(),
                generation,
                items,
            })
            .await
            .is_err()
        {
            return Ok(());
        }
    }
    anyhow::bail!("ItemsChanged stream ended")
}

/// The mirrors the runtime holds, and what it needs to open one.
struct Activation<'a> {
    connection: &'a zbus::Connection,
    source: &'a dyn ItemSource,
    changes_tx: &'a mpsc::Sender<ApplicationEvent>,
    active: BTreeMap<String, ActiveApplication>,
    next_generation: u64,
}

impl Activation<'_> {
    async fn activate(
        &mut self,
        bridge: &mut Bridge,
        now: Duration,
        application: Application,
        owner: String,
    ) -> Effects {
        let bus_name = application.bus_name().to_string();
        if self
            .active
            .get(&bus_name)
            .is_some_and(|current| current.owner == owner)
        {
            return Effects::default();
        }

        let effects = if self.active.remove(&bus_name).is_some() {
            bridge.on_service_vanished(now, &bus_name)
        } else {
            Effects::default()
        };

        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        let watcher_application = application.clone();
        let watcher_connection = self.connection.clone();
        let watcher_tx = self.changes_tx.clone();
        let stopped_tx = self.changes_tx.clone();
        let stopped_application = application.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let watcher = tokio::spawn(async move {
            if let Err(err) = watch_application(
                watcher_connection,
                watcher_application.clone(),
                generation,
                ready_tx,
                watcher_tx,
            )
            .await
            {
                let _ = stopped_tx
                    .send(ApplicationEvent::WatcherStopped {
                        bus_name: stopped_application.bus_name().to_string(),
                        generation,
                        detail: err.to_string(),
                    })
                    .await;
            }
        });

        match ready_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(detail)) => {
                watcher.abort();
                tracing::warn!(
                    application = application.bus_name(),
                    error = detail,
                    "application is present but its ItemsChanged watcher cannot be established"
                );
                return effects;
            }
            Err(_) => {
                watcher.abort();
                tracing::warn!(
                    application = application.bus_name(),
                    "application ItemsChanged watcher stopped before it became ready"
                );
                return effects;
            }
        }

        match self.source.get_items(&application).await {
            Ok(items) => {
                self.active.insert(
                    bus_name,
                    ActiveApplication {
                        owner,
                        generation,
                        watcher,
                    },
                );
                merge(effects, bridge.upsert_service(now, application, items))
            }
            Err(err) => {
                watcher.abort();
                tracing::warn!(
                    application = application.bus_name(),
                    error = %err,
                    "application is present but its Item1 tree is not readable; check its exact-name D-Bus policy grant"
                );
                effects
            }
        }
    }

    /// Activate every enrolled name that is on the bus and not yet mirrored.
    /// Returns whether any of them could not be activated, so the caller can
    /// come back after [`ACTIVATION_RETRY`].
    async fn sweep(
        &mut self,
        bus: &zbus::fdo::DBusProxy<'_>,
        enrollment: &Enrollment,
        bridge: &mut Bridge,
        transport: &dyn Transport,
        start: Instant,
    ) -> anyhow::Result<bool> {
        let mut failed = false;
        for name in bus.list_names().await? {
            let Some(application) = enrollment.application(name.as_str()) else {
                continue;
            };
            let owner = match bus.get_name_owner(name.clone().into()).await {
                Ok(owner) => owner.to_string(),
                Err(_) => continue,
            };
            let effects = self
                .activate(bridge, start.elapsed(), application, owner)
                .await;
            apply(effects, transport, self.source).await?;
            failed |= !self.active.contains_key(name.as_str());
        }
        Ok(failed)
    }
}

/// Connect to enrolled application services and the broker, then run until
/// the process is asked to stop.
pub async fn run(settings: Settings) -> anyhow::Result<()> {
    if !topic::valid_topic_segment(&settings.device_id) {
        anyhow::bail!("configured device id cannot form an MQTT topic segment");
    }
    let enrollment = Enrollment::load(&settings.applications_dir).await?;
    let connection = if settings.session_bus {
        zbus::Connection::session().await?
    } else {
        zbus::Connection::system().await?
    };
    let source = Bounded::new(BusSource::new(connection.clone()), APPLICATION_CALL_TIMEOUT);

    let owner_rule = mos_owner_rule()?;
    let mut owners = Box::pin(MessageStream::for_match_rule(owner_rule, &connection, None).await?);
    let bus = zbus::fdo::DBusProxy::new(&connection).await?;

    let options = mqtt_options(&settings)?;
    let (client, mut eventloop) = AsyncClient::new(options, REQUEST_CAPACITY);
    let transport = MqttTransport::new(client);

    // Two channels out of the event-loop task, neither of which it waits on:
    // requests are dropped when the runtime is busy (see `forward`), and a
    // connection is a counter the runtime catches up with when it can, so a
    // reconnect is never lost behind a queue of requests.
    let (incoming_tx, mut incoming) = mpsc::channel(REQUEST_CAPACITY);
    let (connected_tx, mut connected) = watch::channel(0u64);
    tokio::spawn(async move {
        let mut backoff = ReconnectBackoff::default();
        loop {
            let event = match eventloop.poll().await {
                Ok(event) => {
                    backoff.reset();
                    event
                }
                Err(err) => {
                    let delay = backoff.next_delay();
                    tracing::warn!(
                        error = %err,
                        retry_in_s = delay.as_secs(),
                        "broker connection lost; retrying after backoff"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };
            match event {
                Event::Incoming(Packet::Publish(publish)) => {
                    let message = Incoming {
                        topic: publish.topic,
                        payload: publish.payload.to_vec(),
                    };
                    if !forward(&incoming_tx, message) {
                        return;
                    }
                }
                Event::Incoming(Packet::ConnAck(_)) => {
                    connected_tx.send_modify(|count| *count += 1);
                }
                _ => {}
            }
        }
    });

    let (changes_tx, mut changes) = mpsc::channel(REQUEST_CAPACITY);
    let mut activation = Activation {
        connection: &connection,
        source: &source,
        changes_tx: &changes_tx,
        active: BTreeMap::new(),
        next_generation: 0,
    };
    let mut bridge = Bridge::new(settings.device_id, settings.mode, settings.timings);
    let mut subscribed = Vec::new();
    let start = Instant::now();
    let mut resweep_at: Option<Instant> = None;

    // The ownership match is installed before this sweep, so a service that
    // appears during it is either listed or queued as a signal (possibly
    // both; owner equality makes the duplicate harmless).
    if activation
        .sweep(&bus, &enrollment, &mut bridge, &transport, start)
        .await?
    {
        resweep_at = Some(Instant::now() + ACTIVATION_RETRY);
    }
    resubscribe(&bridge, &transport, &mut subscribed).await?;

    loop {
        let now = start.elapsed();
        let wake = bridge.next_wake(now).map(|wake| start + wake);
        let effects = tokio::select! {
            event = changes.recv() => {
                let Some(event) = event else { return Ok(()) };
                let (effects, resweep) =
                    handle_application_event(start.elapsed(), &mut bridge, &mut activation.active, event);
                if resweep {
                    resweep_at.get_or_insert(Instant::now() + ACTIVATION_RETRY);
                }
                effects
            }
            owner = owners.next() => {
                let Some(owner) = owner else { return Ok(()) };
                let owner = owner?;
                let (name, _old_owner, new_owner) =
                    owner.body().deserialize::<(String, String, String)>()?;
                let Some(application) = enrollment.application(&name) else {
                    continue;
                };
                if new_owner.is_empty() {
                    if activation.active.remove(&name).is_some() {
                        tracing::info!(application = name, "application left the bus; clearing retained MQTT state");
                        bridge.on_service_vanished(start.elapsed(), &name)
                    } else {
                        Effects::default()
                    }
                } else {
                    let effects = activation
                        .activate(&mut bridge, start.elapsed(), application, new_owner)
                        .await;
                    if !activation.active.contains_key(&name) {
                        resweep_at.get_or_insert(Instant::now() + ACTIVATION_RETRY);
                    }
                    effects
                }
            }
            () = sleep_until(resweep_at) => {
                resweep_at = None;
                if activation
                    .sweep(&bus, &enrollment, &mut bridge, &transport, start)
                    .await?
                {
                    resweep_at = Some(Instant::now() + ACTIVATION_RETRY);
                }
                Effects::default()
            }
            message = incoming.recv() => {
                let Some(Incoming { topic, payload }) = message else { return Ok(()) };
                bridge.on_request(start.elapsed(), &topic, &payload)
            }
            changed = connected.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                subscribed.clear();
                Effects::default()
            }
            () = sleep_until(wake) => bridge.on_tick(start.elapsed()),
            _ = tokio::signal::ctrl_c() => return Ok(()),
        };
        apply(effects, &transport, &source).await?;
        resubscribe(&bridge, &transport, &mut subscribed).await?;
    }
}

async fn resubscribe(
    bridge: &Bridge,
    transport: &dyn Transport,
    subscribed: &mut Vec<String>,
) -> anyhow::Result<()> {
    let wanted = bridge.subscriptions();
    if wanted == *subscribed {
        return Ok(());
    }
    for filter in subscribed.iter().filter(|filter| !wanted.contains(filter)) {
        transport.unsubscribe(filter).await?;
    }
    for filter in wanted.iter().filter(|filter| !subscribed.contains(filter)) {
        tracing::info!(filter, "subscribing");
        transport.subscribe(filter).await?;
    }
    *subscribed = wanted;
    Ok(())
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use serde_json::json;

    use super::{
        ActiveApplication, ApplicationEvent, Incoming, forward, handle_application_event,
        mos_owner_rule,
    };
    use crate::bridge::Bridge;
    use crate::config::{Mode, Timings};
    use crate::item::Item;

    #[test]
    fn the_bus_filters_discovery_to_the_mos_namespace() {
        let rule = mos_owner_rule().expect("valid mos ownership rule");
        let rendered = rule.to_string();
        assert!(rendered.contains("arg0namespace='com.mica'"), "{rendered}");
        assert!(rendered.contains("member='NameOwnerChanged'"), "{rendered}");
    }

    #[tokio::test]
    async fn a_stopped_current_watcher_withdraws_stale_application_state() {
        let enrollment = crate::enrollment::Enrollment::from_names(["com.mica.sensor.example"])
            .expect("valid enrollment");
        let application = enrollment
            .application("com.mica.sensor.example")
            .expect("enrolled application");
        let mut bridge = Bridge::new("abc123", Mode::Full, Timings::default());
        bridge.upsert_service(
            Duration::ZERO,
            application,
            BTreeMap::from([
                ("/DeviceInstance".to_string(), Item::new(json!(0))),
                ("/Temperature".to_string(), Item::new(json!(21))),
            ]),
        );
        bridge.on_keepalive(Duration::ZERO);

        let mut active = BTreeMap::from([(
            "com.mica.sensor.example".to_string(),
            ActiveApplication {
                owner: ":1.42".to_string(),
                generation: 7,
                watcher: tokio::spawn(std::future::pending()),
            },
        )]);
        let (effects, resweep) = handle_application_event(
            Duration::from_secs(1),
            &mut bridge,
            &mut active,
            ApplicationEvent::WatcherStopped {
                bus_name: "com.mica.sensor.example".to_string(),
                generation: 7,
                detail: "signal stream ended".to_string(),
            },
        );

        assert!(active.is_empty(), "the stale mirror remained active");
        assert!(
            resweep,
            "the owner is still on the bus, so the runtime must sweep again rather than wait for a restart"
        );
        assert!(
            effects.publications.iter().any(|publication| {
                publication.topic == "N/abc123/sensor/0/Temperature"
                    && publication.payload.is_empty()
                    && publication.retain
            }),
            "the stale retained value was not withdrawn"
        );
    }

    #[tokio::test]
    async fn a_stale_watcher_report_asks_for_no_resweep() {
        let mut bridge = Bridge::new("abc123", Mode::Full, Timings::default());
        let mut active = BTreeMap::from([(
            "com.mica.sensor.example".to_string(),
            ActiveApplication {
                owner: ":1.42".to_string(),
                generation: 8,
                watcher: tokio::spawn(std::future::pending()),
            },
        )]);
        let (effects, resweep) = handle_application_event(
            Duration::from_secs(1),
            &mut bridge,
            &mut active,
            ApplicationEvent::WatcherStopped {
                bus_name: "com.mica.sensor.example".to_string(),
                generation: 7,
                detail: "signal stream ended".to_string(),
            },
        );
        assert_eq!(effects, crate::bridge::Effects::default());
        assert!(
            !resweep,
            "a report from a superseded watcher changes nothing"
        );
        assert_eq!(active.len(), 1);
    }

    /// The event-loop task must never wait on the runtime. A request that
    /// arrives while the inbound channel is full is dropped, not queued
    /// behind a publish that is itself waiting on the event loop.
    #[test]
    fn an_inbound_request_is_dropped_rather_than_awaited_when_the_runtime_is_busy() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let message = || Incoming {
            topic: "R/abc123/keepalive".to_string(),
            payload: Vec::new(),
        };
        assert!(forward(&tx, message()));
        assert!(forward(&tx, message()));
        assert!(rx.try_recv().is_ok(), "the first request is queued");
        assert!(
            rx.try_recv().is_err(),
            "the second request was dropped, not awaited"
        );
    }
}
